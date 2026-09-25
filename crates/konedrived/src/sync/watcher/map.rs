//! The directory map (`docs/design/writes.md` §3, amended by §3.4, §3.3): for every
//! directory of the folder, its handle → its parent's handle and its name.
//!
//! An unprivileged daemon cannot open a handle (`open_by_handle_at` is
//! `EPERM`, §8.2), so this is how it turns an event's directory handle into a
//! path beneath the root. It is built by the bring-up walk with
//! `name_to_handle_at`, whose handles are byte-equal to the events', and kept
//! current from the `FAN_ONDIR` events. A path is worked out when it is
//! needed, from the parents, so a directory renamed with everything below it
//! is one entry changed.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use super::fan::Fid;

#[derive(Debug)]
pub struct Node {
    pub parent: Option<Fid>,
    pub name: OsString,
    /// Carries the notification mark. False where the mark was refused (an
    /// unreadable directory, a spent mark budget, a subvolume with no group):
    /// nothing inside it raises events.
    pub marked: bool,
    /// By name: one directory per name.
    children: HashMap<OsString, Fid>,
}

/// The map disagrees with itself: a parent it does not know, or a move
/// under one's own subtree. Only a lost event makes it so; a walk mends it.
#[derive(Debug, PartialEq, Eq)]
pub struct Stale;

#[derive(Debug)]
pub struct DirMap {
    root: Fid,
    nodes: HashMap<Fid, Node>,
}

impl DirMap {
    pub fn new(root: Fid, marked: bool) -> Self {
        let node = Node { parent: None, name: OsString::new(), marked, children: HashMap::new() };
        Self { nodes: HashMap::from([(root.clone(), node)]), root }
    }

    pub fn root(&self) -> &Fid {
        &self.root
    }

    pub fn get(&self, key: &Fid) -> Option<&Node> {
        self.nodes.get(key)
    }

    pub fn contains(&self, key: &Fid) -> bool {
        self.nodes.contains_key(key)
    }

