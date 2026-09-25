//! "Is this object still there, and where?" — asked of a base item missing
//! from where it was (`docs/design/writes.md` §4 rule 7, §8).
//!
//! Decided by the object, never by events: a move out of the folder whose
//! event was lost, or that happened while the daemon was not running, is
//! still recognised, and a placeholder that left is never taken for a delete.
//! Only the helper can open a file handle (`open_by_handle_at` needs
//! `CAP_DAC_READ_SEARCH`), so the daemon's answer is the helper's
//! `OpenByHandle` ([`HelperLiveness`]): `ESTALE` is gone, a descriptor
//! says where the object is, and anything else — `EPERM` above all, which a
//! nested subvolume always gets (F90) — decides nothing. [`NoLiveness`]
//! decides nothing at all: a missing item stays where it is in the outbox's
//! eyes, and is never deleted on a guess.

#[cfg(test)]
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
use std::time::Duration;

use konedrive_fs::handle::FileHandle;
use nix::fcntl::{openat2, OFlag, OpenHow, ResolveFlag};

use crate::sync::helper::HelperError;
use crate::sync::root::SyncRoot;
use crate::sync::upload::move_out::Helper;
use crate::tree::Store;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Whereabouts {
    /// The handle is stale, or the object has no link left: it was deleted.
    Gone,
    /// Still alive, at this absolute path (read from the descriptor the
    /// helper returns). Inside the folder it is a move the batch did not
    /// see; outside it, a move out.
    At(PathBuf),
}

pub trait Liveness: Send + Sync {
    /// Where the object `handle` names is. An error decides nothing: the
    /// item is examined again later.
    fn whereabouts(&self, handle: &FileHandle) -> io::Result<Whereabouts>;
}

/// No helper to ask (yet): every question is left open.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoLiveness;

impl Liveness for NoLiveness {
    fn whereabouts(&self, _handle: &FileHandle) -> io::Result<Whereabouts> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "nothing can open a file handle yet"))
    }
}

/// The helper's answer: `OpenByHandle` relative to the folder's root, asked
/// from the examination's own thread (the watcher's, never the runtime's),
/// which waits for it — one round trip per missing item (§3.4 rule 7).
pub struct HelperLiveness {
    helper: Arc<dyn Helper>,
    root: SyncRoot,
    runtime: tokio::runtime::Handle,
}

/// Longer than the helper link's own call timeout (30 s), which answers
/// first.
const ASK_WITHIN: Duration = Duration::from_secs(45);

impl HelperLiveness {
    pub fn new(helper: Arc<dyn Helper>, root: SyncRoot, runtime: tokio::runtime::Handle) -> Self {
        Self { helper, root, runtime }
    }
}

impl Liveness for HelperLiveness {
    fn whereabouts(&self, handle: &FileHandle) -> io::Result<Whereabouts> {
        let dir = self
            .root
            .open_registered()?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the folder no longer carries its root id"))?;
        let (helper, handle) = (Arc::clone(&self.helper), handle.clone());
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.runtime.spawn(async move {
            let _ = tx.send(helper.open_by_handle(&dir, &handle).await);
        });
        let answer = rx.recv_timeout(ASK_WITHIN).map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the helper did not answer"))?;
        answered(answer)
    }
}

/// Where `OpenByHandle`'s answer says the object is: `ESTALE` is gone (the
/// examination believes it only for handles of the filesystem the folder is
/// on now, [`handles_current`]); a descriptor names the place, proved by
/// opening it again ([`same_place`]: a file whose dentry the kernel could not
/// connect reads as `/`, and decides nothing); any other refusal — `EPERM` is
/// never gone (F90) — or no answer decides nothing.
pub fn answered(answer: Result<OwnedFd, HelperError>) -> io::Result<Whereabouts> {
    match answer {
        Ok(object) => {
            let object = File::from(object);
            let path = std::fs::read_link(format!("/proc/self/fd/{}", object.as_raw_fd()))?;
            if !same_place(&path, &object) {
                return Err(io::Error::other(format!("{} is not where the object is", path.display())));
            }
            Ok(Whereabouts::At(path))
        }
        Err(HelperError::Refused(libc::ESTALE)) => Ok(Whereabouts::Gone),
        Err(HelperError::Refused(errno)) => Err(io::Error::from_raw_os_error(errno)),
        Err(other) => Err(io::Error::other(other.to_string())),
    }
}

