//! The path the mount shows a file or directory of OneDrive at, and the entry at a path.

use std::io;
use std::path::{Path, PathBuf};

use konedrive_tree::Placement;

use super::{DirectoryContents, DirectoryEntry, EntryState};
use crate::fuse::directory::Directory;

impl DirectoryContents {
    /// The folder itself, as a directory.
    pub(in crate::fuse) fn find_root_directory(&self) -> Directory {
        Directory { id: self.find_root_id(), rel: PathBuf::new() }
    }

    /// The path the mount shows the OneDrive id `id` at: where the store has it, or where
    /// the user moved it or a directory it is in. `None` for what the user removed.
    pub(in crate::fuse) fn find_path(&self, id: &str) -> Option<PathBuf> {
        let unsent_changes = self.read_unsent_changes();
        let (mut names, mut at) = (Vec::new(), id.to_owned());
        for _ in 0..konedrive_fs::MAX_DEPTH {
            if unsent_changes.removed.contains(&at) {
                return None;
            }
            let row = self.find_row(&at).filter(|r| r.placement == Placement::Placed)?;
            let (parent_id, name) = match unsent_changes.moved.get(&at) {
                // Moved into a directory that is not in OneDrive yet: found by its path.
                Some((Directory { id: None, rel }, name)) => {
                    names.push(name.clone());
                    return Some(rel.join(names.iter().rev().collect::<PathBuf>()));
                }
                Some((Directory { id: Some(parent_id), .. }, name)) => (Some(parent_id.clone()), name.clone()),
                None => (row.parent_id.clone(), row.name.clone()),
            };
            match parent_id {
                // The drive's root: the folder itself.
                None => return Some(names.iter().rev().collect()),
                Some(parent_id) => {
                    names.push(name);
                    at = parent_id;
                }
            }
        }
        None
    }

    /// The entry at `rel`, found name by name from the root.
    pub(in crate::fuse) fn find_entry_at(&self, rel: &Path) -> Option<DirectoryEntry> {
        let mut directory = self.find_root_directory();
        let mut names = rel.components().peekable();
        let mut found = self.find_root_entry();
        while let Some(name) = names.next() {
            found = self.find_entry(&directory, name.as_os_str().to_str()?)?;
            if names.peek().is_some() {
                if !matches!(found.state, EntryState::Directory(_)) {
                    return None;
                }
                directory = Directory { id: found.onedrive_id().map(str::to_owned), rel: directory.rel.join(name) };
            }
        }
        Some(found)
    }

    /// Makes the directories of the backing directory down to `rel` where they are missing,
    /// each marked with the OneDrive id of the directory the mount shows there
    /// ([`BackingDirectory::make_marked_directories`](crate::fuse::backing_directory::BackingDirectory::make_marked_directories)).
    pub(in crate::fuse) fn make_marked_directories(&self, rel: &Path) -> io::Result<()> {
        let id_of = |parent_id: Option<&str>, parent: &Path, name: &str| self.find_row_at(&Directory { id: parent_id.map(str::to_owned), rel: parent.to_owned() }, name).map(|row| row.id);
        self.backing_directory.make_marked_directories(rel, self.find_root_id(), id_of)
    }
}
