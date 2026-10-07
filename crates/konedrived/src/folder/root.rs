use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::{self, read_state, State, StateError, XATTR_DRIVE, XATTR_ITEM_ID, XATTR_PIN, XATTR_ROOT, XATTR_STATE};
use konedrive_fs::probe::{probe_dir, ProbeError};
use konedrive_fs::proc_path;
use nix::errno::Errno;
use nix::fcntl::{openat2, OFlag, OpenHow};
use nix::sys::stat::Mode;
use xattr::FileExt;

use crate::helper::{HelperError, HelperLink};

use super::disk::beneath;
use super::walk::reserved;

/// Why a file or a folder of the root was not opened ([`SyncRoot::open_inside`],
/// [`SyncRoot::open_item`]).
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The path leads out of the root, through a link, or to something that is neither a
    /// file nor a folder; or the root no longer carries this root's id.
    #[error("not a plain file inside this sync root")]
    OutsideRoot,
    /// Inside the root and not konedrive's: a file without a state, or one of the daemon's
    /// own `.konedrive-*` names.
    #[error("not a OneDrive file")]
    NotManaged,
    #[error("{0}")]
    Io(String),
}

fn io_error(e: impl std::fmt::Display) -> OpenError {
    OpenError::Io(e.to_string())
}

#[derive(Debug, Clone)]
pub struct SyncRoot {
    pub path: PathBuf,
    pub root_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    #[error("not a directory")]
    NotADirectory,
    #[error("the folder must be empty")]
    NotEmpty,
    /// The folder cannot be a sync root: its filesystem lacks something a placeholder
    /// needs, or it is a symbolic link. The sentence names the folder and what is missing.
    #[error("{0}")]
    Unsupported(String),
    /// The folder could not be looked at or labelled: an errno, with what was being done.
    #[error("{0}")]
    Io(String),
    /// The helper did not register it. A [`HelperError::Timeout`] is the one answer after
    /// which the helper may hold the registration all the same.
    #[error("{0}")]
    Helper(#[from] HelperError),
}

/// Opens a candidate sync root, and nothing else: `O_DIRECTORY` so a file
/// can never be mistaken for one, `O_NOFOLLOW` so the last component cannot
/// be a symlink pointing somewhere else entirely.
fn open_root_dir(path: &Path) -> Result<File, RegisterError> {
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    match nix::fcntl::open(path, flags, Mode::empty()) {
        Ok(fd) => Ok(File::from(fd)),
        Err(Errno::ENOTDIR | Errno::ENOENT | Errno::ELOOP) => Err(not_a_directory(path)),
        Err(e) => Err(RegisterError::Io(format!("{}: {e}", path.display()))),
    }
}

/// Why the open could not produce a directory, said in the user's terms.
///
/// The errno alone cannot: `O_DIRECTORY | O_NOFOLLOW` against a symlink —
/// even one pointing at a perfectly good directory — reports **`ENOTDIR`** on
/// Linux rather than the `ELOOP` `O_NOFOLLOW` documents on its own, so "not a
/// directory" would be the message for a folder the user can see is there.
/// One `lstat` separates them.
fn not_a_directory(path: &Path) -> RegisterError {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => RegisterError::Unsupported(format!(
            "{}: a sync root must be a real directory, not a symbolic link",
            path.display()
        )),
        _ => RegisterError::NotADirectory,
    }
}

/// Everything about a candidate folder that can be decided locally.
///
/// The write probe (`probe_dir`, `konedrive_fs::probe`) runs *here* and
/// nowhere else. The helper runs under `ProtectSystem=strict` and
/// `ProtectHome=read-only` and without `CAP_DAC_OVERRIDE`, so a write of its
/// own into a perfectly good directory in the user's home is normally
/// refused (`EROFS`, `EACCES`): it checks the filesystem's type with `fstatfs`
/// (`check_filesystem_type` in `konedrive-helper/src/registration.rs`) and
/// probes nothing. The daemon runs unprivileged,
/// in the user's own home, with no such sandbox, so this is the one place in
/// the system where a write probe's result means something.
pub fn check_root_candidate(path: &Path) -> Result<(), RegisterError> {
    let dir = open_root_dir(path)?;
    check_root_dir(&dir, path)
}

