//! The nodes of the mount resolved: which entry of the folder a node is, and which node an
//! entry is.
//!
//! The kernel names what it asks about by node. [`NodeTable`] is the map alone: it knows a key
//! and a place for each node, and nothing of the store or the disk. [`NodeLookup`] is the part
//! over it and the [`DirectoryContents`]: it gives an entry its node, says what a node is now — an item is
//! looked up again in the view, an object without an item by its handle in the folder under
//! the mount — and keeps the node of an object that got its item.
//!
//! The map is shared with what tells the kernel of changes; who changes a node's key or
//! place does it through here.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use konedrive_fs::handle::FileHandle;

use super::node_table::{NodeKey, NodeTable};
use super::directory_contents::{Directory, Entry, Local, DirectoryContents};

pub(super) struct NodeLookup {
    directory_contents: Arc<DirectoryContents>,
    node_table: Arc<Mutex<NodeTable>>,
}

impl NodeLookup {
    pub(super) fn new(directory_contents: Arc<DirectoryContents>, node_table: Arc<Mutex<NodeTable>>) -> Self {
        NodeLookup { directory_contents, node_table }
    }

    /// The node of `entry`, which stands in the directory `parent`. An object that got its
    /// item since the kernel was told of it keeps its node.
    pub(super) fn assign_node_id(&self, entry: &Entry, parent: u64) -> u64 {
        let mut node_table = crate::panic::lock(&self.node_table);
        match entry {
            Entry::Local(local) => node_table.assign(NodeKey::LocalOnly(local.object.handle.clone()), parent, &local.name),
            Entry::Item(row) => {
                let key = NodeKey::InOneDrive(row.id.clone());
                if !node_table.node_ids.contains_key(&key) {
                    let on_disk = node_table.path_of(parent).and_then(|dir| self.directory_contents.backing_directory().find_disk_entry(&dir.join(&row.name)));
                    if let Some(object) = on_disk {
                        node_table.rekey(&NodeKey::LocalOnly(object.handle), key.clone());
                    }
                }
                node_table.assign(key, parent, &row.name)
            }
        }
    }

    /// What the node `ino` is now, and its place. `None` for a node whose item or object is
    /// gone.
    pub(super) fn find_entry(&self, node_id: u64) -> Option<(Entry, PathBuf)> {
        if node_id == 1 {
            return Some((Entry::Item(self.directory_contents.root()), PathBuf::new()));
        }
        let (key, rel, name) = {
            let node_table = crate::panic::lock(&self.node_table);
            let node = node_table.node_table.get(&node_id)?;
            (node.key.clone(), node_table.path_of(node_id)?, node.place.1.clone())
        };
        match key {
            NodeKey::InOneDrive(id) => {
                let row = self.directory_contents.find_placed_row(&id, &name).filter(|_| !self.directory_contents.unsent_changes().gone.contains(&id))?;
                Some((Entry::Item(row), rel))
            }
            NodeKey::LocalOnly(handle) => {
                // Another object at its place is not this node.
                let object = self.directory_contents.backing_directory().find_disk_entry(&rel).filter(|o| o.handle == handle)?;
                // It got its item since: the node is the item's from now on.
                if let Some(row) = self.directory_contents.backing_directory().read_onedrive_id(&rel).and_then(|id| self.directory_contents.find_placed_row(&id, &name)) {
                    crate::panic::lock(&self.node_table).rekey(&NodeKey::LocalOnly(handle), NodeKey::InOneDrive(row.id.clone()));
                    return Some((Entry::Item(row), rel));
                }
                Some((Entry::Local(Local { name, object }), rel))
            }
        }
    }

    /// The directory the node `ino` is.
    pub(super) fn find_directory(&self, node_id: u64) -> Option<Directory> {
        let (entry, rel) = self.find_entry(node_id)?;
        entry.is_dir().then(|| Directory { id: entry.id().filter(|id| !id.is_empty()).map(str::to_owned), rel })
    }

    /// The directory the node `ino` stands in, and that directory's node.
    pub(super) fn find_parent(&self, node_id: u64) -> Option<(u64, Directory)> {
        let parent = crate::panic::lock(&self.node_table).node_table.get(&node_id)?.place.0;
        Some((parent, self.find_directory(parent)?))
    }

    /// The node item `id` is, when the kernel was told of one.
    pub(super) fn find_node_id(&self, id: &str) -> Option<u64> {
        crate::panic::lock(&self.node_table).node_ids.get(&NodeKey::InOneDrive(id.to_owned())).copied()
    }

    /// The node `ino` stands as `name` in `parent` now. `taken`: it is the object that took
    /// over the name of this item, and is the item's node from here on — whoever looks the
    /// name up gets the node the new content is read and written through.
    pub(super) fn move_node(&self, node_id: u64, parent: u64, name: &str, taken: Option<(&FileHandle, &str)>) {
        let mut node_table = crate::panic::lock(&self.node_table);
        node_table.move_node(node_id, parent, name);
        if let Some((object, id)) = taken {
            node_table.rekey(&NodeKey::LocalOnly(object.clone()), NodeKey::InOneDrive(id.to_owned()));
        }
    }

    /// The object `object` is gone, and no other is ever this one: its node is dropped, and
    /// nothing is found by it again. A program that still holds the file open goes on through
    /// its own descriptor, which needs no node; the kernel's later questions about the node
    /// are answered as of something gone, as they were once the object left the disk.
    pub(super) fn forget(&self, object: &FileHandle) {
        crate::panic::lock(&self.node_table).forget(&NodeKey::LocalOnly(object.clone()));
    }
}

#[cfg(test)]
mod tests;
