//! The place of an entry in the folder: a name in a directory.

use std::path::PathBuf;

use super::directory_reference::DirectoryReference;

/// A place in the folder: a name in a directory, which is referred to by its OneDrive id
/// when it has one, and by its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EntryPlace {
    pub(super) directory: DirectoryReference,
    pub(super) name: String,
}

impl EntryPlace {
    /// `name` in `directory`.
    pub(super) fn new(directory: &DirectoryReference, name: &str) -> Self {
        EntryPlace { directory: directory.clone(), name: name.to_owned() }
    }

    /// The path of the place in the folder.
    pub(super) fn path(&self) -> PathBuf {
        self.directory.path.join(&self.name)
    }
}
