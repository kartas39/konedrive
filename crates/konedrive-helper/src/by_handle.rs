//! `OpenByHandle` (`docs/design/writes.md` §8.2; SECURITY.md): a descriptor for an object of the
//! peer's own, named by its file handle, wherever it has gone.
//!
//! The daemon cannot turn a handle back into a descriptor —
//! `open_by_handle_at` needs `CAP_DAC_READ_SEARCH` — and needs one when an
//! item has left its folder: to see where it went, to keep a placeholder
//! intercepted (`MarkFile`), and to download it before the item is deleted
//! in OneDrive. This is the one new thing the helper does for that, and it
//! decides nothing but whether to answer.
//!
//! # Ownership plus the attribute is the whole authorisation
//!
//! File handles are guessable, so how the peer came by one counts for
//! nothing. What counts is checked on the object itself, once it is found:
//!
//! - the directory the peer passed is its own, on the device of one of its
//!   roots — the same test `MarkDir` passes;
//! - the object is on that directory's device. A Btrfs handle can name an
//!   object in another subvolume of the same filesystem, which has a device
//!   of its own;
//! - it is a regular file or a directory, and the peer owns it;
//! - it has a link left: an object deleted but still held open somewhere is
//!   gone (`ESTALE`), which is what the daemon asks to learn;
//! - it carries `user.konedrive.item-id`.
//!
//! Anything else is `EPERM`, whatever the reason, and the object is closed.
//!
//! # Looked at through `O_PATH` first
//!
//! An `O_PATH` open checks no permission, raises no fanotify event and breaks
//! no lease (measured, `docs/kernel-behavior-7.2.md` §15), so the object is
//! stat'ed before anything that could have an effect on it happens. Only one
//! that passes is opened for real: somebody else's file never is, nor a
//! device node or a FIFO. The real open is what the attribute
//! is read through, and what the daemon gets.
//!
//! # A regular file comes back read-only
//!
//! Measured under the shipped unit (§15): the helper is root with only
//! `CAP_SYS_ADMIN` and `CAP_DAC_READ_SEARCH`, so to a user's `0644` file it is
//! "other", and `O_RDWR` is `EACCES`. Adding `CAP_DAC_OVERRIDE` for this would
//! let the one root process every local user can talk to write any file on
//! the machine. The daemon owns the file, so it reopens the descriptor for
//! writing itself, through `/proc/self/fd`
//! (`konedrived::sync::helper::reopen_for_writing`). `O_NONBLOCK`, because a
//! file somebody holds a write lease on would otherwise stop this
//! connection's thread until the lease is broken (up to 45 s; §12.4):
//! the daemon gets `EAGAIN` and asks again.

use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::read_item_id;

/// What the object is, once it has passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
}

/// What `fstat` says about a descriptor: all the checks need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seen {
    pub uid: u32,
    pub dev: u64,
    pub ino: u64,
    /// `st_mode`, type bits included.
    pub mode: u32,
    pub nlink: u64,
}

impl Seen {
    pub fn of(fd: BorrowedFd<'_>) -> Result<Self, i32> {
        // SAFETY: `st` is a live, correctly sized `stat` that `fstat` fills;
        // `fstat` works on an `O_PATH` descriptor too.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
            return Err(last_errno());
        }
        Ok(Self { uid: st.st_uid, dev: st.st_dev, ino: st.st_ino, mode: st.st_mode, nlink: st.st_nlink })
    }

    fn kind(&self) -> Option<Kind> {
        match self.mode & libc::S_IFMT {
            libc::S_IFREG => Some(Kind::File),
            libc::S_IFDIR => Some(Kind::Directory),
            _ => None,
        }
    }
}

/// The directory the handle is opened relative to: the peer's own, on the
/// device of one of its roots (`on_a_root`, which the caller works out from
/// the registered roots for `anchor.dev`).
pub fn check_anchor(peer_uid: u32, anchor: &Seen, on_a_root: bool) -> Result<(), i32> {
    if anchor.kind() == Some(Kind::Directory) && anchor.uid == peer_uid && on_a_root {
        Ok(())
    } else {
        Err(libc::EPERM)
    }
}

/// The object the handle names, as `fstat` sees it. Ownership comes before
/// the link count, so that of an object not the peer's nothing more is said
/// than that it is not.
pub fn check_object(peer_uid: u32, anchor: &Seen, object: &Seen) -> Result<Kind, i32> {
    if object.dev != anchor.dev || object.uid != peer_uid {
        return Err(libc::EPERM);
    }
    let kind = object.kind().ok_or(libc::EPERM)?;
    if object.nlink == 0 {
        return Err(libc::ESTALE);
    }
    Ok(kind)
}

/// The flags of the real open (design §4.6, as measured in §15).
pub fn open_flags(kind: Kind) -> libc::c_int {
    match kind {
        Kind::File => libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        Kind::Directory => libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    }
}

