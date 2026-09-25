//! File handles: the kernel's name for an inode that survives renames.
//!
//! `name_to_handle_at(2)` needs no privilege, and its handle is byte-equal to
//! the `FID`/`DFID` a fanotify notification group reports (measured, the kernel probe probe,
//! `docs/kernel-behavior-7.2.md` §14). The write phase keys an item's local
//! object on it (`items.local_handle`): an event's object handle finds the item
//! it touched, and a missing item's handle tells a delete from a move out of
//! the folder. Turning a handle back into a descriptor (`open_by_handle_at`)
//! needs `CAP_DAC_READ_SEARCH`, which only the helper has.
//!
//! The handle is taken without following a final symlink and without opening
//! anything, so asking for a placeholder's handle never fills it.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;

/// The largest handle the kernel hands out (`MAX_HANDLE_SZ`).
pub const MAX_HANDLE_BYTES: usize = 128;

/// An opaque file handle: `handle_type` and `f_handle` of `struct file_handle`.
/// Unique within one filesystem (a Btrfs subvolume's id is part of the
/// handle), and carrying the inode's generation where the filesystem has one,
/// so a reused inode number does not take over an old handle.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileHandle {
    pub kind: i32,
    pub bytes: Vec<u8>,
}

#[repr(C)]
struct RawHandle {
    handle_bytes: libc::c_uint,
    handle_type: libc::c_int,
    f_handle: [u8; MAX_HANDLE_BYTES],
}

impl FileHandle {
    /// The handle of `name` in `dir`, by name: nothing is opened and a final
    /// symlink is not followed.
    pub fn at(dir: &File, name: &OsStr) -> io::Result<Self> {
        let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        Self::raw(dir.as_raw_fd(), &name, 0)
    }

    /// The handle of the inode `file` is open on.
    pub fn of(file: &File) -> io::Result<Self> {
        Self::raw(file.as_raw_fd(), c"", libc::AT_EMPTY_PATH)
    }

    fn raw(dirfd: libc::c_int, name: &std::ffi::CStr, flags: libc::c_int) -> io::Result<Self> {
        let mut raw = RawHandle { handle_bytes: MAX_HANDLE_BYTES as libc::c_uint, handle_type: 0, f_handle: [0; MAX_HANDLE_BYTES] };
        let mut mount_id: libc::c_int = 0;
        // SAFETY: `raw` is a `struct file_handle` followed by MAX_HANDLE_SZ
        // bytes of room, as `handle_bytes` says; `name` is NUL-terminated.
        let rc = unsafe {
            libc::name_to_handle_at(dirfd, name.as_ptr(), (&mut raw as *mut RawHandle).cast(), &mut mount_id, flags)
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let len = (raw.handle_bytes as usize).min(MAX_HANDLE_BYTES);
        Ok(Self { kind: raw.handle_type, bytes: raw.f_handle[..len].to_vec() })
    }

    /// Whether the kernel could take this as a handle at all: a type that is
    /// not negative and between 1 and [`MAX_HANDLE_BYTES`] bytes. Checked
    /// before any system call, by the helper on every `OpenByHandle`.
    pub fn is_well_formed(&self) -> bool {
        self.kind >= 0 && !self.bytes.is_empty() && self.bytes.len() <= MAX_HANDLE_BYTES
    }

    /// Opens the object this handle names (`open_by_handle_at(2)`), on the
    /// filesystem and the mount of `mount`, with `flags`. No path is walked,
    /// so no directory's permissions are checked; the object's own are, as
    /// for any open. Needs `CAP_DAC_READ_SEARCH`: anyone else gets `EPERM`
    /// (measured, `docs/kernel-behavior-7.2.md` §14.6). A handle that names
    /// nothing any more is `ESTALE`; a malformed one is `EINVAL`, without a
    /// system call.
    pub fn open(&self, mount: BorrowedFd<'_>, flags: libc::c_int) -> io::Result<OwnedFd> {
        if !self.is_well_formed() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let mut raw = RawHandle {
            handle_bytes: self.bytes.len() as libc::c_uint,
            handle_type: self.kind,
            f_handle: [0; MAX_HANDLE_BYTES],
        };
        raw.f_handle[..self.bytes.len()].copy_from_slice(&self.bytes);
        // SAFETY: `raw` is a `struct file_handle` whose `handle_bytes` says
        // how many of the MAX_HANDLE_SZ bytes that follow are the handle.
        let fd = unsafe {
            libc::open_by_handle_at(mount.as_raw_fd(), (&mut raw as *mut RawHandle).cast(), flags)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor the kernel just returned, owned by no one else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Stored form: the type, 4 bytes little-endian, then the handle's bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.bytes.len());
        out.extend_from_slice(&self.kind.to_le_bytes());
        out.extend_from_slice(&self.bytes);
        out
    }

    /// `None` for anything [`encode`](Self::encode) cannot have written.
    pub fn decode(stored: &[u8]) -> Option<Self> {
        if stored.len() < 4 || stored.len() > 4 + MAX_HANDLE_BYTES {
            return None;
        }
        let kind = i32::from_le_bytes(stored[..4].try_into().ok()?);
        Some(Self { kind, bytes: stored[4..].to_vec() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_follows_the_inode_through_a_rename_and_not_the_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"one").unwrap();
        let root = File::open(dir.path()).unwrap();
        let before = FileHandle::at(&root, OsStr::new("a")).unwrap();
        std::fs::rename(dir.path().join("a"), dir.path().join("b")).unwrap();
        assert_eq!(FileHandle::at(&root, OsStr::new("b")).unwrap(), before, "the same inode under another name");
        assert_eq!(FileHandle::of(&File::open(dir.path().join("b")).unwrap()).unwrap(), before);
        std::fs::write(dir.path().join("a"), b"two").unwrap();
        assert_ne!(FileHandle::at(&root, OsStr::new("a")).unwrap(), before, "a new inode at the old name");
        assert_eq!(FileHandle::decode(&before.encode()), Some(before));
        assert_eq!(FileHandle::at(&root, OsStr::new("gone")).unwrap_err().raw_os_error(), Some(libc::ENOENT));
    }

    /// A handle the kernel could never have handed out is refused before any
    /// system call: the helper passes the daemon's bytes straight through.
    #[test]
    fn a_malformed_handle_is_refused_before_the_kernel_sees_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = File::open(dir.path()).unwrap();
        let einval = |handle: FileHandle| {
            assert!(!handle.is_well_formed(), "{handle:?}");
            handle.open(std::os::fd::AsFd::as_fd(&root), libc::O_PATH).unwrap_err().raw_os_error()
        };
        assert_eq!(einval(FileHandle { kind: 1, bytes: Vec::new() }), Some(libc::EINVAL));
        assert_eq!(einval(FileHandle { kind: 1, bytes: vec![0; MAX_HANDLE_BYTES + 1] }), Some(libc::EINVAL));
        assert_eq!(einval(FileHandle { kind: -1, bytes: vec![0; 8] }), Some(libc::EINVAL));
        assert!(FileHandle { kind: 0x4d, bytes: vec![0; MAX_HANDLE_BYTES] }.is_well_formed());
    }

    #[test]
    fn a_symlink_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("target"), b"x").unwrap();
        std::os::unix::fs::symlink("target", dir.path().join("link")).unwrap();
        let root = File::open(dir.path()).unwrap();
        // A filesystem may refuse a symlink a handle; it must never give it
        // the target's.
        if let Ok(link) = FileHandle::at(&root, OsStr::new("link")) {
            assert_ne!(link, FileHandle::at(&root, OsStr::new("target")).unwrap());
        }
    }
}