    /// Directories known, the root included.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn keys(&self) -> impl Iterator<Item = &Fid> {
        self.nodes.keys()
    }

    /// The directory the map has as `name` in `parent`.
    pub fn child(&self, parent: &Fid, name: &OsStr) -> Option<Fid> {
        self.nodes.get(parent)?.children.get(name).cloned()
    }

    /// `key` is `name` in `parent` now: a new directory, or one that moved.
    /// A directory already known keeps its children, and its mark. Another
    /// directory the map still had at that name is not there any more: it is
    /// taken out with everything below it, and returned.
    pub fn place(&mut self, key: Fid, parent: &Fid, name: &OsStr, marked: bool) -> Result<Vec<Fid>, Stale> {
        if key == self.root || !self.nodes.contains_key(parent) || self.below(parent, &key) {
            return Err(Stale);
        }
        let displaced = match self.child(parent, name) {
            Some(other) if other != key => self.remove(&other),
            _ => Vec::new(),
        };
        let (old_parent, old_name, children, was_marked) = match self.nodes.remove(&key) {
            Some(node) => (node.parent, node.name, node.children, node.marked),
            None => (None, OsString::new(), HashMap::new(), false),
        };
        if let Some(old) = old_parent.and_then(|p| self.nodes.get_mut(&p)) {
            if old.children.get(&old_name) == Some(&key) {
                old.children.remove(&old_name);
            }
        }
        self.nodes.get_mut(parent).expect("checked above").children.insert(name.to_owned(), key.clone());
        let node = Node { parent: Some(parent.clone()), name: name.to_owned(), marked: marked || was_marked, children };
        self.nodes.insert(key, node);
        Ok(displaced)
    }

    pub fn set_marked(&mut self, key: &Fid, marked: bool) {
        if let Some(node) = self.nodes.get_mut(key) {
            node.marked = marked;
        }
    }

    /// Takes `key` and everything below it out of the map (it was deleted,
    /// or left the folder), and returns what went. The root never goes.
    pub fn remove(&mut self, key: &Fid) -> Vec<Fid> {
        if *key == self.root {
            return Vec::new();
        }
        let Some(node) = self.nodes.remove(key) else { return Vec::new() };
        if let Some(parent) = node.parent.as_ref().and_then(|p| self.nodes.get_mut(p)) {
            if parent.children.get(&node.name) == Some(key) {
                parent.children.remove(&node.name);
            }
        }
        let mut gone = vec![key.clone()];
        let mut stack: Vec<Fid> = node.children.into_values().collect();
        while let Some(next) = stack.pop() {
            if let Some(node) = self.nodes.remove(&next) {
                stack.extend(node.children.into_values());
                gone.push(next);
            }
        }
        gone
    }

    /// `key` and everything the map has below it.
    pub fn subtree(&self, key: &Fid) -> Vec<Fid> {
        let mut out = Vec::new();
        let mut stack = vec![key.clone()];
        while let Some(next) = stack.pop() {
            if let Some(node) = self.nodes.get(&next) {
                stack.extend(node.children.values().cloned());
                out.push(next);
            }
        }
        out
    }

    /// Where `key` is, relative to the root (`""` for the root itself).
    /// `None` for a directory the map does not know.
    pub fn path(&self, key: &Fid) -> Option<PathBuf> {
        let mut names = Vec::new();
        let mut at = key;
        // A map can only loop if it is corrupt; `place` refuses to make one.
        for _ in 0..=self.nodes.len() {
            let node = self.nodes.get(at)?;
            match &node.parent {
                None => {
                    return Some(names.iter().rev().collect());
                }
                Some(parent) => {
                    names.push(node.name.as_os_str());
                    at = parent;
                }
            }
        }
        None
    }

    /// Whether `key` is `ancestor` or lies below it.
    fn below(&self, key: &Fid, ancestor: &Fid) -> bool {
        let mut at = Some(key);
        for _ in 0..=self.nodes.len() {
            match at {
                Some(k) if k == ancestor => return true,
                Some(k) => at = self.nodes.get(k).and_then(|n| n.parent.as_ref()),
                None => return false,
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use konedrive_fs::handle::FileHandle;

    use super::*;

    fn fid(n: u8) -> Fid {
        Fid { fsid: [1, 2], handle: FileHandle { kind: 1, bytes: vec![n; 8] } }
    }

    #[test]
    fn paths_follow_renames_and_a_subtree_goes_whole() {
        let mut map = DirMap::new(fid(0), true);
        map.place(fid(1), &fid(0), OsStr::new("a"), true).unwrap();
        map.place(fid(2), &fid(1), OsStr::new("b"), true).unwrap();
        map.place(fid(3), &fid(2), OsStr::new("c"), false).unwrap();
        assert_eq!(map.path(&fid(3)).unwrap(), Path::new("a/b/c"));
        assert_eq!(map.path(&fid(0)).unwrap(), Path::new(""));
        map.place(fid(1), &fid(0), OsStr::new("z"), false).unwrap();
        assert_eq!(map.path(&fid(3)).unwrap(), Path::new("z/b/c"), "a rename moves everything below it");
        assert_eq!(map.child(&fid(0), OsStr::new("a")), None, "not under its old name");
        assert!(map.get(&fid(1)).unwrap().marked, "a moved directory keeps its mark");
        assert_eq!(map.place(fid(1), &fid(3), OsStr::new("x"), true), Err(Stale), "not under itself");
        assert_eq!(map.place(fid(9), &fid(8), OsStr::new("x"), true), Err(Stale), "not under a stranger");
        assert_eq!(map.child(&fid(0), OsStr::new("z")), Some(fid(1)));
        map.place(fid(4), &fid(0), OsStr::new("y"), true).unwrap();
        assert_eq!(map.place(fid(5), &fid(0), OsStr::new("y"), true).unwrap(), vec![fid(4)], "one directory per name");
        assert_eq!(map.subtree(&fid(1)).len(), 3);
        let mut gone = map.remove(&fid(2));
        gone.sort_by_key(|f| f.handle.bytes[0]);
        assert_eq!(gone, vec![fid(2), fid(3)]);
        assert_eq!(map.path(&fid(3)), None);
        assert_eq!(map.len(), 3, "the root, z and y");
        assert!(map.remove(&fid(0)).is_empty(), "the root stays");
    }
}