/// Whether `path`, opened again through the user's own lookups (no symlink,
/// the last part not followed), is the inode `object` is open on.
pub fn same_place(path: &Path, object: &File) -> bool {
    let how = OpenHow::new()
        .flags(OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
        .resolve(ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS);
    let Ok(there) = openat2(nix::fcntl::AT_FDCWD, path, how) else { return false };
    let (Ok(a), Ok(b)) = (nix::sys::stat::fstat(&there), object.metadata()) else { return false };
    (a.st_dev, a.st_ino) == (b.dev(), b.ino())
}

/// The `meta` key naming the filesystem the folder's file handles were taken
/// on ([`handle_namespace`]).
pub const HANDLES_ON: &str = "handles_root";

/// The filesystem the folder's handles belong to: the root directory's own
/// handle, and the filesystem's UUID where the kernel gives one
/// (`FS_IOC_GETFSUUID`). Both survive a reboot, a remount and a renumbered
/// device (`f_fsid`, the device number on XFS and F2FS, does not, re-review
/// R1); both change when the folder is on another filesystem or subvolume —
/// the home moved to a new disk, restored from a backup together with the
/// store, a Btrfs snapshot rolled back. A handle taken there decodes to
/// `ESTALE` here, which then says nothing about the object: the kernel
/// answers every failure to decode a handle so.
pub fn handle_namespace(root: &File) -> io::Result<String> {
    let handle: String = FileHandle::of(root)?.encode().iter().map(|b| format!("{b:02x}")).collect();
    Ok(match fs_uuid(root) {
        Some(uuid) => format!("root:{handle};uuid:{uuid}"),
        None => format!("root:{handle}"),
    })
}

/// `FS_IOC_GETFSUUID`: `struct fsuuid2 { __u8 len; __u8 uuid[16]; }`, as hex.
fn fs_uuid(file: &File) -> Option<String> {
    const FS_IOC_GETFSUUID: libc::c_ulong = 0x8011_1500; // _IOR(0x15, 0, struct fsuuid2)
    let mut buf = [0u8; 17];
    // SAFETY: the ioctl writes at most `sizeof(struct fsuuid2)` (17) bytes into `buf`.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), FS_IOC_GETFSUUID, buf.as_mut_ptr()) };
    let len = usize::from(buf[0]).min(16);
    (rc == 0 && len > 0).then(|| buf[1..=len].iter().map(|b| format!("{b:02x}")).collect())
}

/// What the store says of the filesystem its handles were taken on, against
/// the one the folder is on now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handles {
    Current,
    /// Nothing recorded yet: this is the filesystem to record.
    Unrecorded(String),
    /// Recorded for another filesystem: this is the one to record once the
    /// handles are taken again ([`renew_handles`]).
    Changed(String),
    /// The folder's own handle cannot be read.
    Unknown,
}

pub fn handles(store: &Store, root: &File) -> Handles {
    let Ok(now) = handle_namespace(root) else { return Handles::Unknown };
    match store.with(|s| s.meta(HANDLES_ON)) {
        Ok(Some(recorded)) if recorded == now => Handles::Current,
        Ok(Some(_)) => Handles::Changed(now),
        Ok(None) => Handles::Unrecorded(now),
        Err(_) => Handles::Unknown,
    }
}

/// Whether the handles `store` records were taken on the filesystem `root` is
/// on now, and so whether an `ESTALE` for one of them may mean "deleted": the
/// first time it is asked, it records the filesystem. A changed filesystem is
/// not current until the examination has taken the handles again
/// ([`renew_handles`]).
pub fn handles_current(store: &Store, root: &File) -> bool {
    match handles(store, root) {
        Handles::Current => true,
        Handles::Unrecorded(now) => store.with(|s| s.set_meta(HANDLES_ON, Some(&now))).is_ok(),
        Handles::Changed(_) | Handles::Unknown => false,
    }
}

