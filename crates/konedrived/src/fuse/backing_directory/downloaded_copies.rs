//! The downloaded copies in the backing directory: which file holds the whole content of a
//! OneDrive file.
//!
//! A downloaded copy is a regular file that is marked with its OneDrive id and marked whole.
//! Any other file at the path of a OneDrive file is not its copy — one the user put there
//! while nothing was mounted, the copy of another OneDrive file, a placeholder an older
//! konedrive left — and is never shown as its content, and never removed as its copy.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use konedrive_fs::placeholder::{self, State, XATTR_ITEM_ID, XATTR_STATE};

use super::{BackingDirectory, DiskMetadata};

impl BackingDirectory {
    /// The downloaded copy of the OneDrive file `id` at `rel`, if the backing directory
    /// holds it: its metadata.
    pub(in crate::fuse) fn find_downloaded_copy(&self, rel: &Path, id: &str) -> Option<DiskMetadata> {
        let path = self.path(rel);
        let metadata = fs::symlink_metadata(&path).ok().filter(fs::Metadata::is_file)?;
        let marked_with = xattr::get(&path, XATTR_ITEM_ID).ok().flatten()?;
        // Whole, too: a placeholder an older konedrive left under the file's name holds
        // nothing of it.
        let whole = xattr::get(&path, XATTR_STATE).ok().flatten().is_some_and(|state| state == State::Hydrated.as_str().as_bytes());
        if marked_with != id.as_bytes() || !whole {
            return None;
        }
        DiskMetadata::from_filesystem(&metadata)
    }

    pub(in crate::fuse) fn has_downloaded_copy(&self, rel: &Path, id: &str) -> bool {
        self.find_downloaded_copy(rel, id).is_some()
    }

    /// Marks the file at `rel` as the whole content of the OneDrive file `id`: its downloaded
    /// copy from here on.
    pub(in crate::fuse) fn mark_downloaded_copy(&self, rel: &Path, id: &str) -> io::Result<()> {
        let path = self.path(rel);
        xattr::set(&path, XATTR_ITEM_ID, id.as_bytes())?;
        xattr::set(&path, XATTR_STATE, State::Hydrated.as_str().as_bytes())
    }

    /// Removes the file at `rel` if it is marked with the OneDrive id `id` and no other. Its
    /// size on disk, when it was removed.
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