/// The same checks, on a directory that is already open. The probe's own
/// error text names `/proc/self/fd/<n>`, which would be meaningless to the
/// person who typed a folder name, so the message is rebuilt around the path
/// they actually gave.
///
/// # Empty is required only for *first* registration
///
/// A folder that already carries a valid `user.konedrive.root` is not a
/// candidate being registered for the first time — it is a root the daemon
/// (this run or a previous one) already claimed, and re-registering it is
/// exactly what startup recovery needs to do before it can safely touch
/// anything inside it. Such a folder is *expected* to be full: placeholders,
/// hydrated files, subdirectories, all of this daemon's own making. Refusing
/// it as `NotEmpty` — the same refusal a stranger's populated folder gets —
/// would make every restart unregister every root the moment anything had
/// been written into it, which is always, immediately after the first
/// registration. The empty check below therefore only runs when the folder
/// has no root id of its own yet; a folder that already names itself as a
/// root skips it.
///
/// # The probe runs first
///
/// `read_root_id` is a `getxattr` in the `user.*` namespace, which is the
/// very thing [`probe_dir`] exists to establish is available. Asking for the
/// root id first means that on a filesystem without `user.*` xattrs
/// registration fails with `cannot access user.konedrive.root: …` — an
/// errno, naming an attribute the user never heard of, about a folder the
/// message does not name — instead of the probe's purpose-built "the
/// filesystem does not support user xattrs" against the folder they typed.
/// The probe is also the more fundamental refusal: a folder that cannot hold
/// a placeholder at all cannot be a sync root whether it is empty or not.
///
/// # A folder that already carries one of our root ids is not probed again
///
/// It was probed when it was first registered, and a folder that shows
/// OneDrive is locked read-only since, the folder itself included, so
/// the probe's write is refused there: no such folder came back after a
/// restart, in either mode — "cannot bring up the sync folder: Permission
/// denied" — and none could be switched to interception once the helper
/// arrived. The id is still only *looked
/// for* first, and a folder where looking fails is probed, whose answer is
/// what is reported.
fn check_root_dir(dir: &File, path: &Path) -> Result<(), RegisterError> {
    if matches!(read_root_id(dir), Ok(Some(_))) {
        return Ok(());
    }
    let through_fd = proc_path(dir);
    probe_dir(&through_fd).map_err(|e| match e {
        ProbeError::Missing { feature, .. } => RegisterError::Unsupported(format!(
            "{}: the filesystem does not support {feature}",
            path.display()
        )),
        ProbeError::Unusable { why, .. } => {
            RegisterError::Unsupported(format!("{}: {why}", path.display()))
        }
    })?;
    if read_root_id(dir)?.is_none() {
        let mut entries = std::fs::read_dir(&through_fd)
            .map_err(|e| RegisterError::Io(format!("{}: {e}", path.display())))?;
        if entries.next().is_some() {
            return Err(RegisterError::NotEmpty);
        }
    }
    Ok(())
}

