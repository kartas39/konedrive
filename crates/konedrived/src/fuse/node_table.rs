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

/// The node ids given to the kernel. Only the table itself reads or changes its maps: whoever
/// needs a node id, a record or a key asks one of its functions.
pub(super) struct NodeTable {
    records: HashMap<u64, NodeRecord>,
    node_ids: HashMap<NodeKey, u64>,
    /// The node id given last. The root is 1 and is never in the table.
    last_node_id: u64,
}

/// What the table knows of one node id: what it stands for, and where the kernel was told
/// it is.
pub(super) struct NodeRecord {
    pub(super) key: NodeKey,
    /// The node id of the directory this one is in.
    pub(super) parent_node_id: u64,
    /// Its name in that directory.
    pub(super) name: String,
}

impl NodeTable {
    /// A table that holds no node yet: the root alone is known.
    pub(super) fn new() -> Self {
        NodeTable { records: HashMap::new(), node_ids: HashMap::new(), last_node_id: 1 }
    }

    /// The node id of `key`, which is `name` in the directory node `parent_node_id`: the one
    /// it has, or a new one.
    pub(super) fn assign(&mut self, key: NodeKey, parent_node_id: u64, name: &str) -> u64 {
        if let Some(node_id) = self.node_ids.get(&key) {
            let record = self.records.get_mut(node_id).expect("a record of every known key");
            (record.parent_node_id, record.name) = (parent_node_id, name.to_owned());
            return *node_id;
        }
        self.last_node_id += 1;
        self.node_ids.insert(key.clone(), self.last_node_id);
        self.records.insert(self.last_node_id, NodeRecord { key, parent_node_id, name: name.to_owned() });
        self.last_node_id
    }

    /// Whether the kernel was given a node id for `key`.
    pub(super) fn has_key(&self, key: &NodeKey) -> bool {
        self.node_ids.contains_key(key)
    }

    /// The node id that stands for `key`, when the kernel was given one.
    pub(super) fn find_node_id(&self, key: &NodeKey) -> Option<u64> {
        self.node_ids.get(key).copied()
    }

    /// What the table knows of the node `node_id`.
    pub(super) fn find_record(&self, node_id: u64) -> Option<&NodeRecord> {
        self.records.get(&node_id)
    }

    /// The node of `was` stands for `now` from here on: a local-only entry that is in
    /// OneDrive now keeps the node id programs hold it by.
    pub(super) fn rekey(&mut self, was: &NodeKey, now: NodeKey) {
        let Some(node_id) = self.node_ids.remove(was) else { return };
        if let Some(record) = self.records.get_mut(&node_id) {
            record.key = now.clone();
        }
        self.node_ids.insert(now, node_id);
    }

    /// The node `node_id` is `name` in the directory node `parent_node_id` now.
    pub(super) fn move_node(&mut self, node_id: u64, parent_node_id: u64, name: &str) {
        if let Some(record) = self.records.get_mut(&node_id) {
            (record.parent_node_id, record.name) = (parent_node_id, name.to_owned());
        }
    }

    /// What `key` stood for is gone, and nothing else is ever that key: its node id and its
    /// record are dropped. The kernel may still name the node id; it is answered as gone.
    pub(super) fn drop_node(&mut self, key: &NodeKey) {
        if let Some(node_id) = self.node_ids.remove(key) {
            self.records.remove(&node_id);
        }
    }

    /// The path of the node `node_id` in the folder, from the names the kernel was told.
    pub(super) fn find_path(&self, mut node_id: u64) -> Option<PathBuf> {
        let mut names = Vec::new();
        while node_id != 1 {
            let record = self.records.get(&node_id)?;
            names.push(record.name.as_str());
            node_id = record.parent_node_id;
        }
        Some(names.iter().rev().collect())
    }
}

#[cfg(test)]
mod tests;
