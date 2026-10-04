//! Walking the folder's directories: the bring-up, a directory that came
//! into the folder, the map rebuilt. Each directory is `MarkDir`ed and marked
//! before it is listed, so nothing lands in it unseen.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::File;
use std::rc::Rc;
use std::time::Instant;

use super::super::fan::{self, Fid};
use super::tree::{look, subdirs, Found, Gone};
use super::Reader;

/// Why directories are walked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Walk {
    /// The bring-up: every directory is sent to the helper (the helper's
    /// walk may have passed its parent before it was made) and marked; the
    /// Full local scan follows, so nothing is dirtied.
    BringUp,
    /// A directory someone else brought into the folder, or one the map knew
    /// without its mark: `MarkDir`, mark, and the tree is dirty. A directory
    /// the map knows marked only moved.
    Foreign,
    /// One the daemon made or moved (a reconcile). The top one is marked
    /// without `MarkDir` (the reconcile sent it); anything below it may be
    /// someone else's, so it is sent, and the tree is dirty.
    Own,
    /// The map rebuilt from the disk: every directory is visited, and what
    /// the map did not know is treated as [`Walk::Foreign`].
    Again,
}

/// Which directories a walk sends to the helper. "New" is new to the map,
/// or known without its mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AskHelper {
    Every,
    New,
    /// New ones, but not the directory the walk was started for.
    NewBelowTop,
}

/// Which directories a walk looks into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Descend {
    All,
    /// Only the new ones: a directory the map knows marked has its own
    /// events for what is below it.
    New,
}

/// How a walk treats what it finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WalkPolicy {
    ask_helper: AskHelper,
    descend: Descend,
    /// The first new directory of a branch makes its tree dirty: nothing in
    /// it raised events before its mark.
    dirties: bool,
}

impl Walk {
    fn policy(self) -> WalkPolicy {
        match self {
            Walk::BringUp => WalkPolicy { ask_helper: AskHelper::Every, descend: Descend::All, dirties: false },
            Walk::Foreign => WalkPolicy { ask_helper: AskHelper::New, descend: Descend::New, dirties: true },
            Walk::Own => WalkPolicy { ask_helper: AskHelper::NewBelowTop, descend: Descend::New, dirties: true },
            Walk::Again => WalkPolicy { ask_helper: AskHelper::New, descend: Descend::All, dirties: true },
        }
    }
}

/// A directory to visit: `name` in `parent`, which is open as `parent_dir`.
pub(super) struct Item {
    parent_dir: Rc<File>,
    parent: Fid,
    name: OsString,
    depth: usize,
    /// The first new directory of its branch: the tree dirtied is its.
    top: bool,
    /// The directory the walk was started for.
    first: bool,
}

impl Item {
    /// The directory a walk is started for: `name` in `parent`, `depth`
    /// names below the root.
    pub(super) fn first(parent_dir: File, parent: Fid, name: OsString, depth: usize) -> Self {
        Self { parent_dir: Rc::new(parent_dir), parent, name, depth, top: true, first: true }
    }
}

impl Reader {
    /// Every directory beneath the root: the bring-up, or the map rebuilt.
    /// What the map knew and the walk did not find is gone; what the walk
    /// could not look into is kept, and walked again later.
    /// `false` when it could not finish: the folder could not be listed, or
    /// the watcher is stopping.
    pub(super) fn walk_all(&mut self, how: Walk) -> bool {
        self.timers.walking(Instant::now());
        self.tree.drop_deferred();
        let root_key = self.tree.map.root().clone();
        let Ok(root) = self.tree.root() else { return false };
        let root = Rc::new(root);
        let names = match subdirs(&root) {
            Ok(names) => names,
            Err(e) => {
                tracing::warn!("cannot list the folder: {e}");
                self.timers.walk_soon(Instant::now());
                return false;
            }
        };
        let items = names
            .into_iter()
            .map(|name| Item { parent_dir: Rc::clone(&root), parent: root_key.clone(), name, depth: 1, top: true, first: false })
            .collect();
        let mut seen = HashSet::from([root_key]);
        self.visit(items, how, Some(&mut seen));
        if self.shared.stopping() {
            return false;
        }
        for key in self.tree.not_in(&seen) {
            self.forget(&key, Gone::Left);
        }
        true
    }

    /// Visits `stack` and, below each directory the walk looks into (see
    /// [`Descend`]), everything inside it: each directory is `MarkDir`ed and
    /// marked before it is listed. `seen` gathers every directory the walk
    /// found, and what the map has below one it could not look into.
    pub(super) fn visit(&mut self, mut stack: Vec<Item>, how: Walk, mut seen: Option<&mut HashSet<Fid>>) {
        let policy = how.policy();
        self.marks.begin_walk();
        while let Some(item) = stack.pop() {
            if self.shared.stopping() {
                return;
            }
            let (dir, key) = match look(&item.parent_dir, &item.parent, &item.name) {
                Found::Dir(dir, key) => (dir, key),
                Found::Closed(key) => {
                    if let Some(seen) = seen.as_deref_mut() {
                        seen.extend(self.tree.map.subtree(&key));
                        seen.insert(key.clone());
                    }
                    self.tree.place_or_rewalk(&mut self.timers, key, &item.parent, &item.name, false);
                    continue;
                }
                Found::Unknown => {
                    if let (Some(seen), Some(known)) = (seen.as_deref_mut(), self.tree.map.child(&item.parent, &item.name)) {
                        seen.extend(self.tree.map.subtree(&known));
                    }
                    self.timers.walk_soon(Instant::now());
                    continue;
                }
                Found::Nothing => continue,
            };
            if let Some(seen) = seen.as_deref_mut() {
                seen.insert(key.clone());
            }
            if let Some(dev) = self.tree.other_device(&dir) {
                self.marks.elsewhere(&key, &item.name, dev);
                self.tree.place_or_rewalk(&mut self.timers, key, &item.parent, &item.name, false);
                continue;
            }
            let new = self.tree.map.get(&key).is_none_or(|n| !n.marked);
            let ask = match policy.ask_helper {
                AskHelper::Every => true,
                AskHelper::New => new,
                AskHelper::NewBelowTop => new && !item.first,
            };
            if ask {
                self.marks.intercept(&dir, &key);
            }
            let marked = !new || self.marks.watch(&dir, &key, fan::DIR_MASK);
            if !self.tree.place_or_rewalk(&mut self.timers, key.clone(), &item.parent, &item.name, marked) {
                continue;
            }
            if new && item.top && policy.dirties {
                self.dirt.tree(&key);
                self.dirt.touch(Instant::now());
            }
            if !new && policy.descend == Descend::New {
                continue;
            }
            if item.depth >= konedrive_fs::MAX_DEPTH {
                tracing::warn!("a directory deeper than {} levels is not watched", konedrive_fs::MAX_DEPTH);
                continue;
            }
            let names = match subdirs(&dir) {
                Ok(names) => names,
                Err(e) => {
                    tracing::debug!("cannot list a directory to watch: {e}");
                    if let Some(seen) = seen.as_deref_mut() {
                        seen.extend(self.tree.map.subtree(&key));
                    }
                    self.timers.walk_soon(Instant::now());
                    continue;
                }
            };
            let dir = Rc::new(dir);
            for name in names {
                stack.push(Item { parent_dir: Rc::clone(&dir), parent: key.clone(), name, depth: item.depth + 1, top: !new, first: false });
            }
        }
    }
}
