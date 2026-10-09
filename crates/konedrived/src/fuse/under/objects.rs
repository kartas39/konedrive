//! What stands at a place of the folder: one object, looked at once, with the file handle
//! that is what it is, and the item id it carries.
//!
//! A file handle goes with its object through every rename and is never another object's, as
//! an inode number is once its file is removed.

use std::fs::{self, File};
use std::io;
use std::os::fd::AsFd;
use std::path::Path;

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::XATTR_ITEM_ID;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;

use super::Under;

/// A file or directory of the folder under the mount: its file handle, which is what it is,
/// and what the filesystem says of it.
#[derive(Debug, Clone)]
pub(in crate::fuse) struct Object {
    pub(in crate::fuse) handle: FileHandle,
    pub(in crate::fuse) meta: fs::Metadata,
}

impl Under {
    /// What stands at `rel`, looked at once, through the folder's descriptor and without
    /// following a link at its name: the handle and the rest are of one object, whatever
    /// takes the name meanwhile. `None` when nothing stands there.
    pub(in crate::fuse) fn object(&self, rel: &Path) -> Option<Object> {
        let found = nix::fcntl::openat(self.folder.as_fd(), rel, OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC, Mode::empty()).ok()?;
        let found = File::from(found);
        Some(Object { handle: FileHandle::of(&found).ok()?, meta: found.metadata().ok()? })
    }

    /// The item id the object at `rel` carries.
    pub(in crate::fuse) fn carried(&self, rel: &Path) -> Option<String> {
        self.attribute(rel, XATTR_ITEM_ID)
    }

    /// The object at `rel` is item `id`'s from here on.
    pub(in crate::fuse) fn carry(&self, rel: &Path, id: &str) -> io::Result<()> {
        xattr::set(self.path(rel), XATTR_ITEM_ID, id.as_bytes())
    }

    /// The object at `rel` is no item's from here on.
    pub(in crate::fuse) fn carry_nothing(&self, rel: &Path) {
        let _ = xattr::remove(self.path(rel), XATTR_ITEM_ID);
    }
}

#[cfg(test)]
mod tests;
