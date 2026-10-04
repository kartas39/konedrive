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

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use konedrive_fs::handle::FileHandle;
use nix::fcntl::{openat2, OFlag, OpenHow, ResolveFlag};

use crate::helper::linked::Helper;
use crate::helper::HelperError;
use crate::folder::root::SyncRoot;

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
    /// item is examined again later. An answer that did not come in time is
    /// [`io::ErrorKind::TimedOut`], and no other error is: the examination
    /// asks nothing more in the run that met one.
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
/// which waits for it — one round trip per missing item (§3.4 rule 7), and
/// at most one that times out in a run.
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
/// on now, [`super::handles::prepare`]); a descriptor names the place, proved by
/// opening it again ([`same_place`]: a file whose dentry the kernel could not
/// connect reads as `/`, and decides nothing); any other refusal — `EPERM` is
/// never gone (F90) — or no answer decides nothing. The link's own timeout
/// is told apart ([`io::ErrorKind::TimedOut`]).
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
        Err(HelperError::Timeout) => Err(io::Error::new(io::ErrorKind::TimedOut, HelperError::Timeout.to_string())),
        Err(other) => Err(io::Error::other(other.to_string())),
    }
}

/// Opens `path` through the user's own lookups with no symbolic link anywhere in it — a link
/// in the way is an error, never followed — so that what is opened is what stands at the path
/// and not what a link there points at. Every place a path is trusted to name an object goes
/// through here: [`same_place`], [`absent_at`], and the handles taken again on a changed
/// filesystem ([`super::handles`]).
pub fn open_no_symlinks(path: &Path, flags: OFlag) -> io::Result<File> {
    let how = OpenHow::new().flags(flags | OFlag::O_CLOEXEC).resolve(ResolveFlag::RESOLVE_NO_SYMLINKS | ResolveFlag::RESOLVE_NO_MAGICLINKS);
    Ok(File::from(openat2(nix::fcntl::AT_FDCWD, path, how)?))
}

/// Whether `path`, opened again through the user's own lookups (no symlink,
/// the last part not followed), is the inode `object` is open on.
pub fn same_place(path: &Path, object: &File) -> bool {
    let Ok(there) = open_no_symlinks(path, OFlag::O_PATH | OFlag::O_NOFOLLOW) else { return false };
    let (Ok(a), Ok(b)) = (there.metadata(), object.metadata()) else { return false };
    (a.dev(), a.ino()) == (b.dev(), b.ino())
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
    absent_below(open_no_symlinks(parent, OFlag::O_PATH | OFlag::O_DIRECTORY), name, handle)
}
