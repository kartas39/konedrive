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

/// The marks a file or directory on disk carries: the OneDrive id it is marked with, and the
/// state it is marked with, each as it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fuse) struct DiskMarks {
    onedrive_id: Option<Vec<u8>>,
    state: Option<Vec<u8>>,
}

impl DiskMarks {
    /// The marks of whatever is at `path`, a path of the filesystem: of the backing
    /// directory through its descriptor, or of a folder no mount lies over yet.
    pub(in crate::fuse) fn read(path: &Path) -> Self {
        DiskMarks { onedrive_id: xattr::get(path, XATTR_ITEM_ID).ok().flatten(), state: xattr::get(path, XATTR_STATE).ok().flatten() }
    }

    /// Whether it is marked whole.
    fn is_whole(&self) -> bool {
        self.state.as_deref() == Some(State::Hydrated.as_str().as_bytes())
    }

    /// The one test of a downloaded copy: marked with the OneDrive id `onedrive_id`, and
    /// marked whole. The lookup of a downloaded copy and the landing of a download go by it.
    pub(in crate::fuse) fn is_downloaded_copy_of(&self, onedrive_id: &str) -> bool {
        self.onedrive_id.as_deref() == Some(onedrive_id.as_bytes()) && self.is_whole()
    }

    /// Whether it is marked with a OneDrive id, whichever, and is not whole: it holds
    /// nothing of that file.
    pub(in crate::fuse) fn is_partial_file(&self) -> bool {
        self.onedrive_id.is_some() && !self.is_whole()
    }

    /// Whether it is marked with a OneDrive id at all.
    pub(in crate::fuse) fn has_onedrive_id(&self) -> bool {
        self.onedrive_id.is_some()
    }

    /// Whether it is marked with a state that is not whole, whatever its OneDrive id: what
    /// an older konedrive's placeholder carries.
    pub(in crate::fuse) fn is_legacy_placeholder(&self) -> bool {
        self.state.is_some() && !self.is_whole()
    }
}

impl BackingDirectory {
    /// The downloaded copy of the OneDrive file `id` at `rel`, if the backing directory
    /// holds it: its metadata. A regular file that passes the one test
    /// ([`DiskMarks::is_downloaded_copy_of`]).
    pub(in crate::fuse) fn find_downloaded_copy(&self, rel: &Path, id: &str) -> Option<DiskMetadata> {
        let path = self.resolve_path(rel);
        let metadata = fs::symlink_metadata(&path).ok().filter(fs::Metadata::is_file)?;
        // Whole, too: a file that is marked with the id and holds nothing of it is not its
        // copy.
        if !DiskMarks::read(&path).is_downloaded_copy_of(id) {
            return None;
        }
        DiskMetadata::from_filesystem(&metadata)
    }

    /// A file at `path` that is marked with the OneDrive id `onedrive_id` and is not whole:
    /// it holds nothing of that file yet, and the mount shows nothing of it. Made where
    /// nothing is; one that is there already, marked with the same id and not whole, is
    /// taken as it is. Anything else at the path is not replaced (`EEXIST`).
    pub(in crate::fuse) fn make_partial_file(&self, path: &Path, onedrive_id: &str) -> io::Result<()> {
        match self.make_file(path) {
            Ok(_) => self.mark_with_onedrive_id(path, onedrive_id),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let resolved = self.resolve_path(path);
                let is_file = fs::symlink_metadata(&resolved).is_ok_and(|metadata| metadata.is_file());
                let is_partial_file_of_id = DiskMarks::read(&resolved).is_partial_file() && self.read_onedrive_id(path).as_deref() == Some(onedrive_id);
                if is_file && is_partial_file_of_id { Ok(()) } else { Err(e) }
            }
            Err(e) => Err(e),
        }
    }

    /// Marks the file at `rel` as the whole content of the OneDrive file `id`: its downloaded
    /// copy from here on.
    pub(in crate::fuse) fn mark_downloaded_copy(&self, rel: &Path, id: &str) -> io::Result<()> {
        let path = self.resolve_path(rel);
        xattr::set(&path, XATTR_ITEM_ID, id.as_bytes())?;
        xattr::set(&path, XATTR_STATE, State::Hydrated.as_str().as_bytes())
    }

    /// Takes the marks off the file at `path` if it is marked with the OneDrive id
    /// `onedrive_id` and no other: it is no copy of that file any more, and the user's own
    /// from here on. Whether it was that file's.
    pub(in crate::fuse) fn unmark_downloaded_copy(&self, path: &Path, onedrive_id: &str) -> bool {
        if self.read_onedrive_id(path).as_deref() != Some(onedrive_id) {
            return false;
        }
        let resolved = self.resolve_path(path);
        let _ = xattr::remove(&resolved, XATTR_STATE);
        xattr::remove(&resolved, XATTR_ITEM_ID).is_ok()
    }

    /// Removes the file at `rel` if it is marked with the OneDrive id `id` and no other. Its
    /// size on disk, when it was removed. By the id alone, and not by the one test of a
    /// downloaded copy: a file that is marked with the id and is not whole — a copy whose
    /// marking was interrupted — goes with its OneDrive file too, and would else be left behind
    /// to be shown as a file of the user's own.
    pub(in crate::fuse) fn remove_downloaded_copy(&self, rel: &Path, id: &str) -> Option<u64> {
        let path = self.resolve_path(rel);
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