/// The folder's filesystem changed: its recorded handles say
/// nothing any more. In the store, in one go: every item forgets its local
/// object — a Full local scan takes each again where it is, and one missing
/// then is placed again from OneDrive rather than deleted (WR4) — and each
/// `move-out` row takes the handle of what stands at the place it last proved
/// if that carries the row's item id, or goes (the item stays in OneDrive and
/// is placed again). Then `now` is recorded. How many rows went.
pub fn renew_handles(store: &Store, now: &str) -> Result<usize, crate::tree::TreeError> {
    use crate::tree::outbox::{Inode, OutboxKind};
    let rows = store.with(|s| s.outbox_rows())?;
    let mut dropped = 0;
    for row in rows.into_iter().filter(|r| r.kind == OutboxKind::MoveOut) {
        let Some(id) = row.item_id.clone() else { continue };
        let found = row.target_name.as_deref().map(Path::new).filter(|p| p.is_absolute()).and_then(|path| {
            let carries = xattr::get(path, konedrive_fs::placeholder::XATTR_ITEM_ID).ok().flatten();
            let meta = std::fs::symlink_metadata(path).ok()?;
            let dir = File::open(path.parent()?).ok()?;
            let handle = FileHandle::at(&dir, path.file_name()?).ok()?;
            (carries.as_deref() == Some(id.as_bytes())).then(|| Inode { dev: meta.dev(), ino: meta.ino(), handle: Some(handle) })
        });
        match found {
            Some(inode) => {
                store.with(|s| s.outbox_amend(row.seq, |r| r.inode = Some(inode)))?;
            }
            None => {
                store.with(|s| s.outbox_drop(row.seq, None, Some(&id), None))?;
                dropped += 1;
            }
        }
    }
    store.with(|s| {
        s.forget_local_handles()?;
        s.set_meta(HANDLES_ON, Some(now))
    })?;
    Ok(dropped)
}

/// Whether the object `handle` names is proved absent from `name` in `dir` —
/// the evidence an `ESTALE` needs before it counts as a delete:
/// the directory or the name is not there (`ENOENT`), or another object
/// stands there. The object itself there, or any other answer (`EIO` above
/// all: an inode that cannot be read reads as `ESTALE` too), is no evidence.
pub fn absent_below(dir: io::Result<File>, name: &std::ffi::OsStr, handle: &FileHandle) -> bool {
    let gone = |e: &io::Error| matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR));
    let dir = match dir {
        Ok(dir) => dir,
        Err(e) => return gone(&e),
    };
    match FileHandle::at(&dir, name) {
        Ok(there) => &there != handle,
        Err(e) => gone(&e),
    }
}

/// [`absent_below`] at an absolute path, its directory opened through the
/// user's own lookups.
pub fn absent_at(path: &Path, handle: &FileHandle) -> bool {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { return false };
    let how = OpenHow::new()
        .flags(OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
        .resolve(ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS);
    let dir = openat2(nix::fcntl::AT_FDCWD, parent, how).map(File::from).map_err(io::Error::from);
    absent_below(dir, name, handle)
}

/// A table of where objects went, for tests: a handle it knows is alive
/// there, any other is gone. Never for the daemon itself: "gone" by default
/// would delete in OneDrive whatever it was not told about — so it exists
/// only in test builds.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct FakeLiveness {
    alive: Mutex<HashMap<FileHandle, PathBuf>>,
    asked: Mutex<Vec<FileHandle>>,
}

#[cfg(test)]
impl FakeLiveness {
    pub fn new() -> Self {
        Self::default()
    }

    /// The object `handle` names now lives at `path`.
    pub fn alive(&self, handle: FileHandle, path: impl Into<PathBuf>) {
        self.alive.lock().unwrap().insert(handle, path.into());
    }

    /// The object at `path` is alive there, and so is everything below it —
    /// as the helper answers for what a moved folder took along.
    pub fn alive_tree(&self, path: &Path) {
        let mut stack = vec![path.to_path_buf()];
        while let Some(p) = stack.pop() {
            let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else { continue };
            if let Ok(handle) = File::open(parent).and_then(|dir| FileHandle::at(&dir, name)) {
                self.alive(handle, p.clone());
            }
            if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
                stack.extend(std::fs::read_dir(&p).into_iter().flatten().flatten().map(|e| e.path()));
            }
        }
    }

    /// Every handle asked about, in order.
    pub fn asked(&self) -> Vec<FileHandle> {
        self.asked.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl Liveness for FakeLiveness {
    fn whereabouts(&self, handle: &FileHandle) -> io::Result<Whereabouts> {
        self.asked.lock().unwrap().push(handle.clone());
        Ok(match self.alive.lock().unwrap().get(handle) {
            Some(path) => Whereabouts::At(path.clone()),
            None => Whereabouts::Gone,
        })
    }
}
