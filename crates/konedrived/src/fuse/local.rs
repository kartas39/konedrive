//! The nodes of the mount resolved: which entry of the folder a node is now.

use std::path::PathBuf;

use super::nodes::Key;
use super::shared::Shared;
use super::view::{Dir, Entry, Local};

impl Shared {
    /// The node of `entry`, which stands in the directory `parent`. An object that got its
    /// item since the kernel was told of it keeps its node.
    pub(super) fn ino_of(&self, entry: &Entry, parent: u64) -> u64 {
        let mut nodes = crate::panic::lock(&self.nodes);
        match entry {
            Entry::Local(local) => nodes.ino(Key::Local(local.object.handle.clone()), parent, &local.name),
            Entry::Item(row) => {
                let key = Key::Item(row.id.clone());
                if !nodes.inos.contains_key(&key) {
                    let on_disk = nodes.rel(parent).and_then(|dir| self.view.under().object(&dir.join(&row.name)));
                    if let Some(object) = on_disk {
                        nodes.rekey(&Key::Local(object.handle), key.clone());
                    }
                }
                nodes.ino(key, parent, &row.name)
            }
        }
    }

    /// What the node `ino` is now, and its place. `None` for a node whose item or object is
    /// gone.
    pub(super) fn node(&self, ino: u64) -> Option<(Entry, PathBuf)> {
        if ino == 1 {
            return Some((Entry::Item(self.view.root()), PathBuf::new()));
        }
        let (key, rel, name) = {
            let nodes = crate::panic::lock(&self.nodes);
            let node = nodes.items.get(&ino)?;
            (node.key.clone(), nodes.rel(ino)?, node.place.1.clone())
        };
        match key {
            Key::Item(id) => {
                let row = self.view.shown_as(&id, &name).filter(|_| !self.view.overlay().gone.contains(&id))?;
                Some((Entry::Item(row), rel))
            }
            Key::Local(handle) => {
                // Another object at its place is not this node.
                let object = self.view.under().object(&rel).filter(|o| o.handle == handle)?;
                // It got its item since: the node is the item's from now on.
                if let Some(row) = self.view.under().carried(&rel).and_then(|id| self.view.shown_as(&id, &name)) {
                    crate::panic::lock(&self.nodes).rekey(&Key::Local(handle), Key::Item(row.id.clone()));
                    return Some((Entry::Item(row), rel));
                }
                Some((Entry::Local(Local { name, object }), rel))
            }
        }
    }

    /// The directory the node `ino` is.
    pub(super) fn dir(&self, ino: u64) -> Option<Dir> {
        let (entry, rel) = self.node(ino)?;
        entry.is_dir().then(|| Dir { id: entry.id().filter(|id| !id.is_empty()).map(str::to_owned), rel })
    }

    /// The directory the node `ino` stands in, and that directory's node.
    pub(super) fn above(&self, ino: u64) -> Option<(u64, Dir)> {
        let parent = crate::panic::lock(&self.nodes).items.get(&ino)?.place.0;
        Some((parent, self.dir(parent)?))
    }
}
