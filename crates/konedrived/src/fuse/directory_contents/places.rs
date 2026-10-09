//! Where the folder shows an item, and what stands at a path of it.

use std::io;
use std::path::{Path, PathBuf};

use konedrive_tree::Placement;

use super::{Directory, Entry, DirectoryContents};
use crate::fuse::unsent_changes::Above;

impl DirectoryContents {
    /// The folder itself, as a directory.
    pub(in crate::fuse) fn root_directory(&self) -> Directory {
        Directory { id: self.root_id(), rel: PathBuf::new() }
    }

    /// Where the folder shows item `id`: where the store has it, or where the user moved it
    /// or a folder above it.
    pub(in crate::fuse) fn find_path(&self, id: &str) -> Option<PathBuf> {
        let unsent_changes = self.unsent_changes();
        let (mut names, mut at) = (Vec::new(), id.to_owned());
        for _ in 0..konedrive_fs::MAX_DEPTH {
            if unsent_changes.gone.contains(&at) {
                return None;
            }
            let row = self.row(&at).filter(|r| r.placement == Placement::Placed)?;
            let (above, name) = match unsent_changes.moved.get(&at) {
                Some((above, name)) => (Some(above.clone()), name.clone()),
                None => (row.parent_id.clone().map(Above::Item), row.name.clone()),
            };
            match above {
                // The drive's root: the folder itself.
                None => return Some(names.iter().rev().collect()),
                Some(Above::Local(dir)) => {
                    names.push(name);
                    return Some(dir.join(names.iter().rev().collect::<PathBuf>()));
                }
                Some(Above::Item(parent)) => {
                    names.push(name);
                    at = parent;
                }
            }
        }
        None
    }

    /// What stands at `rel`, name by name from the root.
    pub(in crate::fuse) fn find_entry_at(&self, rel: &Path) -> Option<Entry> {
        let mut dir = self.root_directory();
        let mut names = rel.components().peekable();
        let mut found = Entry::Item(self.root());
        while let Some(name) = names.next() {
            found = self.find_entry(&dir, name.as_os_str().to_str()?)?;
            if names.peek().is_some() {
                dir = Directory { id: found.id().map(str::to_owned), rel: dir.rel.join(name) };
                if !found.is_dir() {
                    return None;
                }
            }
        }
        Some(found)
    }

    /// The directories of the folder under the mount down to `rel`, made where they are
    /// missing, each carrying the id of the item this view has there
    /// ([`BackingDirectory::backing`](crate::fuse::backing_directory::BackingDirectory::backing)).
    pub(in crate::fuse) fn make_marked_directories(&self, rel: &Path) -> io::Result<()> {
        let item = |id: Option<&str>, dir: &Path, name: &str| self.find_row_at(&Directory { id: id.map(str::to_owned), rel: dir.to_owned() }, name).map(|row| row.id);
        self.backing_directory.make_marked_directories(rel, self.root_id(), item)
    }
}
