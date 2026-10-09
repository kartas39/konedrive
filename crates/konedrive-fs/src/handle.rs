//! File handles: the kernel's name for an inode that survives renames.
//!
//! `name_to_handle_at(2)` needs no privilege. The upload worker records the
//! handle of the object an item was sent from (`items.local_handle`), and
//! tells by it whether a directory that carries an item's id is that item's.
//!
//! The handle is taken without following a final symlink and without opening
//! anything.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
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
mod tests;