/// The whole request. `on_a_root` says whether the peer holds a root on a
/// device. `Err` is the errno the daemon is answered with: `EINVAL` for a
/// handle the kernel could not have given, `EPERM` for anything the checks
/// refuse, `ESTALE` for an object that is gone, and otherwise what the kernel
/// said.
pub fn open(
    peer_uid: u32,
    anchor: &File,
    handle: &FileHandle,
    on_a_root: impl FnOnce(u64) -> bool,
) -> Result<OwnedFd, i32> {
    if !handle.is_well_formed() {
        return Err(libc::EINVAL);
    }
    let anchor_seen = Seen::of(anchor.as_fd())?;
    check_anchor(peer_uid, &anchor_seen, on_a_root(anchor_seen.dev))?;

    let found = handle
        .open(anchor.as_fd(), libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .map_err(|e| errno_of(&e))?;
    let first = Seen::of(found.as_fd())?;
    let kind = check_object(peer_uid, &anchor_seen, &first)?;
    drop(found);

    let object = handle.open(anchor.as_fd(), open_flags(kind)).map_err(|e| errno_of(&e))?;
    let now = Seen::of(object.as_fd())?;
    // A handle names one inode, so this is the same object; checked again
    // anyway, and its link count may have gone to 0 in between.
    if (now.dev, now.ino) != (first.dev, first.ino) {
        return Err(libc::EPERM);
    }
    check_object(peer_uid, &anchor_seen, &now)?;
    let object = File::from(object);
    match read_item_id(&object) {
        Ok(Some(_)) => Ok(object.into()),
        Ok(None) => Err(libc::EPERM),
        Err(e) => Err(errno_of(&e)),
    }
}

fn errno_of(e: &std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

fn last_errno() -> i32 {
    errno_of(&std::io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEER: u32 = 1000;

    fn seen(uid: u32, dev: u64, mode: u32, nlink: u64) -> Seen {
        Seen { uid, dev, ino: 7, mode, nlink }
    }

    fn dir() -> Seen {
        seen(PEER, 42, libc::S_IFDIR | 0o755, 1)
    }

    #[test]
    fn the_directory_must_be_the_peers_own_on_one_of_its_roots() {
        assert_eq!(check_anchor(PEER, &dir(), true), Ok(()));
        assert_eq!(check_anchor(PEER, &dir(), false), Err(libc::EPERM), "no root on its device");
        assert_eq!(check_anchor(PEER, &seen(1001, 42, libc::S_IFDIR | 0o755, 1), true), Err(libc::EPERM));
        assert_eq!(
            check_anchor(PEER, &seen(PEER, 42, libc::S_IFREG | 0o644, 1), true),
            Err(libc::EPERM),
            "not a directory"
        );
    }

    #[test]
    fn only_the_peers_own_file_or_directory_on_the_same_device_passes() {
        let file = seen(PEER, 42, libc::S_IFREG | 0o644, 1);
        assert_eq!(check_object(PEER, &dir(), &file), Ok(Kind::File));
        assert_eq!(check_object(PEER, &dir(), &dir()), Ok(Kind::Directory));
        assert_eq!(check_object(PEER, &dir(), &seen(1001, 42, libc::S_IFREG | 0o644, 1)), Err(libc::EPERM));
        assert_eq!(
            check_object(PEER, &dir(), &seen(PEER, 43, libc::S_IFREG | 0o644, 1)),
            Err(libc::EPERM),
            "another device: another filesystem, or another Btrfs subvolume"
        );
        for other in [libc::S_IFLNK, libc::S_IFIFO, libc::S_IFSOCK, libc::S_IFCHR, libc::S_IFBLK] {
            assert_eq!(check_object(PEER, &dir(), &seen(PEER, 42, other | 0o644, 1)), Err(libc::EPERM), "{other:o}");
        }
    }

    #[test]
    fn a_deleted_object_is_gone_but_only_the_peers_says_so() {
        assert_eq!(check_object(PEER, &dir(), &seen(PEER, 42, libc::S_IFREG | 0o644, 0)), Err(libc::ESTALE));
        assert_eq!(check_object(PEER, &dir(), &seen(PEER, 42, libc::S_IFDIR | 0o755, 0)), Err(libc::ESTALE));
        assert_eq!(
            check_object(PEER, &dir(), &seen(1001, 42, libc::S_IFREG | 0o644, 0)),
            Err(libc::EPERM),
            "somebody else's deleted file is refused like any of theirs"
        );
    }

    #[test]
    fn a_file_is_opened_read_only_and_non_blocking_a_directory_as_one() {
        let file = open_flags(Kind::File);
        assert_eq!(file & libc::O_ACCMODE, libc::O_RDONLY);
        assert_ne!(file & libc::O_NONBLOCK, 0);
        assert_ne!(file & libc::O_NOFOLLOW, 0);
        assert_eq!(file & libc::O_DIRECTORY, 0);
        let directory = open_flags(Kind::Directory);
        assert_eq!(directory & libc::O_ACCMODE, libc::O_RDONLY);
        assert_ne!(directory & libc::O_DIRECTORY, 0);
    }

    /// The limits are checked before anything is looked at: a handle the
    /// kernel could not have given is `EINVAL`, whatever the directory.
    #[test]
    fn a_malformed_handle_is_einval_before_anything_else() {
        let tmp = tempfile::tempdir().unwrap();
        let anchor = File::open(tmp.path()).unwrap();
        let never = |_: u64| -> bool { panic!("the roots must not be consulted") };
        for handle in [
            FileHandle { kind: 1, bytes: Vec::new() },
            FileHandle { kind: 1, bytes: vec![0; 129] },
            FileHandle { kind: -1, bytes: vec![0; 8] },
        ] {
            assert_eq!(open(PEER, &anchor, &handle, never).unwrap_err(), libc::EINVAL, "{handle:?}");
        }
    }

    /// Everything up to the kernel call, on the host: a directory that is not
    /// on one of the peer's roots is refused before any handle is opened.
    #[test]
    fn a_directory_off_the_peers_roots_is_refused_before_the_handle_is_opened() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f"), b"x").unwrap();
        let anchor = File::open(tmp.path()).unwrap();
        let handle = FileHandle::at(&anchor, std::ffi::OsStr::new("f")).unwrap();
        let me = nix::unistd::geteuid().as_raw();
        assert_eq!(open(me, &anchor, &handle, |_| false).unwrap_err(), libc::EPERM);
        assert_eq!(open(me + 1, &anchor, &handle, |_| true).unwrap_err(), libc::EPERM, "not the peer's");
    }
}
