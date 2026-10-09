//! The node ids the kernel knows, each an item of the store or an object the folder alone
//! holds, and each node's place.

use std::collections::HashMap;
use std::path::PathBuf;

use konedrive_fs::handle::FileHandle;

/// What a node is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum Key {
    /// An item of the store, by its id.
    Item(String),
    /// A file or directory OneDrive has no item for yet, by its file handle in the folder
    /// under the mount.
    Local(FileHandle),
}

/// The node ids the kernel knows. The root is 1.
#[derive(Default)]
pub(super) struct Nodes {
    pub(super) items: HashMap<u64, Node>,
    pub(super) inos: HashMap<Key, u64>,
    pub(super) next: u64,
}

pub(super) struct Node {
    pub(super) key: Key,
    /// The node above and this one's name there.
    pub(super) place: (u64, String),
}

impl Nodes {
    /// The node of `key`, which stands as `name` in `parent`: the one it has, or a new one.
    pub(super) fn ino(&mut self, key: Key, parent: u64, name: &str) -> u64 {
        let place = (parent, name.to_owned());
        if let Some(i) = self.inos.get(&key) {
            self.items.get_mut(i).expect("a node of every known key").place = place;
            return *i;
        }
        self.next += 1;
        self.inos.insert(key.clone(), self.next);
        self.items.insert(self.next, Node { key, place });
        self.next
    }

    /// The node `was` is `now` from here on: an object that got its item, which keeps the
    /// node programs hold it by.
    pub(super) fn rekey(&mut self, was: &Key, now: Key) {
        let Some(ino) = self.inos.remove(was) else { return };
        if let Some(node) = self.items.get_mut(&ino) {
            node.key = now.clone();
        }
        self.inos.insert(now, ino);
    }

    /// The node stands as `name` in `parent` now.
    pub(super) fn moved(&mut self, ino: u64, parent: u64, name: &str) {
        if let Some(node) = self.items.get_mut(&ino) {
            node.place = (parent, name.to_owned());
        }
    }

    /// The node of `key` is gone, and no other is ever that key: its id and its place are
    /// dropped. The kernel may still name the node; it is answered as gone.
    pub(super) fn forget(&mut self, key: &Key) {
        if let Some(ino) = self.inos.remove(key) {
            self.items.remove(&ino);
        }
    }

    /// The node's path below the folder, from the names the kernel was told.
    pub(super) fn rel(&self, mut ino: u64) -> Option<PathBuf> {
        let mut names = Vec::new();
        while ino != 1 {
            let (parent, name) = &self.items.get(&ino)?.place;
            names.push(name.as_str());
            ino = *parent;
        }
        Some(names.iter().rev().collect())
    }
}

#[cfg(test)]
mod tests;
