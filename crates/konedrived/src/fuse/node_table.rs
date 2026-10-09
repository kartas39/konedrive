//! The node ids the kernel knows, each an item of the store or an object the folder alone
//! holds, and each node's place.

use std::collections::HashMap;
use std::path::PathBuf;

use konedrive_fs::handle::FileHandle;

/// What a node is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum NodeKey {
    /// An item of the store, by its id.
    InOneDrive(String),
    /// A file or directory OneDrive has no item for yet, by its file handle in the folder
    /// under the mount.
    LocalOnly(FileHandle),
}

/// The node ids the kernel knows. The root is 1.
#[derive(Default)]
pub(super) struct NodeTable {
    pub(super) node_table: HashMap<u64, Node>,
    pub(super) node_ids: HashMap<NodeKey, u64>,
    pub(super) next: u64,
}

pub(super) struct Node {
    pub(super) key: NodeKey,
    /// The node above and this one's name there.
    pub(super) place: (u64, String),
}

impl NodeTable {
    /// The node of `key`, which stands as `name` in `parent`: the one it has, or a new one.
    pub(super) fn assign(&mut self, key: NodeKey, parent: u64, name: &str) -> u64 {
        let place = (parent, name.to_owned());
        if let Some(i) = self.node_ids.get(&key) {
            self.node_table.get_mut(i).expect("a node of every known key").place = place;
            return *i;
        }
        self.next += 1;
        self.node_ids.insert(key.clone(), self.next);
        self.node_table.insert(self.next, Node { key, place });
        self.next
    }

    /// The node `was` is `now` from here on: an object that got its item, which keeps the
    /// node programs hold it by.
    pub(super) fn rekey(&mut self, was: &NodeKey, now: NodeKey) {
        let Some(node_id) = self.node_ids.remove(was) else { return };
        if let Some(node) = self.node_table.get_mut(&node_id) {
            node.key = now.clone();
        }
        self.node_ids.insert(now, node_id);
    }

    /// The node stands as `name` in `parent` now.
    pub(super) fn move_node(&mut self, node_id: u64, parent: u64, name: &str) {
        if let Some(node) = self.node_table.get_mut(&node_id) {
            node.place = (parent, name.to_owned());
        }
    }

    /// The node of `key` is gone, and no other is ever that key: its id and its place are
    /// dropped. The kernel may still name the node; it is answered as gone.
    pub(super) fn forget(&mut self, key: &NodeKey) {
        if let Some(node_id) = self.node_ids.remove(key) {
            self.node_table.remove(&node_id);
        }
    }

    /// The node's path below the folder, from the names the kernel was told.
    pub(super) fn path_of(&self, mut node_id: u64) -> Option<PathBuf> {
        let mut names = Vec::new();
        while node_id != 1 {
            let (parent, name) = &self.node_table.get(&node_id)?.place;
            names.push(name.as_str());
            node_id = *parent;
        }
        Some(names.iter().rev().collect())
    }
}

#[cfg(test)]
mod tests;