/// Binds an empty folder to the account: probe, stamp it, tell the helper.
///
/// # Why the root id is read before it is minted
///
/// `user.konedrive.root` is written before the helper is told about it, and
/// it is never overwritten. That ordering is not free to change — a crash,
/// or a `HelperError::Timeout`, between the helper saving the root and this
/// function returning leaves a registration the daemon cannot see — so the
/// xattr is read *first* and reused whenever it is there. The helper treats
/// a registration with the same uid and the same id as idempotent,
/// so retrying with the id already on the folder converges.
///
/// Minting a fresh id on every attempt, as this used to, does the opposite:
/// the retry offers root B for a directory the helper already holds as root
/// A, which is `nesting_conflict` → `SameDirectory` → **`EINVAL`**, forever,
/// and `unregister_root(B)` can never remove A either. The folder becomes
/// permanently unregisterable. A reused id costs nothing when the first
/// attempt failed for good (an unwritable folder, a refused filesystem):
/// the id is a name, not a claim, and the next successful registration uses
/// it. We do not remove it on failure precisely because a timeout cannot
/// tell us whether the helper saved it.
///
/// `link.register_root` triggers a full `openat2` walk of the whole tree
/// inside the helper (bounded by `HelperLink`'s 120 s `register_root`
/// timeout, not the ordinary 30 s call bound). A root that walk could not
/// fully cover is logged by the helper as degraded
/// (`konedrive-helper/src/registration.rs` `record_walk`) and kept track of
/// nowhere: `RegisterRoot`'s ack carries only an errno, so a degraded root
/// still acks success here (issue #219). Nothing in this crate
/// today has anywhere to put that signal — the natural home is a later
/// helper→daemon query (or an addition to `RootState`/`LastError` on
/// `org.konedrive.Folder`), once one exists.
pub async fn register_root(link: &HelperLink, path: &Path) -> Result<SyncRoot, RegisterError> {
    let (dir, root) = prepare(path).await?;
    link.register_root(&dir, &root.root_id).await?;
    Ok(root)
}

/// Everything [`register_root`] does before the helper is told: the folder
/// checks, and the root id read or minted. Returns the open
/// directory the helper is to be handed, so that the descriptor the checks
/// ran on is the descriptor it registers.
///
/// Separate so that `SyncService` can write a new root down in
/// `config.toml` between the two halves — before the helper holds anything
/// the daemon's next start would not know about.
pub(crate) async fn prepare(path: &Path) -> Result<(File, SyncRoot), RegisterError> {
    // Opening, listing, probing and stamping a directory are
    // all blocking syscalls, and `helper/mod.rs`'s module doc treats a
    // blocking call left on a tokio worker as a first-class defect.
    let requested = path.to_path_buf();
    tokio::task::spawn_blocking(move || prepare_root(&requested))
        .await
        .map_err(|e| RegisterError::Io(format!("the registration task failed: {e}")))?
}

/// The root id `path` carries, if it is a directory carrying one of ours —
/// read, never minted, and never followed through a symlink. For a root
/// restored from a `config.toml` that did not record its id; `None` for
/// anything that cannot be read.
pub(crate) async fn recorded_root_id(path: &Path) -> Option<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let dir = open_dir_nofollow(&path).ok()?;
        read_root_id(&dir).ok().flatten()
    })
    .await
    .ok()
    .flatten()
}

/// Whose files a folder holds, as far as the drive it remembers goes
/// (`user.konedrive.drive`, `docs/design/accounts.md` §6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DriveOf {
    /// It carries no drive, or the asking account's: that account may register it.
    Free,
    /// It carries another drive, and holds that account's files: it may not be registered.
    Foreign(String),
    /// It carries another drive and is empty: there is nothing of that account's to adopt.
    /// It may be registered once the stale drive is taken off ([`forget_drive`]).
    Stale(String),
}

fn open_dir_nofollow(path: &Path) -> nix::Result<File> {
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    nix::fcntl::open(path, flags, Mode::empty()).map(File::from)
}

