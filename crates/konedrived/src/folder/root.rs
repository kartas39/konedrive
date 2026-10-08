use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use konedrive_fs::placeholder::{XATTR_DRIVE, XATTR_ROOT};
use konedrive_fs::probe::{probe_dir, ProbeError};
use konedrive_fs::proc_path;
use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;
use xattr::FileExt;


/// The folders a mount lies over, each with the descriptor opened on it before the mount.
static UNDER: Mutex<Vec<(PathBuf, Arc<File>)>> = Mutex::new(Vec::new());

/// From now on the folder at `path` is opened through `folder` ([`SyncRoot::open_registered`]):
/// a mount lies over its path.
pub(crate) fn mounted_over(path: &Path, folder: Arc<File>) {
    let mut under = crate::panic::lock(&UNDER);
    under.retain(|(at, _)| at != path);
    under.push((path.to_path_buf(), folder));
}

/// The mount over `path` is gone: the folder is opened by its path again.
pub(crate) fn mount_gone(path: &Path) {
    crate::panic::lock(&UNDER).retain(|(at, _)| at != path);
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

/// Makes `path` a sync root: the folder checked — a directory, empty unless it carries a
/// root id already, on a filesystem that keeps the attributes a downloaded copy carries —
/// and its root id read, or minted and written. Nothing is mounted here: the folder's first
/// sync does that.
///
/// A root id the folder carries is used again, so that a registration made again after a
/// failure, a Forget or a restart names the same folder.
pub async fn register_root(path: &Path) -> Result<SyncRoot, RegisterError> {
    // Opening, listing, probing and stamping a directory are all blocking syscalls.
    let requested = path.to_path_buf();
    tokio::task::spawn_blocking(move || prepare_root(&requested))
        .await
        .map_err(|e| RegisterError::Io(format!("the registration task failed: {e}")))?
        .map(|(_dir, root)| root)
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

/// [`register_root`]'s work, with the directory the checks ran on.
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
/// `8-4-4-4-12`, lowercase-or-uppercase hex throughout, version nibble `4`. The daemon
/// believes no other value it finds on a folder.
pub(crate) fn looks_like_a_root_id(id: &str) -> bool {
    let fields: Vec<&str> = id.split('-').collect();
    id.len() == 36
        && fields.iter().map(|f| f.len()).eq([8, 4, 4, 4, 12])
        && fields.iter().all(|f| f.chars().all(|c| c.is_ascii_hexdigit()))
        && fields[2].starts_with('4')
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

impl SyncRoot {
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
        // A folder a mount lies over is reached through the descriptor opened before the
        // mount: its path is the mount.
        let under = crate::panic::lock(&UNDER).iter().find(|(path, _)| *path == self.path).map(|(_, folder)| Arc::clone(folder));
        let dir = match under {
            Some(folder) => nix::fcntl::open(&proc_path(&*folder), flags.difference(OFlag::O_NOFOLLOW), Mode::empty()).map(File::from)?,
            None => nix::fcntl::open(&self.path, flags, Mode::empty()).map(File::from)?,
        };
        match dir.get_xattr(XATTR_ROOT)? {
            Some(id) if id == self.root_id.as_bytes() => Ok(Some(dir)),
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
