//! A reference to a directory of the mount.

use std::path::PathBuf;

/// A directory of the mount: its OneDrive id, when OneDrive has it, and its path in the
/// folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Directory {
    pub(super) id: Option<String>,
    pub(super) rel: PathBuf,
}

impl Directory {
    /// Whether `reference` refers to this directory: by its OneDrive id when the reference
    /// has one, and by its path alone when it has none — the directory may have got its id
    /// since the reference was written.
    pub(super) fn matches(&self, reference: &Directory) -> bool {
        match &reference.id {
            Some(id) => self.id.as_deref() == Some(id),
            None => reference.rel == self.rel,
        }
    }
}
