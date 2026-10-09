//! The node ids given to the kernel: for each, what it stands for — a file or directory of
//! OneDrive, or a local-only entry — and where it is.
//!
//! A node id is the number the mount gives the kernel for a name; the kernel calls it an
//! inode number (`ino`). The root of the mount is node 1.

use std::collections::HashMap;
use std::path::PathBuf;

use konedrive_fs::handle::FileHandle;

/// What a node id stands for.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum NodeKey {
    /// A file or directory of OneDrive, by its OneDrive id.
    InOneDrive(String),
    /// A file or directory that is not in OneDrive yet, by its file handle in the backing
    /// directory.
    LocalOnly(FileHandle),
}

/// The node ids given to the kernel.
pub(super) struct NodeTable {
    pub(super) nodes: HashMap<u64, Node>,
    pub(super) node_ids: HashMap<NodeKey, u64>,
    /// The node id given last. The root is 1 and is never in the table.
    last_node_id: u64,
}

pub(super) struct Node {
    pub(super) key: NodeKey,
    /// The node id of the directory this one is in, and its name there.
    pub(super) place: (u64, String),
}

impl NodeTable {
    /// A table that holds no node yet: the root alone is known.
    pub(super) fn new() -> Self {
        NodeTable { nodes: HashMap::new(), node_ids: HashMap::new(), last_node_id: 1 }
    }

    /// The node id of `key`, which is `name` in the directory node `parent`: the one it has,
    /// or a new one.
    pub(super) fn assign(&mut self, key: NodeKey, parent: u64, name: &str) -> u64 {
        let place = (parent, name.to_owned());
        if let Some(node_id) = self.node_ids.get(&key) {
            self.nodes.get_mut(node_id).expect("a node of every known key").place = place;
            return *node_id;
        }
        self.last_node_id += 1;
        self.node_ids.insert(key.clone(), self.last_node_id);
        self.nodes.insert(self.last_node_id, Node { key, place });
        self.last_node_id
    }

    /// The node of `was` stands for `now` from here on: a local-only entry that is in
    /// OneDrive now keeps the node id programs hold it by.
    pub(super) fn rekey(&mut self, was: &NodeKey, now: NodeKey) {
        let Some(node_id) = self.node_ids.remove(was) else { return };
        if let Some(node) = self.nodes.get_mut(&node_id) {
            node.key = now.clone();
        }
        self.node_ids.insert(now, node_id);
    }

    /// The node `node_id` is `name` in the directory node `parent` now.
    pub(super) fn move_node(&mut self, node_id: u64, parent: u64, name: &str) {
        if let Some(node) = self.nodes.get_mut(&node_id) {
            node.place = (parent, name.to_owned());
        }
    }

    /// What `key` stood for is gone, and nothing else is ever that key: its node id and its
    /// place are dropped. The kernel may still name the node id; it is answered as gone.
    pub(super) fn forget(&mut self, key: &NodeKey) {
        if let Some(node_id) = self.node_ids.remove(key) {
            self.nodes.remove(&node_id);
        }
    }

    /// The path of the node `node_id` in the folder, from the names the kernel was told.
    pub(super) fn path_of(&self, mut node_id: u64) -> Option<PathBuf> {
        let mut names = Vec::new();
        while node_id != 1 {
            let (parent, name) = &self.nodes.get(&node_id)?.place;
            names.push(name.as_str());
            node_id = *parent;
        }
        Some(names.iter().rev().collect())
    }
}

#[cfg(test)]
mod tests;
