//! The node ids the kernel knows, each an item of the store, and each node's place.

use std::collections::HashMap;
use std::path::PathBuf;

use konedrive_tree::Row;

/// The node ids the kernel knows, each an item of the store. The root is 1.
#[derive(Default)]
pub(super) struct Nodes {
    pub(super) items: HashMap<u64, Node>,
    pub(super) inos: HashMap<String, u64>,
    pub(super) next: u64,
}

pub(super) struct Node {
    pub(super) id: String,
    /// The node above and this one's name there.
    pub(super) place: (u64, String),
}

impl Nodes {
    pub(super) fn ino(&mut self, row: &Row, parent: u64) -> u64 {
        let place = (parent, row.name.clone());
        if let Some(i) = self.inos.get(&row.id) {
            self.items.get_mut(i).expect("a node of every known id").place = place;
            return *i;
        }
        self.next += 1;
        self.inos.insert(row.id.clone(), self.next);
        self.items.insert(self.next, Node { id: row.id.clone(), place });
        self.next
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
