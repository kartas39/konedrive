//! The folder under the mount: the directory on disk the mount lies over.
//!
//! [`Under`] owns the descriptor opened on the folder before the mount, and everything of the
//! folder is reached through it, never through the mount. It knows the disk and nothing else:
//! not the store, not the kernel's nodes, not the outbox, not the mount. What is an item's is
//! told by the id a file or directory carries, and whoever asks brings the id.
//!
//! What it answers for, each in a module of its own:
//!
//! - [`objects`]: what stands at a place, its file handle, and the item id it carries;
//! - [`copies`]: an item's downloaded copy — whether a place holds it, marking one, removing one;
//! - [`files`]: the files and directories a change makes, renames and removes;
//! - [`backing`]: the directories down to a place, each marked with its item's id;
//!
//! A place is always a path below the folder (`rel`); the folder itself is the empty path.

use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

mod backing;
mod copies;
mod files;
mod objects;

pub(super) use objects::Object;

/// Where an earlier build built its downloads, inside the folder: taken off when the folder
/// is opened. A download is built in a file without a name now.
const OLD_STAGING: &str = ".konedrive-staging";

/// The folder under the mount, by the descriptor opened on it before the mount.
pub(super) struct Under {
    folder: File,
}

impl Under {
    /// Opens `folder`, which no mount lies over yet: afterwards its path is the mount, and
    /// this is the one way to what it holds.
    pub(super) fn open(folder: &Path) -> io::Result<Self> {
        // `fusermount3` mounts only where its user may write; the folder was locked when the
        // daemon last ended.
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(folder, fs::Permissions::from_mode(0o755));
        }
        let under = Under { folder: File::open(folder)? };
        let _ = fs::remove_dir_all(under.path(Path::new(OLD_STAGING)));
        Ok(under)
    }

    /// The path of `rel` in the folder: through the descriptor, never through the mount. Good
    /// for as long as this lives.
    pub(super) fn path(&self, rel: &Path) -> PathBuf {
        konedrive_fs::proc_path(&self.folder).join(rel)
    }

    /// One more descriptor of the folder, for what in the daemon opens the folder by its path:
    /// that path is the mount.
    pub(super) fn descriptor(&self) -> io::Result<File> {
        self.folder.try_clone()
    }

    /// The space of the filesystem the folder is on.
    pub(super) fn space(&self) -> io::Result<libc::statvfs> {
        // SAFETY: a plain structure the call fills.
        let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: the folder's own descriptor.
        match unsafe { libc::fstatvfs(self.folder.as_raw_fd(), &mut v) } {
            0 => Ok(v),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// The attribute `name` of the object at `rel`; of the folder itself for the empty path.
    pub(super) fn attribute(&self, rel: &Path, name: &str) -> Option<String> {
        let value = match rel.as_os_str().is_empty() {
            true => xattr::FileExt::get_xattr(&self.folder, name),
            false => xattr::get(self.path(rel), name),
        };
        value.ok().flatten().map(|v| String::from_utf8_lossy(&v).into_owned())
    }
}

#[cfg(test)]
mod tests;