/// What the folder at `path` says about its drive, for an account whose drive is `mine`.
/// Read through a descriptor, never followed through a symlink, and nothing is changed. A
/// folder that cannot be opened is [`DriveOf::Free`]: the registration's own checks refuse
/// it.
pub(crate) async fn drive_of(path: &Path, mine: Option<String>) -> DriveOf {
    let path = path.to_path_buf();
    let read = tokio::task::spawn_blocking(move || {
        let Ok(dir) = open_dir_nofollow(&path) else { return DriveOf::Free };
        let theirs = dir.get_xattr(XATTR_DRIVE).ok().flatten().and_then(|raw| String::from_utf8(raw).ok());
        match theirs {
            None => DriveOf::Free,
            Some(theirs) if theirs.is_empty() || Some(&theirs) == mine.as_ref() => DriveOf::Free,
            Some(theirs) => {
                let empty = std::fs::read_dir(proc_path(&dir)).is_ok_and(|mut entries| entries.next().is_none());
                if empty { DriveOf::Stale(theirs) } else { DriveOf::Foreign(theirs) }
            }
        }
    });
    // A task that did not run says nothing for the folder: refused.
    read.await.unwrap_or_else(|e| DriveOf::Foreign(format!("unknown ({e})")))
}

/// Takes the drive off the folder at `path`, which [`drive_of`] found [`DriveOf::Stale`]:
/// only while it is still empty. A failure is logged and the folder is registered all the
/// same, as it was: the drive it keeps is then replaced by nothing, and `mark_drive` leaves
/// it.
pub(crate) async fn forget_drive(path: &Path) {
    let path = path.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || {
        let Ok(dir) = open_dir_nofollow(&path) else { return };
        if !std::fs::read_dir(proc_path(&dir)).is_ok_and(|mut entries| entries.next().is_none()) {
            return;
        }
        let removed = super::disk::Modes::of(&dir).and_then(|modes| {
            let _held = modes.hold();
            konedrive_fs::placeholder::with_owner_write(&dir, || dir.remove_xattr(XATTR_DRIVE))
        });
        match removed {
            Ok(()) => tracing::info!("{} is empty: the drive it carried is taken off", path.display()),
            Err(e) => tracing::warn!("cannot take the stale drive off the empty {}: {e}", path.display()),
        }
    })
    .await;
}

/// Writes `drive` on the registered root as the drive it shows
/// (`user.konedrive.drive`, §6.3), when it carries none yet — through a
/// window in the read-only lock, like every attribute the daemon writes in a
/// locked folder. Whether it was written. Blocking.
///
/// A folder remembers its account's drive so that another account cannot adopt
/// it once it is forgotten: a registration of a folder that carries another
/// drive is refused.
pub(crate) fn mark_drive(root: &SyncRoot, drive: &str) -> io::Result<bool> {
    let dir = root
        .open_registered()?
        .ok_or_else(|| io::Error::other(format!("{} no longer carries its root id", root.path.display())))?;
    if dir.get_xattr(XATTR_DRIVE)?.is_some() {
        return Ok(false);
    }
    let modes = super::disk::Modes::of(&dir)?;
    let _held = modes.hold();
    konedrive_fs::placeholder::with_owner_write(&dir, || dir.set_xattr(XATTR_DRIVE, drive.as_bytes()))?;
    Ok(true)
}

/// [`register_root`] with nobody to intercept anything: the
/// same local checks and the same root id, but no helper is told, so no
/// directory under this root is ever marked and no open inside it is ever
/// suspended.
///
/// This is the deliberate, separately-named opt-in behind
/// `org.konedrive.Folder.RegisterWithoutInterception`, never a fallback
/// that a failed helper connection can slide into: `RegisterRoot` itself
/// still refuses outright without a helper, because a placeholder nobody
/// intercepts reads as zeros, which is the one outcome this project exists
/// to prevent. What makes the mode safe to offer at all is that it is
/// *visible* — `RootState` reports `no-interception` and `LastError` says
/// plainly that files here read as zeros until they are hydrated. A folder
/// registered this way while no helper was connected is switched to
/// interception by `SyncService` once one connects.
pub async fn register_root_unprotected(path: &Path) -> Result<SyncRoot, RegisterError> {
    let (_dir, root) = prepare(path).await?;
    Ok(root)
}

