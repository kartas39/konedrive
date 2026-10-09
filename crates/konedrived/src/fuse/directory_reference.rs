//! A reference to a directory of the mount.

use std::path::PathBuf;

/// A reference to a directory of the mount: its OneDrive id, when OneDrive has it, and its
/// path in the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DirectoryReference {
    pub(super) onedrive_id: Option<String>,
    pub(super) path: PathBuf,
}

impl DirectoryReference {
    /// Whether `reference` refers to this directory: by its OneDrive id when the reference
    /// has one, and by its path alone when it has none — the directory may have got its id
    /// since the reference was written.
    pub(super) fn matches(&self, reference: &DirectoryReference) -> bool {
        match &reference.onedrive_id {
            Some(onedrive_id) => self.onedrive_id.as_deref() == Some(onedrive_id),
            None => reference.path == self.path,
        }
    }
}

#[cfg(test)]
mod tests;
