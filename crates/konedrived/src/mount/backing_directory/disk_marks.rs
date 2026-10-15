//! The marks an entry of the backing directory carries: which file or directory of OneDrive it
//! belongs to, and whether it is that file's downloaded copy.
//!
//! A downloaded copy is a regular file that is marked with its OneDrive id and marked whole.
//! Any other file at the path of a OneDrive file is not its copy — one the user put there
//! while nothing was mounted, the copy of another OneDrive file, a placeholder an older
//! konedrive left — and is never shown as its content, and never removed as its copy.

use std::fs::File;
use std::io;
use std::path::Path;

use konedrive_fs::placeholder::{self, State, XATTR_CTAG, XATTR_ITEM_ID, XATTR_STATE};

use super::BackingDirectory;

/// The marks a file or directory on disk carries: the OneDrive id it is marked with, and the
/// state it is marked with, each as it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::mount) struct DiskMarks {
    onedrive_id: Option<Vec<u8>>,
    state: Option<Vec<u8>>,
}

impl DiskMarks {
    /// The marks of whatever is at `path`, a path of the filesystem: of the backing
    /// directory through its descriptor, or of a folder no mount lies over yet.
    pub(in crate::mount) fn read(path: &Path) -> Self {
        DiskMarks { onedrive_id: xattr::get(path, XATTR_ITEM_ID).ok().flatten(), state: xattr::get(path, XATTR_STATE).ok().flatten() }
    }

    /// [`read`](Self::read), which says so when the marks could not be read: only an
    /// attribute that is not there is no mark. Whatever else the filesystem answers — it
    /// denies the reading, as it does of what the user may not read, or fails at it — says
    /// nothing of the entry: it is not unmarked, its marks are not known.
    pub(in crate::mount) fn try_read(path: &Path) -> std::io::Result<Self> {
        Ok(DiskMarks { onedrive_id: xattr::get(path, XATTR_ITEM_ID)?, state: xattr::get(path, XATTR_STATE)? })
    }

    /// The OneDrive id it is marked with.
    pub(in crate::mount) fn onedrive_id(&self) -> Option<String> {
        self.onedrive_id.as_deref().map(|id| String::from_utf8_lossy(id).into_owned())
    }

    /// Whether it is marked whole.
    pub(in crate::mount) fn is_whole(&self) -> bool {
        self.state.as_deref() == Some(State::Hydrated.as_str().as_bytes())
    }

    /// The one test of a downloaded copy: marked with the OneDrive id `onedrive_id`, and
    /// marked whole. The start-up check goes by it.
    pub(in crate::mount) fn is_downloaded_copy_of(&self, onedrive_id: &str) -> bool {
        self.onedrive_id.as_deref() == Some(onedrive_id.as_bytes()) && self.is_whole()
    }

    /// Whether it is marked with a state that is not whole, whatever its OneDrive id: what
    /// an older konedrive's placeholder carries.
    pub(in crate::mount) fn is_legacy_placeholder(&self) -> bool {
        self.state.is_some() && !self.is_whole()
    }
}

impl BackingDirectory {
    /// The OneDrive id the entry at `rel` is marked with.
    pub(in crate::mount) fn read_onedrive_id(&self, rel: &Path) -> Option<String> {
        self.read_attribute(rel, XATTR_ITEM_ID)
    }

    /// Marks the entry at `rel` with the OneDrive id `id` and nothing else: it belongs to
    /// that file or directory of OneDrive from here on, and is no whole copy of what OneDrive
    /// has of it. Also an entry that has no write bit: the bit is lifted for the call.
    pub(in crate::mount) fn mark_with_onedrive_id(&self, rel: &Path, id: &str) -> io::Result<()> {
        let entry = File::open(self.resolve_path(rel))?;
        placeholder::with_owner_write(&entry, || xattr::FileExt::set_xattr(&entry, XATTR_ITEM_ID, id.as_bytes()))
    }

    /// Takes the OneDrive id off the entry at `rel`: it is the user's own from here on.
    pub(in crate::mount) fn clear_onedrive_id(&self, rel: &Path) {
        let _ = xattr::remove(self.resolve_path(rel), XATTR_ITEM_ID);
    }

    /// Marks the file at `rel` as the whole content of the OneDrive file `id`: its downloaded
    /// copy from here on.
    pub(in crate::mount) fn mark_downloaded_copy(&self, rel: &Path, id: &str) -> io::Result<()> {
        let path = self.resolve_path(rel);
        xattr::set(&path, XATTR_ITEM_ID, id.as_bytes())?;
        xattr::set(&path, XATTR_STATE, State::Hydrated.as_str().as_bytes())
    }

    /// The cTag the file at `rel` is marked with: the version of the OneDrive file its
    /// downloaded copy holds. Only an attribute that is not there is no mark; whatever else
    /// the filesystem answers is an error.
    pub(in crate::mount) fn read_ctag_mark(&self, rel: &Path) -> io::Result<Option<String>> {
        Ok(xattr::get(self.resolve_path(rel), XATTR_CTAG)?.map(|ctag| String::from_utf8_lossy(&ctag).into_owned()))
    }

    /// Takes off the entry at `rel` the marks that say which item of OneDrive it is and
    /// which content of it: the OneDrive id first, then the state and the cTag. It is the
    /// user's own from here on. What else konedrive keeps on it stays, its pin among it.
    /// Also off a downloaded copy, which has no write bit: the bit is lifted for the call.
    pub(in crate::mount) fn remove_onedrive_marks(&self, rel: &Path) -> io::Result<()> {
        let entry = File::open(self.resolve_path(rel))?;
        placeholder::with_owner_write(&entry, || {
            for mark in [XATTR_ITEM_ID, XATTR_STATE, XATTR_CTAG] {
                match xattr::FileExt::remove_xattr(&entry, mark) {
                    Ok(()) => {}
                    Err(e) if e.raw_os_error() == Some(libc::ENODATA) => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;