/// The local half of [`register_root`]: everything up to, but not including,
/// telling the helper. Returns the open directory the helper is handed, so
/// that the descriptor the checks ran on is the descriptor it registers.
fn prepare_root(path: &Path) -> Result<(File, SyncRoot), RegisterError> {
    let dir = open_root_dir(path)?;
    check_root_dir(&dir, path)?;
    let resolved = resolved_path(&dir, path)?;
    let root_id = root_id_of(&dir)?;
    Ok((dir, SyncRoot { path: resolved, root_id }))
}

/// Where the open directory actually is, read back from the kernel rather
/// than taken from the caller's string, and then proved to still lead to the
/// same inode. The resolved path is what every later `dehydrate` measures
/// "inside this root" against, so it must be free of symlinks and `..`.
fn resolved_path(dir: &File, path: &Path) -> Result<PathBuf, RegisterError> {
    let failed = |e: io::Error| RegisterError::Io(format!("{}: {e}", path.display()));
    let resolved = std::fs::read_link(proc_path(dir)).map_err(failed)?;
    let here = dir.metadata().map_err(failed)?;
    let there = std::fs::metadata(&resolved).map_err(failed)?;
    if (here.dev(), here.ino()) != (there.dev(), there.ino()) {
        return Err(RegisterError::Io(format!(
            "{} moved while it was being registered",
            path.display()
        )));
    }
    Ok(resolved)
}

/// The folder's own root id, minted only if it has none. An
/// empty value is treated as none: it names no registration the helper could
/// be holding, so there is nothing to preserve.
fn root_id_of(dir: &File) -> Result<String, RegisterError> {
    if let Some(id) = read_root_id(dir)? {
        return Ok(id);
    }
    let root_id = uuid_v4();
    dir.set_xattr(XATTR_ROOT, root_id.as_bytes())
        .map_err(|e| RegisterError::Io(format!("cannot write {XATTR_ROOT}: {e}")))?;
    Ok(root_id)
}

/// The folder's own root id, if it already carries a valid one — `None`
/// otherwise. Shared by [`check_root_dir`] (only a folder with
/// *no* id yet must be empty) and [`root_id_of`] (an existing id
/// is reused, never overwritten).
///
/// # A value that is not an id of ours is not an id
///
/// Honouring any non-empty string here is what makes
/// `setfattr -n user.konedrive.root -v x ~/Documents` enough to register a
/// folder full of somebody's existing data: the empty check is skipped
/// for anything that "carries a root id", and nothing ever removes the
/// xattr again, so one `setfattr` disarms that check for that folder
/// permanently. It matters because `PopulateFromDirectory` skips names that already
/// exist — those files never get an item id, never get a placeholder, and
/// yet live inside a tree the helper now marks and this module now walks and
/// punches.
///
/// So the shape is checked before the value is believed: the 36-character
/// `8-4-4-4-12` hex form with version nibble `4` that [`uuid_v4`] mints and
/// `uuid_v4_mints_a_fresh_identifier_every_time` spells out. Anything else —
/// absent, empty, a word, a truncated id — reads as *no* id: the folder must
/// then be empty to be registered, and [`root_id_of`] mints a real one over
/// the top. Overwriting is safe precisely because the value is not one we
/// could ever have minted, so no helper registration can be named by it
/// ("never overwrite" protects ids we *did* mint).
fn read_root_id(dir: &File) -> Result<Option<String>, RegisterError> {
    let read = dir.get_xattr(XATTR_ROOT).map_err(|e| RegisterError::Io(format!("cannot read {XATTR_ROOT}: {e}")))?;
    let Some(raw) = read else {
        return Ok(None);
    };
    // Not text: no id of ours, like any other value we could not have minted.
    let Ok(id) = String::from_utf8(raw) else {
        return Ok(None);
    };
    Ok(looks_like_a_root_id(&id).then_some(id))
}

