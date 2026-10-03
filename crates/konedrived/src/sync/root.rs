//! Root registration, dehydration and startup recovery: the daemon's side of
//! binding an empty local folder to the signed-in drive, of freeing a
//! hydrated file's space again, and of cleaning up after a crash
//! that caught a file mid-operation.
//!
//! # One descriptor, from the first open to the last write
//!
//! Dehydration and recovery are the only things in this project that destroy
//! a file's contents on purpose, so everything they decide and everything
//! they do must be about the same inode. Dehydration opens the file
//! **once**, `O_RDWR`, and every
//! step after that — reading the state, checking the stamp, marking it
//! `dehydrating`, handing the descriptor to the helper for `ClearIgnore`,
//! taking the write lease, punching, restoring the mtime — goes through that
//! one descriptor. `konedrive_fs`'s API is descriptor-based throughout;
//! nothing here needs a path once the file is open.
//!
//! The version this replaces opened the path four times and punched the
//! fourth, having checked the third. A rename landing in that gap — an
//! editor's save-and-replace, `mv`, anything — meant the guard passed on the
//! old inode while `fallocate` emptied the *new* one: measured, 300 KiB of a
//! freshly written file zeroed and stamped `online-only`, three runs out of
//! three, with `dehydrate` returning `Ok(())`. A descriptor cannot be
//! renamed out from under its holder, which is the whole of the fix.
//!
//! [`recover`] walks a whole tree instead of taking one path, so it extends
//! the same rule to directories: every name it looks at is opened with
//! `openat` from a directory descriptor it already holds, and the descriptor
//! it classified is the descriptor it punches. Its own version of the defect
//! above was measured too — a `sub/` swapped for a symlink out of the root
//! while recovery awaited an ack, and a file elsewhere on the filesystem
//! emptied and counted as a success.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_fs::lease::WriteLease;
use konedrive_fs::placeholder::{
    punch_all, punch_from, read_progress, read_stamp, read_state, remove_progress, remove_stamp, stamp_matches,
    write_state, State, StateError, XATTR_DRIVE, XATTR_ROOT,
};
use konedrive_fs::probe::{probe_dir, ProbeError};
use konedrive_fs::MAX_DEPTH;
use nix::errno::Errno;
use nix::fcntl::{openat2, OFlag, OpenHow, ResolveFlag};
use nix::sys::stat::Mode;
use xattr::FileExt;

use super::helper::{Clearance, HelperError, HelperLink, NotCleared};
use super::{InodeKey, InodeLocks};

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
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Helper(String),
}

/// The path of an open descriptor, for the few APIs that still take one.
///
/// Using `/proc/self/fd/<n>` rather than the caller's path string is the same
/// idiom the helper uses in `check_filesystem`: whatever is done through it
/// lands in the exact directory the descriptor was opened on, and cannot be
/// redirected by swapping a component of the path afterwards.
fn proc_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// Opens a candidate sync root, and nothing else: `O_DIRECTORY` so a file
/// can never be mistaken for one, `O_NOFOLLOW` so the last component cannot
/// be a symlink pointing somewhere else entirely.
fn open_root_dir(path: &Path) -> Result<File, RegisterError> {
    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    match nix::fcntl::open(path, flags, Mode::empty()) {
        Ok(fd) => Ok(File::from(fd)),
        Err(Errno::ENOTDIR | Errno::ENOENT | Errno::ELOOP) => Err(not_a_directory(path)),
        Err(e) => Err(RegisterError::Unsupported(format!("{}: {e}", path.display()))),
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
/// The write half of the probe (`probe_dir`, `konedrive_fs::probe`) is
/// authoritative *here*, not on the helper's side. The helper runs under
/// `ProtectHome=read-only`, so its own write probe can be
/// refused (`EROFS`/`EACCES`/`EPERM`) against a perfectly good directory in
/// the user's own home — that is why `check_filesystem` in
/// `konedrive-helper/src/main.rs` treats a refused write probe as "nothing
/// new learned" and falls back to `fstatfs` alone. The daemon runs
/// unprivileged, in the user's own home, with no such sandbox, so this is
/// the one place in the system where the write probe's result actually means
/// something.
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
/// arrived. The helper's own re-registration skips its probe for
/// the same reason. H93 still holds: the id is only *looked
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
            .map_err(|e| RegisterError::Unsupported(format!("{}: {e}", path.display())))?;
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
/// fully cover is tracked by the helper as "degraded"
/// (`konedrive-helper/src/main.rs::record_walk`/`degraded_roots`), but that
/// status is not yet surfaced anywhere: `RegisterRoot`'s ack carries only an
/// errno, so a degraded root still acks success here. Nothing in
/// this crate today has anywhere to put that signal — the natural home is a
/// later helper→daemon query (or an addition to `RootState`/`LastError` on
/// `org.konedrive.Folder`), once one exists.
pub async fn register_root(link: &HelperLink, path: &Path) -> Result<SyncRoot, RegisterError> {
    let (dir, root) = prepare(path).await?;
    link.register_root(&dir, &root.root_id)
        .await
        .map_err(|e| RegisterError::Helper(e.to_string()))?;
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
pub(super) async fn prepare(path: &Path) -> Result<(File, SyncRoot), RegisterError> {
    // Opening, listing, probing and stamping a directory are
    // all blocking syscalls, and `sync/helper.rs`'s module doc treats a
    // blocking call left on a tokio worker as a first-class defect.
    let requested = path.to_path_buf();
    tokio::task::spawn_blocking(move || prepare_root(&requested))
        .await
        .map_err(|e| RegisterError::Unsupported(format!("the registration task failed: {e}")))?
}

/// The root id `path` carries, if it is a directory carrying one of ours —
/// read, never minted, and never followed through a symlink. For a root
/// restored from a `config.toml` that did not record its id; `None` for
/// anything that cannot be read.
pub(super) async fn recorded_root_id(path: &Path) -> Option<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let dir = nix::fcntl::open(&path, flags, Mode::empty()).map(File::from).ok()?;
        read_root_id(&dir).ok().flatten()
    })
    .await
    .ok()
    .flatten()
}

