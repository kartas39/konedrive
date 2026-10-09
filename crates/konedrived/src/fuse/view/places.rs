//! Where the folder shows an item, and what stands at a path of it.

use std::io;
use std::path::{Path, PathBuf};

use konedrive_tree::Placement;

use super::{Dir, Entry, View};
use crate::fuse::overlay::Above;

impl View {
    /// The folder itself, as a directory.
    pub(in crate::fuse) fn root_dir(&self) -> Dir {
        Dir { id: self.root_id(), rel: PathBuf::new() }
    }

    /// Where the folder shows item `id`: where the store has it, or where the user moved it
    /// or a folder above it.
    pub(in crate::fuse) fn place(&self, id: &str) -> Option<PathBuf> {
        let overlay = self.overlay();
        let (mut names, mut at) = (Vec::new(), id.to_owned());
        for _ in 0..konedrive_fs::MAX_DEPTH {
            if overlay.gone.contains(&at) {
                return None;
            }
            let row = self.row(&at).filter(|r| r.placement == Placement::Placed)?;
            let (above, name) = match overlay.moved.get(&at) {
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
    pub(in crate::fuse) fn resolve(&self, rel: &Path) -> Option<Entry> {
        let mut dir = self.root_dir();
        let mut names = rel.components().peekable();
        let mut found = Entry::Item(self.root());
        while let Some(name) = names.next() {
            found = self.entry(&dir, name.as_os_str().to_str()?)?;
            if names.peek().is_some() {
                dir = Dir { id: found.id().map(str::to_owned), rel: dir.rel.join(name) };
                if !found.is_dir() {
                    return None;
                }
            }
        }
        Some(found)
    }

    /// The directories of the folder under the mount down to `rel`, made where they are
    /// missing, each carrying the id of the item this view has there
    /// ([`Under::backing`](crate::fuse::under::Under::backing)).
    pub(in crate::fuse) fn backing(&self, rel: &Path) -> io::Result<()> {
        let item = |id: Option<&str>, dir: &Path, name: &str| self.item_at(&Dir { id: id.map(str::to_owned), rel: dir.to_owned() }, name).map(|row| row.id);
        self.under.backing(rel, self.root_id(), item)
    }
}