/// The canonical form of what [`uuid_v4`] mints — 36 characters,
/// `8-4-4-4-12`, lowercase-or-uppercase hex throughout, version nibble `4`.
/// The helper registers a root under no other form, so the rule is the
/// protocol's ([`konedrive_proto::is_root_id`]).
pub(crate) fn looks_like_a_root_id(id: &str) -> bool {
    konedrive_proto::is_root_id(id)
}

/// 16 random bytes in the canonical form; no dependency for one identifier.
pub(crate) fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("the OS random number generator failed");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// How [`SyncRoot::item`] reaches an item, and how its marks are then read from the
/// descriptor it gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reach {
    /// Opened read-only and `O_NONBLOCK`: the marks are read, and written, through the
    /// descriptor. What `Pin`, `Unpin`, `FreeUp` and `WebUrl` do.
    Open,
    /// `O_PATH`: the place in the tree and nothing more. The file is not opened, so no
    /// event is raised and nothing is downloaded, whatever the helper is doing
    /// (`docs/kernel-behavior-7.2/open-by-handle.md`); the marks are read by the
    /// descriptor's name in `/proc`. What `Files.Menu` does, which only asks.
    Look,
}

impl Reach {
    fn flags(self) -> OFlag {
        match self {
            Reach::Open => OFlag::O_RDONLY | OFlag::O_NONBLOCK,
            Reach::Look => OFlag::O_PATH,
        }
    }

    /// The attribute `name` of an item reached by `Look`: an `O_PATH` descriptor reads no
    /// attribute itself, and its name in `/proc` is the object, not a path walked again.
    fn looked(item: &File, name: &str) -> io::Result<Option<String>> {
        Ok(xattr::get_deref(proc_path(item), name)?.map(|raw| String::from_utf8_lossy(&raw).into_owned()))
    }

    /// The item's state, as `placeholder::read_state` reads it.
    pub(crate) fn state(self, item: &File) -> Result<Option<State>, StateError> {
        match self {
            Reach::Open => read_state(item),
            Reach::Look => match Self::looked(item, XATTR_STATE)? {
                None => Ok(None),
                Some(value) => value.parse().map(Some).map_err(|()| StateError::Corrupt(value)),
            },
        }
    }

    /// The item's id in OneDrive, as `placeholder::read_item_id` reads it.
    pub(crate) fn item_id(self, item: &File) -> io::Result<Option<String>> {
        match self {
            Reach::Open => placeholder::read_item_id(item),
            Reach::Look => Self::looked(item, XATTR_ITEM_ID),
        }
    }

    /// Whether the item carries a pin of its own, as `placeholder::read_pin` reads it.
    pub(crate) fn pin(self, item: &File) -> io::Result<bool> {
        match self {
            Reach::Open => placeholder::read_pin(item),
            Reach::Look => Ok(Self::looked(item, XATTR_PIN)?.is_some()),
        }
    }
}

