//! The downloaded copies in the folder: which file is an item's whole content.
//!
//! A copy is a regular file that carries its item's id and is marked whole. Any other file at
//! the item's place is not the item's — one the user put in the folder while nothing was
//! mounted, the copy of another item, a placeholder an older konedrive left — and is never
//! shown as its content, and never removed as its copy.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use konedrive_fs::placeholder::{self, State, XATTR_ITEM_ID, XATTR_STATE};

use super::BackingDirectory;

impl BackingDirectory {
    /// The downloaded copy of item `id` at `rel`, if the folder holds it: what the filesystem
    /// says of it.
    pub(in crate::fuse) fn find_downloaded_copy(&self, rel: &Path, id: &str) -> Option<fs::Metadata> {
        let path = self.path(rel);
        let meta = fs::symlink_metadata(&path).ok().filter(fs::Metadata::is_file)?;
        let carried = xattr::get(&path, XATTR_ITEM_ID).ok().flatten()?;
        // Whole, too: a placeholder an older konedrive left under the item's name holds
        // nothing of it.
        let whole = xattr::get(&path, XATTR_STATE).ok().flatten().is_some_and(|state| state == State::Hydrated.as_str().as_bytes());
        (carried == id.as_bytes() && whole).then_some(meta)
    }

    pub(in crate::fuse) fn has_downloaded_copy(&self, rel: &Path, id: &str) -> bool {
        self.find_downloaded_copy(rel, id).is_some()
    }

    /// Marks the file at `rel` as the whole content of item `id`: the item's copy from here on.
    pub(in crate::fuse) fn mark_downloaded_copy(&self, rel: &Path, id: &str) -> io::Result<()> {
        let path = self.path(rel);
        xattr::set(&path, XATTR_ITEM_ID, id.as_bytes())?;
        xattr::set(&path, XATTR_STATE, State::Hydrated.as_str().as_bytes())
    }

    /// Removes the file at `rel` if it carries the id of item `id` and no other. Its size on
    /// disk, when it went.
    pub(in crate::fuse) fn remove_downloaded_copy(&self, rel: &Path, id: &str) -> Option<u64> {
        let path = self.path(rel);
        let file = File::open(&path).ok()?;
        if placeholder::read_item_id(&file).ok().flatten().as_deref() != Some(id) {
            return None;
        }
        let bytes = file.metadata().map_or(0, |m| m.blocks() * 512);
        fs::remove_file(path).ok().map(|()| bytes)
    }
}

#[cfg(test)]
mod tests;