/// Whether an account whose drive is `mine` may register `path` as far as
/// the folder's drive goes (`user.konedrive.drive`, design §8.3): a folder
/// that carries none, or `mine`, may be; one that carries another drive holds
/// that account's files, and may not — unless it is empty, which holds
/// nothing to adopt: its stale drive is taken off, and it may be. Read and
/// written through a descriptor, never followed through a symlink; a folder
/// that cannot be opened is left for the registration's own checks to refuse.
pub(super) async fn drive_allows(path: &Path, mine: Option<String>) -> bool {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
        let Ok(dir) = nix::fcntl::open(&path, flags, Mode::empty()).map(File::from) else { return true };
        let theirs = dir.get_xattr(XATTR_DRIVE).ok().flatten().and_then(|raw| String::from_utf8(raw).ok());
        match theirs {
            None => true,
            Some(theirs) if theirs.is_empty() || Some(&theirs) == mine.as_ref() => true,
            Some(theirs) => {
                let empty = std::fs::read_dir(proc_path(&dir)).is_ok_and(|mut entries| entries.next().is_none());
                if empty {
                    let _modes = super::disk::dir_modes();
                    let removed = konedrive_fs::placeholder::with_owner_write(&dir, || dir.remove_xattr(XATTR_DRIVE));
                    match removed {
                        Ok(()) => tracing::info!("{} is empty: the drive {theirs} it carried is taken off", path.display()),
                        Err(e) => tracing::warn!("cannot take the drive {theirs} off the empty {}: {e}", path.display()),
                    }
                }
                empty
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// Writes `drive` on the registered root as the drive it shows
/// (`user.konedrive.drive`, design §8.3), when it carries none yet — through a
/// window in the read-only lock, like every attribute the daemon writes in a
/// locked folder. Whether it was written. Blocking.
///
/// A folder remembers its account's drive so that another account cannot adopt
/// it once it is forgotten: a registration of a folder that carries another
/// drive is refused.
pub(super) fn mark_drive(root: &SyncRoot, drive: &str) -> io::Result<bool> {
    let dir = root
        .open_registered()?
        .ok_or_else(|| io::Error::other(format!("{} no longer carries its root id", root.path.display())))?;
    if dir.get_xattr(XATTR_DRIVE)?.is_some() {
        return Ok(false);
    }
    let _modes = super::disk::dir_modes();
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
    let unsupported = |e: io::Error| RegisterError::Unsupported(format!("{}: {e}", path.display()));
    let resolved = std::fs::read_link(proc_path(dir)).map_err(unsupported)?;
    let here = dir.metadata().map_err(unsupported)?;
    let there = std::fs::metadata(&resolved).map_err(unsupported)?;
    if (here.dev(), here.ino()) != (there.dev(), there.ino()) {
        return Err(RegisterError::Unsupported(format!(
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
    let unsupported =
        |e: io::Error| RegisterError::Unsupported(format!("cannot access {XATTR_ROOT}: {e}"));
    let root_id = uuid_v4();
    dir.set_xattr(XATTR_ROOT, root_id.as_bytes()).map_err(unsupported)?;
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
/// folder full of somebody's existing data: H78's relaxation skips the empty
/// check for anything that "carries a root id", and nothing ever removes the
/// xattr again, so one `setfattr` disarms that check for that folder
/// permanently. It matters because §4.3's populate skips names that already
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
    let unsupported =
        |e: io::Error| RegisterError::Unsupported(format!("cannot access {XATTR_ROOT}: {e}"));
    let Some(raw) = dir.get_xattr(XATTR_ROOT).map_err(unsupported)? else {
        return Ok(None);
    };
    let id = String::from_utf8(raw)
        .map_err(|_| RegisterError::Unsupported(format!("{XATTR_ROOT} is not valid UTF-8")))?;
    Ok(looks_like_a_root_id(&id).then_some(id))
}

/// The canonical form of what [`uuid_v4`] mints — 36 characters,
/// `8-4-4-4-12`, lowercase-or-uppercase hex throughout, version nibble `4`.
pub(super) fn looks_like_a_root_id(id: &str) -> bool {
    if id.len() != 36 {
        return false;
    }
    let fields: Vec<&str> = id.split('-').collect();
    if fields.iter().map(|f| f.len()).ne([8, 4, 4, 4, 12]) {
        return false;
    }
    if !fields.iter().all(|f| f.chars().all(|c| c.is_ascii_hexdigit())) {
        return false;
    }
    fields[2].starts_with('4')
}

/// 16 random bytes in the canonical form; no dependency for one identifier.
fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("the OS random number generator failed");
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

#[derive(Debug, thiserror::Error)]
pub enum DehydrateError {
    #[error("not a OneDrive file")]
    NotManaged,
    #[error("the file is not downloaded")]
    NotHydrated,
    #[error("the file was modified locally")]
    ModifiedLocally,
    #[error("the file is in use")]
    InUse,
    #[error("not a plain file inside this sync root")]
    OutsideRoot,
    /// A helper is running and this daemon has no link to it, so a mark its
    /// group may hold on the file cannot be cleared. Nothing
    /// was changed; try again once the link is up.
    #[error("the konedrive helper is running but not connected to this daemon")]
    HelperNotConnected,
    #[error("{0}")]
    Io(String),
}

fn io_error(e: impl std::fmt::Display) -> DehydrateError {
    DehydrateError::Io(e.to_string())
}

impl From<NotCleared> for DehydrateError {
    fn from(e: NotCleared) -> Self {
        match e {
            NotCleared::Unlinked => DehydrateError::HelperNotConnected,
            other => io_error(other),
        }
    }
}

impl SyncRoot {
    /// Opens `path` for dehydration: once, `O_RDWR`, and only if it really
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
    /// consulted here anyway. It matters as soon as puts a path from
    /// outside this process on the other end of a D-Bus method.
    ///
    ///: this is the *only* way anything in `sync` may turn a
    /// caller's path into a descriptor it will write through.
    /// `SyncService::hydrate_now` used to open the checked string itself,
    /// with no `O_NOFOLLOW` and no `RESOLVE_BENEATH`, after awaiting an
    /// unbounded lock — measured, a file outside the root overwritten with
    /// hydration content and `Hydrate` reporting success. An ordinary
    /// directory rename inside the root was enough; no attacker was required.
    pub(super) fn open_inside(&self, path: &Path) -> Result<File, DehydrateError> {
        let dir = self
            .open_registered()
            .map_err(|e| DehydrateError::Io(format!("{}: {e}", self.path.display())))?
            .ok_or(DehydrateError::OutsideRoot)?;

        let relative = self.relative(path)?;
        let how = OpenHow::new()
            .flags(OFlag::O_RDWR | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
            .resolve(
                ResolveFlag::RESOLVE_BENEATH
                    | ResolveFlag::RESOLVE_NO_SYMLINKS
                    | ResolveFlag::RESOLVE_NO_MAGICLINKS,
            );
        match openat2(dir.as_fd(), &relative, how) {
            Ok(fd) => Ok(File::from(fd)),
            Err(Errno::EACCES) => self.open_locked(&dir, &relative, path),
            Err(Errno::EXDEV | Errno::ELOOP | Errno::EISDIR) => Err(DehydrateError::OutsideRoot),
            Err(e) => Err(DehydrateError::Io(format!("{}: {e}", path.display()))),
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
    fn open_locked(&self, dir: &File, relative: &Path, shown: &Path) -> Result<File, DehydrateError> {
        let how = OpenHow::new()
            .flags(OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
            .resolve(
                ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS,
            );
        let read_only = match openat2(dir.as_fd(), relative, how) {
            Ok(fd) => File::from(fd),
            Err(Errno::EXDEV | Errno::ELOOP | Errno::EISDIR) => return Err(DehydrateError::OutsideRoot),
            Err(e) => return Err(DehydrateError::Io(format!("{}: {e}", shown.display()))),
        };
        if !read_only.metadata().map_err(io_error)?.is_file() {
            return Err(DehydrateError::OutsideRoot);
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
    pub(super) fn open_item(&self, path: &Path) -> Result<(File, PathBuf), DehydrateError> {
        let dir = self
            .open_registered()
            .map_err(|e| DehydrateError::Io(format!("{}: {e}", self.path.display())))?
            .ok_or(DehydrateError::OutsideRoot)?;
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
        let reserved = crate::drive::item::RESERVED_PREFIX.as_bytes();
        if relative.components().any(|part| part.as_os_str().as_encoded_bytes().starts_with(reserved)) {
            return Err(DehydrateError::NotManaged);
        }
        let how = OpenHow::new()
            .flags(OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
            .resolve(
                ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS,
            );
        let item = match openat2(dir.as_fd(), &relative, how) {
            Ok(fd) => File::from(fd),
            Err(Errno::EXDEV | Errno::ELOOP) => return Err(DehydrateError::OutsideRoot),
            Err(e) => return Err(DehydrateError::Io(format!("{}: {e}", path.display()))),
        };
        let meta = item.metadata().map_err(io_error)?;
        if meta.is_file() {
            if read_state(&item).map_err(io_error)?.is_none() {
                return Err(DehydrateError::NotManaged);
            }
        } else if !meta.is_dir() {
            return Err(DehydrateError::OutsideRoot);
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
    pub(super) fn open_registered(&self) -> io::Result<Option<File>> {
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
    pub(super) fn relative(&self, path: &Path) -> Result<PathBuf, DehydrateError> {
        let name = path.file_name().ok_or(DehydrateError::OutsideRoot)?;
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let parent = std::fs::canonicalize(parent).map_err(io_error)?;
        let root = std::fs::canonicalize(&self.path).map_err(io_error)?;
        let inside = parent.strip_prefix(&root).map_err(|_| DehydrateError::OutsideRoot)?;
        Ok(inside.join(name))
    }
}

/// The guard: only a clean, fully downloaded file may be emptied
/// (dehydration's step 1). It runs on the very descriptor the punch will
/// use, so what it decided about cannot be swapped for something else
/// afterwards.
pub fn check_dehydratable(file: &File) -> Result<(), DehydrateError> {
    match read_state(file).map_err(io_error)? {
        None => Err(DehydrateError::NotManaged),
        Some(State::Hydrated) => {
            if stamp_matches(file).map_err(io_error)? {
                Ok(())
            } else {
                Err(DehydrateError::ModifiedLocally)
            }
        }
        Some(_) => Err(DehydrateError::NotHydrated),
    }
}

/// Whether `file` is a zero-byte file as `create_placeholder` makes one:
/// `hydrated` from birth, with no stamp, since there is nothing to download.
/// Freeing it up has nothing to free, and succeeds without touching it
/// — it used to fail the stamp check and answer
/// `ModifiedLocally`, which told the user their edits would be lost. A file
/// that *became* empty here still carries the stamp of what it held, fails
/// that check, and is refused as modified.
fn nothing_to_free(file: &File) -> Result<bool, DehydrateError> {
    if read_state(file).map_err(io_error)? != Some(State::Hydrated) {
        return Ok(false);
    }
    let empty = file.metadata().map_err(io_error)?.len() == 0;
    Ok(empty && read_stamp(file).map_err(io_error)?.is_none())
}

/// The timestamps a punch would destroy, kept so they can be put back
///.
#[derive(Clone, Copy)]
struct FileTimes {
    atime: libc::timespec,
    mtime: libc::timespec,
}

impl FileTimes {
    fn of(file: &File) -> io::Result<Self> {
        let meta = file.metadata()?;
        Ok(Self {
            atime: libc::timespec { tv_sec: meta.atime(), tv_nsec: meta.atime_nsec() },
            mtime: libc::timespec { tv_sec: meta.mtime(), tv_nsec: meta.mtime_nsec() },
        })
    }

    /// Puts both back on the same descriptor. `futimens` takes the two raw
    /// `timespec`s the file was carrying, so a pre-epoch or
    /// nanosecond-precise mtime survives the round trip exactly.
    fn restore(self, file: &File) -> io::Result<()> {
        let times = [self.atime, self.mtime];
        // SAFETY: `file` is an open descriptor and `times` is a live array of
        // exactly the two `timespec`s `futimens` reads.
        let rc = unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Dehydration's (`docs/design/hydration.md` §8) steps 1–2, first half:
/// check, then publish `dehydrating` and make
/// it durable *before* anyone is told to stop intercepting this file.
///
/// # Why this cannot be moved after `ClearIgnore`
///
/// The order is the difference between failing loudly and failing silently.
/// With the mark cleared and the state still reading `hydrated`, an open
/// landing in that window reaches the helper's `handle_open`, matches
/// `Ok(Some(State::Hydrated))`, and is answered by **placing a fresh ignore
/// mark and allowing** — and then we punch. The file ends up empty *and*
/// permanently un-intercepted: zeros, forever, with no `ClearIgnore` failure
/// anywhere to notice. The lease does not cover that window; it is taken
/// afterwards.
///
/// With `dehydrating` already on disk the same open takes the `hydrate(...)`
/// arm instead: it waits, and re-hydrates after we finish, which is exactly
/// what §8's closing paragraph promises. The `fsync` is what makes the new
/// state survive a crash in the middle of the sequence, where startup
/// recovery (§4.4) then finds a `dehydrating` file and cleans it up.
/// # The barrier's own failure rolls back, like the two either side
///
/// If the `fsync` fails, the state write before it still happened — in page
/// cache, on a file that is otherwise exactly as hydrated as it was a moment
/// ago. Returning `Err` and leaving `dehydrating` behind would announce a
/// dehydration that never started, and the next startup recovery would empty
/// a perfectly good, fully hydrated file on the strength of it. So this
/// failure rolls the state back to `hydrated` exactly as a refused
/// `ClearIgnore` and a refused lease do.
fn mark_dehydrating(file: &File) -> Result<FileTimes, DehydrateError> {
    check_dehydratable(file)?;
    // Captured before anything touches the file: `fallocate` bumps the mtime
    // to now, and §4.2 requires an `online-only` file's mtime to still be the
    // remote `lastModifiedDateTime` that `create_placeholder` set.
    let times = FileTimes::of(file).map_err(io_error)?;
    write_state(file, State::Dehydrating).map_err(io_error)?;
    if let Err(e) = file.sync_all() {
        return Err(roll_back(file, io_error(e)));
    }
    Ok(times)
}

/// Undoes [`mark_dehydrating`] when the sequence stops before the punch
/// (steps 2 and 3 both say "roll back and report").
///
/// The rollback's own failure is reported, never swallowed: it leaves a file
/// stuck in `dehydrating` with its blocks intact, which startup recovery
/// (§4.4) will punch and turn into `online-only` — correct, but a silent
/// "free up space failed" that empties the file at the next start is not
/// something to discover from a log line that was never written.
fn roll_back(file: &File, cause: DehydrateError) -> DehydrateError {
    match write_state(file, State::Hydrated) {
        Ok(()) => cause,
        Err(e) => {
            tracing::error!("cannot roll the state back to hydrated after {cause}: {e}");
            DehydrateError::Io(format!(
                "{cause}, and rolling the state back to hydrated failed: {e}"
            ))
        }
    }
}

/// Dehydration's steps 3–5: take the lease, punch, put the mtime back, publish
/// `online-only`.
///
/// The caller must have cleared the helper's ignore mark first (invariant
/// M3) — this is private for that reason: an exported function
/// that punches with no helper involvement puts the project's one silent,
/// unrecoverable failure behind nothing but a doc comment.
///
/// Only the two refusals *before* the punch roll the state back. Once
/// `fallocate` has run there is nothing to roll back to — the blocks are
/// gone and the file is not `hydrated` any more — so a failure from there on
/// leaves it `dehydrating` deliberately: startup recovery punches
/// whatever is left, sets `online-only` and removes the stamp, which is the
/// correct end state, and the next open hydrates it again.
fn punch_clean_file(file: &File, restore: FileTimes) -> Result<(), DehydrateError> {
    punch_clean_file_watched(file, restore, |_, _| {})
}

/// The two ends of the lease, as somewhere a test can stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watch {
    /// The lease has just been taken and nothing has been destroyed yet.
    UnderLease,
    /// The file is punched, its mtime is back, and it has been published
    /// `online-only`; the lease is about to be released.
    BeforeRelease,
}

/// As [`punch_clean_file`], with an observation point at each end of the
/// lease's life.
///
/// Both exist so that "nobody can open this file while it is being emptied"
/// is something a test can *measure* — an opener started at `UnderLease`
/// must still be suspended at `BeforeRelease` — rather than something a
/// reader has to infer from where a `drop` happens to sit. One point was not
/// enough: a lease released between the hook and the punch, or between the
/// punch and the state flip, both went unnoticed by a test that only asked
/// `F_GETLEASE` at the start. Production takes the empty
/// closure, which costs nothing.
fn punch_clean_file_watched(
    file: &File,
    restore: FileTimes,
    mut watch: impl FnMut(Watch, &File),
) -> Result<(), DehydrateError> {
    // Step 3. The kernel grants this only while nobody else holds the file
    // open, which is what makes emptying it safe; a refusal is "in use", and
    // the state must go back to `hydrated` before we report it.
    let lease = match WriteLease::take(file).map_err(io_error)? {
        Some(lease) => lease,
        None => return Err(roll_back(file, DehydrateError::InUse)),
    };

    watch(Watch::UnderLease, file);

    // Steps 4–5. The lease is held across all of them: `lease` is dropped
    // below, so every open arriving from here on waits for the break instead
    // of reading a file mid-punch or a file that is empty but still says
    // `dehydrating`.
    punch_and_publish(file, restore).map_err(io_error)?;
    watch(Watch::BeforeRelease, file);
    drop(lease);
    Ok(())
}

/// Dehydration's steps 4–5 on their own: empty the file, put the mtime back, make
/// that durable, and only then say it is `online-only`.
///
/// **The caller must hold the write lease across this call** and must have
/// confirmed the helper's `ClearIgnore` (invariant M3) before it. Both
/// [`punch_clean_file_watched`] and startup recovery's `reset_interrupted`
/// run it, because the sequence a crash left half-finished is the same
/// sequence a dehydration runs — what differs is only how the two arrive
/// here and what a refusal means to each of them, which is why the lease is
/// taken by the caller rather than in here.
fn punch_and_publish(file: &File, restore: FileTimes) -> io::Result<()> {
    punch_all(file)?;
    // Before the fsync, so the restored mtime is covered by it.
    restore.restore(file)?;
    file.sync_all()?;

    // The stamp described a hydrated file; leaving it behind would describe
    // this one wrongly, so its removal is reported rather than swallowed —
    // even though by now the file really is online-only.
    write_state(file, State::OnlineOnly)?;
    remove_stamp(file).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("the file is now online-only, but its stamp could not be removed: {e}"),
        )
    })
}

/// The full sequence, including the helper round trip.
///
/// `root` is the registered sync root the file must live in; see
/// [`SyncRoot::open_inside`] for what that is worth and why the check is
/// here.
///
/// # Why `clear_ignore`'s error must propagate — never `let _ =`
///
/// The helper's ignore mark carries `FAN_MARK_IGNORED_SURV_MODIFY` (spec
/// §5.1, invariant M3), which removed an accidental safety net: an ignore
/// mark used to be cleared by any modification to the file, so a dehydration
/// that skipped or failed the clear used to repair itself the moment the
/// punch modified the file. It no longer does. If this were ever weakened —
/// "the clear mostly works, and worst case the helper re-derives it later" —
/// a failed `ClearIgnore` followed by a punch leaves the file **empty and
/// permanently un-intercepted**: every future open is allowed straight
/// through by the stale ignore mark and reads zeros, silently, forever
/// (until the kernel happens to evict the inode). That is the worst outcome
/// this whole sub-project exists to avoid, and it would happen with no error
/// visible anywhere past this point. Do not "simplify" this into a swallowed
/// error.
///
/// The guarantee covers every *reported* failure: `Refused`, `Timeout`,
/// `NotRunning` and `Io` all arrive here as `Err` and stop the sequence, and
/// a panic propagates out of this function, which has no `catch_unwind`. It
/// cannot cover a helper that acknowledges success without having acted —
/// that is out of the daemon's reach by construction — and it is not a
/// guarantee `punch_clean_file` makes on its own, which is why that function
/// is private.
///
/// This needs no special case for "the mark was already gone": the helper
/// acks `ClearIgnore` with errno 0 both when it actually removed the mark
/// and when the mark was never there or the kernel had already evicted it
/// (an evictable mark is designed to vanish on its own) — see
/// `konedrive-helper/src/main.rs`'s `apply`/`ClearIgnore` handling and spec
/// §5.1. So any non-zero errno reaching here means something genuinely went
/// wrong, and stopping is always the right call.
///
/// # Cancelling this leaves the file `dehydrating`
///
/// Every failure *this function reports* rolls the state back or is past the
/// point where there is anything to roll back to. Dropping the future does
/// not: between `mark_dehydrating` and the punch the file reads
/// `dehydrating` on disk, and a caller that cancels there (a shutdown, a
/// `tokio::time::timeout`, a `select!` losing a race) leaves it that way,
/// with its blocks intact and its ignore mark possibly already cleared.
/// That is a safe state, not a lost one — the helper treats `dehydrating`
/// as "hydrate it again", and [`recover`] punches and relabels
/// it at the next start — but it is not a *tidy* one, so a caller that can
/// cancel should prefer to let the sequence finish.
pub async fn dehydrate(
    link: &HelperLink,
    root: &SyncRoot,
    path: &Path,
) -> Result<(), DehydrateError> {
    // The file work is blocking and belongs on a blocking
    // thread.
    let target = path.to_path_buf();
    let owned_root = root.clone();
    let file = tokio::task::spawn_blocking(move || owned_root.open_inside(&target))
        .await
        .map_err(|e| DehydrateError::Io(format!("the dehydration task failed: {e}")))??;
    dehydrate_opened(&Clearance::Link(link.clone()), file).await
}

/// [`dehydrate`] from the descriptor on, so that a caller which had to open
/// the file itself — `SyncService::dehydrate`, which needs its inode to take
/// per-inode lock *before* anything is marked —
/// hands that same descriptor straight in rather than resolving the name a
/// second time. is thereby strengthened, not weakened: there is
/// now exactly one open per dehydration, and it is the one every step uses.
///
/// # What stands between the punch and a stale ignore mark
///
/// `clearance` is step 2, decided by the local rule on
/// [`Clearance`]: with a helper link, the helper clears the mark — for a
/// folder without interception too, where it used to be skipped; with no
/// helper bound to its socket, no mark of ours exists; with a helper bound
/// and no link, nothing is emptied and the call answers
/// [`DehydrateError::HelperNotConnected`]. `SyncService::dehydrate` passes
/// the link for an intercepted root and refuses `NoHelper` without one, since
/// a file freed up there with nothing intercepting would read zeros.
pub(super) async fn dehydrate_opened(
    clearance: &Clearance,
    file: File,
) -> Result<(), DehydrateError> {
    // Each phase hands the descriptor on to the next, so all three still
    // operate on the single open of.
    let marked = tokio::task::spawn_blocking(move || {
        if nothing_to_free(&file)? {
            return Ok(None);
        }
        let restore = mark_dehydrating(&file)?;
        Ok::<_, DehydrateError>(Some((file, restore)))
    })
    .await
    .map_err(|e| DehydrateError::Io(format!("the dehydration task failed: {e}")))??;
    let Some((file, restore)) = marked else {
        return Ok(());
    };

    // Step 2, second half: local rule, here, where the file
    // is about to be emptied, and after `dehydrating` is durable (H69) — so a
    // mark an opener places from now on is placed through a `hydrate` path
    // that reads `dehydrating` and takes it off again (H139). The descriptor
    // the helper is handed is the one that was just checked and marked, and
    // the one that is about to be punched: the mark cannot be cleared on one
    // inode and a hole punched in another.
    //
    // Nothing here rests on what can or cannot have happened to the file
    // before: not on "a folder without interception carries no mark that
    // matters", which was falsified three times (H132, C2, N2), each time by
    // a race nobody had seen. A link means the helper clears the mark; no
    // helper bound to its socket means no group of ours, and so no mark,
    // exists; a helper bound and no link means nothing is emptied.
    if let Err(e) = clearance.clear(&file).await {
        return Err(tokio::task::spawn_blocking(move || roll_back(&file, DehydrateError::from(e)))
            .await
            .unwrap_or_else(|e| DehydrateError::Io(format!("the rollback task failed: {e}"))));
    }

    tokio::task::spawn_blocking(move || punch_clean_file(&file, restore))
        .await
        .map_err(|e| DehydrateError::Io(format!("the dehydration task failed: {e}")))?
}

/// How much of a registered root a startup [`recover`] found, fixed, and
/// could not reach.
///
/// `{ reset, scanned }` alone could not tell "nothing needed fixing" from
/// "every single file was refused", and the second of those leaves files in
/// `dehydrating` — invariant M3's dangerous state — waiting for a next start
/// that may never come. The four counts below are what a caller needs to see
/// that, and every file the walk meets lands in exactly one of them (or in
/// none, when it is simply not ours).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Files carrying a `user.konedrive.state` this daemon wrote: everything
    /// it manages, in whatever state the crash left it. A file with no state
    /// xattr at all is not ours and is not counted here.
    pub scanned: usize,
    /// Interrupted files (`hydrating` or `dehydrating`) that were punched
    /// back to `online-only`. A subset of `scanned`.
    pub reset: usize,
    /// Interrupted files that could **not** be reset: the helper refused the
    /// `ClearIgnore`, or the punch itself failed. Each is left exactly as it
    /// was found, for the next start to try again. A subset of `scanned`,
    /// disjoint from `reset` and `busy`.
    pub failed: usize,
    /// Things recovery could not look at at all: a directory it could not
    /// open or list, a file it could not open, a file whose state xattr it
    /// could not read, a subtree on another filesystem, or a branch deeper
    /// than [`MAX_DEPTH`]. Anything counted here may be hiding an
    /// interrupted file, so a non-zero value means the root was **not**
    /// fully recovered.
    pub skipped: usize,
    /// Interrupted files something had open, so no lease could be taken
    ///. Not a failure: whatever has the
    /// file open is, as often as not, an opener waiting for it to be filled
    /// — or, after a reconnect, a fill from the connection before, still
    /// running — and an interrupted file is one the helper
    /// fills on its next open. Left exactly as found, like a failure, for the
    /// next start if nothing fills it first. A subset of `scanned`.
    pub busy: usize,
    /// Interrupted files left exactly as found because a helper is running
    /// and this daemon has no link to it yet, so a mark its group may hold
    /// on them cannot be cleared. Not a failure: recovery runs
    /// again once the link is up (`SyncService::resume`). A subset of
    /// `scanned`.
    pub deferred: usize,
}

/// Why a whole [`recover`] could not run. Everything smaller than this —
/// one unreadable directory, one file that could not be opened, one
/// `ClearIgnore` the helper refused — is logged and counted in the
/// [`RecoveryReport`] instead, because recovery is the one component whose
/// entire job is coping with a messy on-disk state.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("{0} no longer carries this sync root's registration")]
    NotRegistered(PathBuf),
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// Why one interrupted file could not be reset. The kind is preserved rather
/// than flattened into a string, so a caller (and a log reader) can tell a
/// helper that refused from a helper that timed out from a disk that is
/// full — three situations with three different answers.
#[derive(Debug, thiserror::Error)]
pub enum ResetError {
    #[error("the helper did not clear the ignore mark: {0}")]
    Helper(HelperError),
    /// A helper is running and there is no link to it.
    #[error("a konedrive helper is running and this daemon is not connected to it yet")]
    Unlinked,
    /// Under the lease the file no longer reads `hydrating` or
    /// `dehydrating` — something finished it after recovery first looked.
    #[error("the file was finished meanwhile; it now reads {0:?}")]
    Finished(Option<State>),
    #[error("the file is open in another process")]
    InUse,
    #[error("{0}")]
    Io(#[from] io::Error),
}

impl From<NotCleared> for ResetError {
    fn from(e: NotCleared) -> Self {
        match e {
            NotCleared::Helper(e) => ResetError::Helper(e),
            NotCleared::Unlinked => ResetError::Unlinked,
            NotCleared::Unknown(why) => ResetError::Io(io::Error::other(why)),
        }
    }
}

/// Startup recovery,: after a crash or power loss, a file caught
/// mid-hydration or mid-dehydration holds content that must not be trusted —
/// punch it back to `online-only` so the next open fetches it again.
///
/// # Why this takes a [`Clearance`], unlike 's own draft
///
/// A file that crashed `dehydrating` can still be carrying its ignore mark:
/// the daemon may have died between `write_state(Dehydrating)` and a
/// successful `ClearIgnore`. That mark carries
/// `FAN_MARK_IGNORED_SURV_MODIFY` (`docs/design/hydration.md` §4.3), so
/// nothing clears it on its own any more, and punching a file that still
/// carries it reproduces the exact defect `dehydrate` in this module exists
/// to prevent (invariant M3): the file ends up empty *and* permanently
/// un-intercepted, reading as zeros on every future open with no error
/// anywhere to notice it. So every file this function is about to punch goes
/// through the same local rule `dehydrate` applies (on
/// [`Clearance`]) — on the same descriptor it is about to punch, never a
/// path re-open — and a file the rule does not clear is left
/// exactly as it was found: counted `failed` when the helper refused, and
/// `deferred` when a helper is running that this daemon has no link to,
/// until the link is up. This needs no special case for a mark that was
/// never there or that the kernel already evicted: the helper acks that as
/// success too (see `dehydrate`'s doc comment).
///
/// For an intercepted root the caller passes the link it has just
/// registered the root on — callers must connect to the helper and register
/// their roots before recovering them, not after. For a root
/// registered without interception the caller passes whatever the rule has
/// to go on: its link if it has one, the helper's socket if not.
///
/// # The file can be finished while recovery looks at it
///
/// Recovery runs on every reconnect, and the previous connection's fills
/// keep running while it walks. It read a file's state once, when it
/// opened it, and nothing stopped a fill from committing
/// `hydrated` — and an opener from having the helper ignore-mark the file —
/// between recovery's `ClearIgnore` and its lease: recovery then punched a
/// complete file under a fresh mark, and the next reader got 65 536 zero
/// bytes (measured with that gap widened). So,
/// for each interrupted file:
///
/// - the per-inode lock every fill and every free-up of this daemon holds
///   (`locks`, `SyncService`'s own) is taken, and if it is held the file is
///   left as it is and counted `busy` — something is filling it or freeing
///   it up right now. Taken without waiting: waiting would hold the reconnect
///   behind a download of any length;
/// - the state is read **again once the lease is held**, and the file is
///   punched only if it still reads `hydrating` or `dehydrating`. Under the
///   lease nothing else has the file open, so no fill is under way; and a
///   file that came back to one of those two states after reading
///   `hydrated` did so through a free-up or a fill that cleared its mark
///   first (M3), and cannot be marked again while it reads them. A file that
///   reads anything else was finished meanwhile, and is left as it is.
///
/// # The walk never leaves the root, by construction
///
/// This empties files, in bulk, across a whole tree, with no user pointing
/// at any of them — so the containment `SyncRoot::open_inside` gives
/// `dehydrate` matters more here, not less. It takes the `&SyncRoot` rather
/// than a path for exactly that reason, and:
///
/// - the root directory must still carry *this* root's `user.konedrive.root`
///   before anything inside it is touched at all ([`RecoveryError::NotRegistered`]);
/// - every step of the walk is an `openat` from a **directory descriptor**,
///   never a path — `O_DIRECTORY | O_NOFOLLOW` for subdirectories, `O_RDONLY
///   | O_NOFOLLOW` for files, reopened writable through the descriptor itself
///   only for the one being reset — so the name that was classified is the
///   name that is opened, out of a directory that cannot be swapped
///   underneath the walk while it waits for the helper;
/// - every descriptor is `fstat`ed after it is opened and refused unless it
///   is a regular file (or a directory) on the root's own `st_dev`.
///
/// The version this replaces re-resolved every subdirectory by absolute path
/// with `std::fs::read_dir` and opened every file with `File::open`, both of
/// which follow symlinks, having classified the entry with an lstat-shaped
/// `DirEntry::file_type` an unbounded time earlier — the gap is a helper
/// round trip per interrupted file. Reproduced by the review: with `sub/`
/// replaced by a symlink to a directory outside the root while recovery
/// awaited a `ClearIgnore` ack, a file outside the root was emptied,
/// relabelled `online-only` and counted as a success. The helper does not
/// back this out: its own check is "same uid, same *filesystem* as one of
/// that uid's roots", which any file in the user's home satisfies.
///
/// # One file open at a time
///
/// The walk opens a file, decides about it, punches it and closes it before
/// it opens the next one. The version this replaces opened *every* regular
/// file in a directory `O_RDWR` — whatever its state — and held all of those
/// descriptors until the directory was finished, with `Err(_) => continue`
/// turning the inevitable `EMFILE` into silence: measured, 2000 interrupted
/// files under the default `RLIMIT_NOFILE` of 1024 gave
/// `RecoveryReport { reset: 1009, scanned: 1009 }` and left 991 files
/// `hydrating` with untrusted content, no error and no log line. What the
/// walk does hold is one directory descriptor per *level* of the tree it is
/// currently inside, which is bounded by the depth of the tree rather than
/// by the size of any directory, and a directory that cannot be opened is
/// counted in `skipped` rather than passed over in silence.
pub async fn recover(
    clearance: &Clearance,
    root: &SyncRoot,
    locks: &InodeLocks,
) -> Result<RecoveryReport, RecoveryError> {
    let opened = root.clone();
    let root_dir = on_blocking_thread(move || opened.open_registered())
        .await??
        .ok_or_else(|| RecoveryError::NotRegistered(root.path.clone()))?;
    let root_dev = root_dir.metadata()?.dev();

    let mut report = RecoveryReport::default();
    let mut stack = Vec::new();
    descend(&mut stack, Arc::new(root_dir), root.path.clone(), 0, &mut report).await;

    while let Some(mut frame) = stack.pop() {
        let Some((name, kind)) = frame.names.next() else {
            // Finished: the directory descriptor goes with the frame.
            continue;
        };
        let dir = Arc::clone(&frame.dir);
        let shown = frame.shown.join(&name);
        let child_depth = frame.depth + 1;
        // Back on the stack before anything is awaited, so the walk resumes
        // in this directory — through this descriptor — afterwards.
        stack.push(frame);

        let opened = on_blocking_thread(move || open_entry(&dir, &name, kind, root_dev)).await?;
        match opened {
            Err(e) => {
                report.skipped += 1;
                tracing::warn!("startup recovery: cannot open {}: {e}", shown.display());
            }
            Ok(Entry::Elsewhere) => {}
            Ok(Entry::OtherFilesystem) => {
                report.skipped += 1;
                tracing::warn!(
                    "startup recovery: {} is on a different filesystem than the root and was \
                     not recovered",
                    shown.display()
                );
            }
            Ok(Entry::Directory(sub)) => {
                descend(&mut stack, Arc::new(sub), shown, child_depth, &mut report).await;
            }
            Ok(Entry::File(file, state)) => {
                recover_file(clearance, locks, file, state, &shown, &mut report).await;
            }
        }
    }
    Ok(report)
}

/// One directory the walk is part-way through: the descriptor everything
/// inside it is opened from, and the names still to look at.
///
/// Names, not descriptors: a directory of 100 000 files costs one `Vec` of
/// its names, while opening them up front would cost 100 000 descriptors
///. The `FileType` beside each name is the `d_type` the kernel
/// gave us — a hint for which of the two opens to attempt, never the
/// authority for what was opened, which is the `fstat` in [`open_entry`].
struct Frame {
    dir: Arc<File>,
    /// The path a person would recognise, for log lines only. Nothing is
    /// ever opened through it.
    shown: PathBuf,
    names: std::vec::IntoIter<(OsString, std::fs::FileType)>,
    /// This directory's nesting level under the root, root itself being 0 —
    /// the same convention the helper's `walk_below` uses for `MAX_DEPTH`.
    depth: usize,
}

/// Lists `dir` and pushes it onto the walk. A directory that cannot be
/// listed is counted and left behind rather than ending the walk: a
/// mode-`000` subdirectory, or one the user deleted while the daemon
/// was starting, used to make `recover` return `Err` for the *whole* root,
/// losing the count of what it had already punched and never visiting a
/// sibling subtree.
async fn descend(
    stack: &mut Vec<Frame>,
    dir: Arc<File>,
    shown: PathBuf,
    depth: usize,
    report: &mut RecoveryReport,
) {
    if depth >= MAX_DEPTH {
        // The helper's own walk stopped marking at this depth — the same
        // `konedrive_fs::MAX_DEPTH` — so anything under here is unmarked and
        // cannot be intercepted regardless of what recovery finds in it. Counted, not
        // silent: `skipped`'s own doc comment promises that.
        report.skipped += 1;
        tracing::warn!(
            "startup recovery: {} is deeper than {MAX_DEPTH} levels, matching the helper's own \
             limit — not walked further",
            shown.display()
        );
        return;
    }
    let listing = {
        let dir = Arc::clone(&dir);
        on_blocking_thread(move || list_names(&dir)).await
    };
    match listing {
        Ok(Ok((names, unreadable))) => {
            report.skipped += unreadable;
            if unreadable > 0 {
                tracing::warn!(
                    "startup recovery: {unreadable} entries of {} could not be read",
                    shown.display()
                );
            }
            stack.push(Frame { dir, shown, names: names.into_iter(), depth });
        }
        Ok(Err(e)) | Err(e) => {
            report.skipped += 1;
            tracing::warn!(
                "startup recovery: cannot read {}, so nothing inside it is recovered this \
                 time: {e}",
                shown.display()
            );
        }
    }
}

/// Every name in an open directory, with the entry kind the kernel reported
/// for it, plus a count of the entries that could not be read at all.
///
/// The directory is listed through `/proc/self/fd/<n>`, so the listing is of
/// the descriptor the walk holds — not of whatever the path may name by now.
fn list_names(dir: &File) -> io::Result<(Vec<(OsString, std::fs::FileType)>, usize)> {
    let mut names = Vec::new();
    let mut unreadable = 0;
    for entry in std::fs::read_dir(proc_path(dir))? {
        match entry.and_then(|entry| Ok((entry.file_name(), entry.file_type()?))) {
            Ok(entry) => names.push(entry),
            Err(_) => unreadable += 1,
        }
    }
    Ok((names, unreadable))
}

/// What one name in a directory turned out to be, once opened.
enum Entry {
    /// A directory on the root's own filesystem, to walk into.
    Directory(File),
    /// A regular file on the root's own filesystem, open read-only, with
    /// whatever `user.konedrive.state` it carries — including the error of
    /// failing to make sense of it, which is emphatically not the same as
    /// carrying none (see `konedrive_fs::placeholder::StateError`).
    File(File, Result<Option<State>, StateError>),
    /// Something startup recovery has no business opening for writing: a
    /// symlink, a FIFO, a socket or a device node. None of it is ours, and
    /// none of it is worth a log line — a directory listing full of a
    /// user's ordinary files is expected to contain exactly this kind of
    /// thing, and recovery not commenting on every one of them is the point.
    Elsewhere,
    /// A directory or file `fstat`-confirmed to be on a filesystem other
    /// than the root's own `st_dev` — a bind mount or a removable disk
    /// mounted inside the sync folder. Unlike [`Entry::Elsewhere`] this is
    /// not silent: the helper's own validation is scoped to the root's
    /// device, so punching across one is exactly the escape it would not
    /// catch, and a whole subtree excluded this way could be
    /// hiding an interrupted file recovery never looked at — precisely what
    /// `RecoveryReport::skipped`'s doc comment says a non-zero value means.
    OtherFilesystem,
}

/// Opens one name **from the directory descriptor it was listed in**, with
/// the flags its kind calls for, and then proves on the open descriptor what
/// it is.
///
/// `kind` is only the `d_type` from the listing: it decides which of the two
/// opens to attempt, and is never trusted for anything after that. A name
/// that was a regular file when it was listed and is a symlink by the time
/// it is opened is refused by `O_NOFOLLOW` (`ELOOP`); one that has become a
/// directory is refused by the `fstat` below; one that has become a FIFO or
/// a device node is opened once, read-only, and refused by the same `fstat`
/// before anything at all is read from it or written to it. (`O_NONBLOCK`
/// keeps a read-only open of a FIFO from waiting for a writer, and an
/// unprivileged process cannot create a device node inside a sync root to
/// begin with.)
///
/// Files are opened **read-only**: under the read
/// phase's lock every file is `0444`, and `O_RDWR` would refuse them all —
/// every interrupted file counted `skipped` and never reset. Nothing is
/// written through this descriptor; `reset_interrupted` reopens the one file
/// it is about to reset writable, on the same inode, so a file that is not
/// ours is never made writable, not even for the moment of an open.
fn open_entry(
    dir: &File,
    name: &OsString,
    kind: std::fs::FileType,
    root_dev: u64,
) -> io::Result<Entry> {
    let flags = if kind.is_dir() {
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
    } else if kind.is_file() {
        // Read-only: under the lock every file is
        // `0444`, and the one file recovery is about to reset is reopened
        // writable in `reset_interrupted` — never a file that is not ours.
        OFlag::O_RDONLY | OFlag::O_NONBLOCK | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
    } else {
        return Ok(Entry::Elsewhere);
    };
    let opened = match nix::fcntl::openat(dir.as_fd(), name.as_os_str(), flags, Mode::empty()) {
        Ok(fd) => File::from(fd),
        // The name stopped being what it was between the listing and here:
        // a symlink now (`ELOOP`), or no longer a directory (`ENOTDIR`, also
        // what `O_DIRECTORY | O_NOFOLLOW` reports for a symlink). Neither is
        // ours to open, and neither is a reason to stop.
        Err(Errno::ELOOP | Errno::ENOTDIR | Errno::EISDIR) => return Ok(Entry::Elsewhere),
        Err(e) => return Err(e.into()),
    };

    let meta = opened.metadata()?;
    if meta.dev() != root_dev {
        return Ok(Entry::OtherFilesystem);
    }
    if meta.is_dir() {
        return Ok(Entry::Directory(opened));
    }
    if !meta.is_file() {
        return Ok(Entry::Elsewhere);
    }
    let state = read_state(&opened);
    Ok(Entry::File(opened, state))
}

/// What to do about one open file: nothing at all unless it is one of ours
/// and the crash caught it mid-operation.
async fn recover_file(
    clearance: &Clearance,
    locks: &InodeLocks,
    file: File,
    state: Result<Option<State>, StateError>,
    shown: &Path,
    report: &mut RecoveryReport,
) {
    let state = match state {
        // Not a file this daemon manages. Not ours to count, and certainly
        // not ours to empty.
        Ok(None) => return,
        Ok(Some(state)) => state,
        // A managed file whose state we cannot make sense of, or cannot read
        // at all. Recovery must not punch it — `unwrap_or(None)` would have
        // called it "not one of ours" and moved on — but it must not pass
        // over it in silence either: §5.2 has the helper deny every open of
        // such a file with `EIO` for as long as it stays that way, and
        // recovery is the one place that walks the whole tree and could
        // notice.
        Err(e) => {
            report.skipped += 1;
            tracing::error!(
                "startup recovery: cannot read the state of {}: {e}. It is left exactly as it \
                 is — the helper denies every open of a managed file in an unknown state — but \
                 nothing here can repair it",
                shown.display()
            );
            return;
        }
    };
    report.scanned += 1;
    if !matches!(state, State::Hydrating | State::Dehydrating) {
        return;
    }
    // Not while this daemon fills or frees up the same file.
    let _guard = match InodeKey::of(&file) {
        Ok(key) => match locks.try_lock(key) {
            Some(guard) => guard,
            None => {
                report.busy += 1;
                tracing::info!(
                    "startup recovery: {} is {state:?} and being filled or freed up right now; \
                     left as it is",
                    shown.display()
                );
                return;
            }
        },
        Err(e) => {
            report.failed += 1;
            tracing::error!(
                "startup recovery: cannot tell which file {} is ({e}); left as found",
                shown.display()
            );
            return;
        }
    };
    match reset_interrupted(clearance, file).await {
        Ok(()) => report.reset += 1,
        Err(ResetError::Finished(now)) => {
            tracing::info!(
                "startup recovery: {} was {state:?} and is {now:?} now — finished while \
                 recovery looked at it; left as it is",
                shown.display()
            );
        }
        Err(ResetError::Unlinked) => {
            report.deferred += 1;
            tracing::info!(
                "startup recovery: {} is {state:?}, and a konedrive helper is running that this \
                 daemon is not connected to yet, so a mark it may hold on the file cannot be \
                 cleared; left as found until the connection is up",
                shown.display()
            );
        }
        Err(ResetError::InUse) => {
            report.busy += 1;
            tracing::info!(
                "startup recovery: {} is open elsewhere, {state:?}; left as found — its next \
                 open fills it, or the next start resets it",
                shown.display()
            );
        }
        Err(e) => {
            report.failed += 1;
            tracing::error!(
                "startup recovery: leaving {} exactly as found, {state:?}, for the next start: \
                 {e}",
                shown.display()
            );
        }
    }
}

/// Clears the ignore mark and punches one crash-interrupted file, both on
/// the inode the walk opened — through one writable reopen of the
/// descriptor the walk opened read-only (`/proc/self/fd/<n>`),
/// made before either — the identical sequence `dehydrate`'s
/// `mark_dehydrating`/`punch_clean_file` pair runs on a file this process is
/// actively working on, applied here to one a crash left mid-sequence
/// instead. Nothing here re-opens anything by path: the inode
/// that was classified is the inode that is punched, whatever the name points
/// at by the time the helper answers.
///
/// A `hydrating` file whose download left a checkpoint keeps the
/// checkpointed prefix and the checkpoint; only what lies past it
/// is punched, and the next open continues the download from there.
///
/// Returns before punching, leaving the file untouched, if `ClearIgnore` is
/// refused; that failure is never swallowed (see [`recover`]'s doc comment).
///
/// # The lease, and why recovery needs it more than `dehydrate`
///
/// `punch_clean_file` takes an `F_SETLEASE` before it empties a file, so
/// that an application which opens it mid-punch is suspended by the kernel
/// instead of reading a file with its blocks going away underneath. The
/// window is *wider* at startup, not narrower: the helper's own
/// `register_root` walk has to mark the whole tree before anything is
/// intercepted at all, so until that finishes any thumbnailer, backup or
/// indexer can have a `hydrating`/`dehydrating` file open while this runs.
/// A refused lease means exactly that — somebody has it open — so the file
/// is left as it was found and counted, for a start that finds it quieter.
///
/// # The mtime
///
/// `fallocate` moves the mtime to now, and §4.2 wants an `online-only`
/// file's mtime to be the remote `lastModifiedDateTime`. Recovery has no
/// remote metadata to restore, so it restores what the file had a moment
/// before the punch — which for a `dehydrating` file is exactly the remote
/// stamp `create_placeholder` set, and for a `hydrating` one is the time the
/// interrupted download last wrote, the best available answer until the next
/// hydration sets it properly. What it must not do is leave *now*: a whole
/// tree of files that recovery touched would then look locally modified, and
/// each is a hole full of zeros, which is an upload-over-remote hazard the
/// moment a delta engine exists. The stamp is removed in the same sequence,
/// so `dehydrate`'s "modified locally" guard is not what saves you.
///
/// In a folder that shows OneDrive, the time of a `hydrating` file kept with
/// its checkpoint is the one thing left wrong, and not for long: every
/// bring-up starts the folder's sync, whose first cycle is a Full reconcile,
/// and that puts the tree's time back without touching the checkpoint
/// (`materialize::check_file`).
async fn reset_interrupted(clearance: &Clearance, file: File) -> Result<(), ResetError> {
    // Writable only now, and only this file: the walk opened
    // every file read-only, and a file that is not ours is never made
    // writable, even for a moment — only a file the walk read `hydrating` or
    // `dehydrating` gets here. The reopen goes through the descriptor, so it
    // is the inode that was classified, whatever the name leads to by now
    //.
    //
    // It comes *before* the clear, and the read-only descriptor is closed at
    // once, so that the mark is cleared on, and the lease taken on, one and
    // the same open file. A write lease is refused while any other open file
    // of the inode exists, and the helper link sends a *duplicate* of the
    // descriptor it is given, which its writer thread drops only after the
    // send — possibly after the `Ack` has already brought this function to
    // its lease. With the read-only descriptor handed to the helper, that
    // duplicate kept the read-only open file alive and the lease was refused:
    // measured, every file `busy` and none reset with the writer thread
    // delayed 50 ms after its send. A duplicate of the descriptor the lease
    // is taken on is the same open file, and refuses nothing.
    let file = on_blocking_thread(move || {
        let writable = konedrive_fs::placeholder::reopen_writable(&file)?;
        drop(file);
        Ok::<_, io::Error>(writable)
    })
    .await??;
    // Invariant M3, on the very descriptor the punch will use — by the local
    // rule (see [`Clearance`]), for a root with interception or
    // without. It used to be skipped for a root without interception, on the
    // strength of a chain of reasoning about where a stale mark could be; the
    // chain was falsified three times, and nothing here
    // depends on it any more.
    clearance.clear(&file).await?;
    // `fault-injection` builds only: the VM suite's N3 scenario.
    fault::recovery_after_clear().await;
    on_blocking_thread(move || {
        let Some(lease) = lease_retrying_briefly(&file)? else {
            return Err(ResetError::InUse);
        };
        // Look again, now that nothing else has the file open.
        let state = match read_state(&file) {
            Ok(Some(state @ (State::Hydrating | State::Dehydrating))) => state,
            Ok(now) => return Err(ResetError::Finished(now)),
            Err(e) => return Err(ResetError::Io(io::Error::other(e.to_string()))),
        };
        let restore = FileTimes::of(&file)?;
        // A download's checkpoint is kept with its bytes. Only a
        // `hydrating` file can have one; the bytes it counts were made
        // durable before it was written, and the fill that continues from it
        // checks them against the quickXorHash along with the rest.
        let checkpoint = match state {
            State::Hydrating => read_progress(&file)
                .ok()
                .flatten()
                .filter(|p| p.bytes > 0 && p.bytes <= file.metadata().map(|m| m.len()).unwrap_or(0)),
            _ => None,
        };
        match checkpoint {
            Some(progress) => keep_checkpoint(&file, restore, progress.bytes)?,
            None => {
                // The attribute before the punch: a count of bytes must
                // never outlive the bytes it counts, not even across a
                // failure or a crash between the two.
                remove_progress(&file)?;
                punch_and_publish(&file, restore)?;
            }
        }
        drop(lease);
        Ok(())
    })
    .await?
}

/// The write lease recovery punches under, with a refusal retried a few
/// times over about 75 ms before the file counts as in use.
///
/// `F_SETLEASE` is refused while any other open file of the inode exists
/// anywhere, and recovery's walk makes one of its own it cannot fully control:
/// the read-only descriptor it classified the file on, closed before the
/// lease — but a process being spawned at that moment holds an inherited copy
/// of it until its `exec`, and that copy alone refuses the lease. Measured on
/// this module's tests, where one test spawns `unshare`: 2 runs in 10 left an
/// interrupted file `busy`, none in 20 with that test skipped, and none in 30
/// on part 1's code, whose walk and lease shared one open file. Such a copy
/// goes within milliseconds; an application holding the file open does not,
/// and still finds the file `busy` after the last try, as before.
fn lease_retrying_briefly(file: &File) -> io::Result<Option<WriteLease<'_>>> {
    for pause in [5, 20, 50] {
        if let Some(lease) = WriteLease::take(file)? {
            return Ok(Some(lease));
        }
        std::thread::sleep(std::time::Duration::from_millis(pause));
    }
    WriteLease::take(file)
}

/// [`punch_and_publish`] for an interrupted download with a checkpoint: only
/// what lies past the checkpoint is punched, and the checkpoint stays
///. The order is otherwise the same — the punch and the mtime
/// made durable before the file is called `online-only`.
fn keep_checkpoint(file: &File, restore: FileTimes, bytes: u64) -> io::Result<()> {
    punch_from(file, bytes)?;
    restore.restore(file)?;
    file.sync_all()?;
    write_state(file, State::OnlineOnly)?;
    remove_stamp(file)
}

/// A deliberate stall for a race window too narrow to hit by chance —
/// compiled in **only** with the `fault-injection` cargo feature, like the
/// helper's. `tests/vm/Cargo.toml` builds this crate with it
/// for the VM suite; the daemon that ships does not have it.
///
/// Startup recovery clears a file's ignore mark and then takes its write
/// lease. A fill from the previous
/// helper connection can commit `hydrated` in between, and an opener can have
/// the file ignore-marked, in well under a millisecond; the VM suite widens
/// that gap to put both inside it.
#[cfg(feature = "fault-injection")]
pub mod fault {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static RECOVERY_STALL_MS: AtomicU64 = AtomicU64::new(0);

    /// From now on, every file recovery is about to reset waits this long
    /// between clearing its ignore mark and taking its lease. Zero disarms.
    pub fn set_recovery_stall(stall: Duration) {
        RECOVERY_STALL_MS.store(stall.as_millis() as u64, Ordering::SeqCst);
    }

    pub(super) async fn recovery_after_clear() {
        let ms = RECOVERY_STALL_MS.load(Ordering::SeqCst);
        if ms > 0 {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    }
}

/// The shipped build: nothing to arm, nothing compiled in.
#[cfg(not(feature = "fault-injection"))]
mod fault {
    #[inline(always)]
    pub(super) async fn recovery_after_clear() {}
}

/// Runs one blocking step of the walk on a blocking thread:
/// `openat`, `getxattr`, `fallocate` and `fsync` are all blocking syscalls,
/// and `sync/helper.rs`'s module doc treats a blocking call left on a tokio
/// worker as a first-class defect.
async fn on_blocking_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| io::Error::other(format!("the recovery task failed: {e}")))
}

#[cfg(test)]
mod tests;