impl SyncRoot {
    /// Opens `path` to fill it or free it up: once, `O_RDWR`, and only if it really
    /// is a plain file inside this root. A file the read-only
    /// lock made `0444` refuses `O_RDWR`; it is opened read-only instead and
    /// reopened writable on the same inode, and only when it is one of ours
    /// (see [`open_locked`](Self::open_locked)).
    ///
    /// Three separate things are checked, because the punch that follows is
    /// irreversible:
    ///
    /// - the root directory still carries *this* root's `user.konedrive.root`
    ///   — a registration that has been removed or replaced no longer
    ///   authorises emptying anything inside it;
    /// - the file's path lies under the root's resolved path, with its parent
    ///   directory resolved first so that `..` and symlinked parents cannot
    ///   spell their way out;
    /// - the kernel agrees, which is what `RESOLVE_BENEATH` is for: the open
    ///   is refused outright if resolution would leave the root, and
    ///   `RESOLVE_NO_SYMLINKS` plus `O_NOFOLLOW` refuse it if any component,
    ///   including the last, is a symlink at all.
    ///
    /// The helper's own check cannot stand in for this one: it is scoped to
    /// the device a root lives on, not to the root itself, and it is not
    /// consulted here anyway. It matters as soon as a D-Bus method
    /// puts a path from outside this process on the other end.
    ///
    /// This is the *only* way anything in `sync` may turn a
    /// caller's path into a descriptor it will write through.
    /// `SyncService::hydrate_now` used to open the checked string itself,
    /// with no `O_NOFOLLOW` and no `RESOLVE_BENEATH`, after awaiting an
    /// unbounded lock — measured, a file outside the root overwritten with
    /// hydration content and `Hydrate` reporting success. An ordinary
    /// directory rename inside the root was enough; no attacker was required.
    pub(crate) fn open_inside(&self, path: &Path) -> Result<File, OpenError> {
        let dir = self
            .open_registered()
            .map_err(|e| OpenError::Io(format!("{}: {e}", self.path.display())))?
            .ok_or(OpenError::OutsideRoot)?;

        let relative = self.relative(path)?;
        let how = OpenHow::new()
            .flags(OFlag::O_RDWR | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
            .resolve(beneath());
        match openat2(dir.as_fd(), &relative, how) {
            Ok(fd) => Ok(File::from(fd)),
            Err(Errno::EACCES) => self.open_locked(&dir, &relative, path),
            Err(Errno::EXDEV | Errno::ELOOP | Errno::EISDIR) => Err(OpenError::OutsideRoot),
            Err(e) => Err(OpenError::Io(format!("{}: {e}", path.display()))),
        }
    }

    /// [`open_inside`](Self::open_inside) for a file the read-only lock made
    /// `0444`: opened for reading with the same resolution rules,
    /// and reopened writable — on the same inode — only if it carries a
    /// konedrive state. A file that is not ours comes back read-only and
    /// untouched; every caller refuses such a file before it writes anything.
    ///
    /// `O_NONBLOCK`, because a read-only open of a FIFO waits for a writer,
    /// and a `0444` FIFO is refused `O_RDWR` and so reaches here; the `fstat`
    /// below then refuses it.
    fn open_locked(&self, dir: &File, relative: &Path, shown: &Path) -> Result<File, OpenError> {
        let how = OpenHow::new()
            .flags(OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
            .resolve(beneath());
        let read_only = match openat2(dir.as_fd(), relative, how) {
            Ok(fd) => File::from(fd),
            Err(Errno::EXDEV | Errno::ELOOP | Errno::EISDIR) => return Err(OpenError::OutsideRoot),
            Err(e) => return Err(OpenError::Io(format!("{}: {e}", shown.display()))),
        };
        if !read_only.metadata().map_err(io_error)?.is_file() {
            return Err(OpenError::OutsideRoot);
        }
        match read_state(&read_only) {
            Ok(Some(_)) => {
                let writable = konedrive_fs::placeholder::reopen_writable(&read_only).map_err(io_error)?;
                // Only the writable descriptor may be left: a second one of
                // our own would refuse the write lease a free-up takes.
                drop(read_only);
                Ok(writable)
            }
            _ => Ok(read_only),
        }
    }

    /// Opens a file or a folder inside this root — or the root itself — to
    /// read or write its pin, with its full path as the activity log names
    /// it. Read-only and `O_NONBLOCK`, under the same resolution rules as
    /// [`open_inside`](Self::open_inside): the root must still carry this
    /// root's id, and neither `..`, a symbolic link nor anything else may
    /// lead outside it. A regular file must be one of ours (it carries a
    /// state); a folder need not carry an item id, since a folder filled with
    /// `PopulateFromDirectory` has none. A `.konedrive-*` name anywhere on
    /// the way is `NotManaged`; anything else is refused.
    pub(crate) fn open_item(&self, path: &Path) -> Result<(File, PathBuf), OpenError> {
        self.item(path, Reach::Open)
    }

    /// [`open_item`](Self::open_item), reached as `reach` says: the one set of
    /// rules for what `Pin`, `Unpin`, `FreeUp` and `WebUrl` take, whether the
    /// item is opened for them or only looked at for `Files.Menu`.
    pub(crate) fn item(&self, path: &Path, reach: Reach) -> Result<(File, PathBuf), OpenError> {
        let dir = self
            .open_registered()
            .map_err(|e| OpenError::Io(format!("{}: {e}", self.path.display())))?
            .ok_or(OpenError::OutsideRoot)?;
        // The root itself, named by its own path: its parent resolved, its
        // name carried over, as `relative` does.
        let named = match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
                std::fs::canonicalize(parent).ok().map(|parent| parent.join(name))
            }
            _ => None,
        };
        let root = std::fs::canonicalize(&self.path).map_err(io_error)?;
        if named.as_deref() == Some(root.as_path()) {
            return Ok((dir, self.path.clone()));
        }
        let relative = self.relative(path)?;
        // `.konedrive-*` is the daemon's own — the holding directory, a new
        // folder before its label, a replacement before its swap — and
        // nothing in or under it is anyone's to pin or free up.
        if relative.components().any(|part| reserved(part.as_os_str())) {
            return Err(OpenError::NotManaged);
        }
        let how = OpenHow::new().flags(reach.flags() | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC).resolve(beneath());
        let item = match openat2(dir.as_fd(), &relative, how) {
            Ok(fd) => File::from(fd),
            Err(Errno::EXDEV | Errno::ELOOP) => return Err(OpenError::OutsideRoot),
            Err(e) => return Err(OpenError::Io(format!("{}: {e}", path.display()))),
        };
        let meta = item.metadata().map_err(io_error)?;
        if meta.is_file() {
            if reach.state(&item).map_err(io_error)?.is_none() {
                return Err(OpenError::NotManaged);
            }
        } else if !meta.is_dir() {
            // A symbolic link too, which `Reach::Look` gets a descriptor of.
            return Err(OpenError::OutsideRoot);
        }
        Ok((item, self.path.join(relative)))
    }

    /// This root's directory, opened as a directory and proved to still be
    /// *this* registered root — `Ok(None)` when the folder no longer carries
    /// this root's `user.konedrive.root`.
    ///
    /// Every descriptor-based walk into the root starts here, because a
    /// registration that has been removed or replaced (the user unregistered
    /// the folder, something else claimed it) no longer authorises emptying
    /// anything inside it. `O_DIRECTORY` so a file can never be mistaken for
    /// the root, `O_NOFOLLOW` so its last component cannot be a symlink to
    /// somewhere else entirely.
    pub(crate) fn open_registered(&self) -> io::Result<Option<File>> {
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let dir = nix::fcntl::open(&self.path, flags, Mode::empty()).map(File::from)?;
        match dir.get_xattr(XATTR_ROOT)? {
            Some(id) if id == self.root_id.as_bytes() => Ok(Some(dir)),
            _ => Ok(None),
        }
    }

    /// `path` as a name to resolve from this root's directory descriptor.
    /// The parent is resolved first and the final component is carried over
    /// untouched, so a symlinked file is refused by the open rather than
    /// silently followed here.
    pub(crate) fn relative(&self, path: &Path) -> Result<PathBuf, OpenError> {
        let name = path.file_name().ok_or(OpenError::OutsideRoot)?;
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let parent = std::fs::canonicalize(parent).map_err(io_error)?;
        let root = std::fs::canonicalize(&self.path).map_err(io_error)?;
        let inside = parent.strip_prefix(&root).map_err(|_| OpenError::OutsideRoot)?;
        Ok(inside.join(name))
    }
}

#[cfg(test)]
pub(crate) mod tests;
