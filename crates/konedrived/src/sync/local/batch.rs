//! A batch: the places a quiet spell of events made dirty (write design
//! §3.3, amended by §17).
//!
//! Events are hints. The watcher resolves an event's directory handle
//! to a path through its directory map and adds what it learnt here:
//!
//! - a directory and a name ([`Batch::name`]) for `FAN_CREATE`, `FAN_DELETE`,
//!   each side of `FAN_RENAME`, `FAN_ATTRIB`, `FAN_CLOSE_WRITE`. A name absent
//!   from the disk when examined — a delete, the old side of a rename, the
//!   `#<inode>` pseudo-name of a write through `O_TMPFILE`, or a merged event
//!   whose name moved on — makes the whole directory examined. `"."` is the
//!   directory itself (an event reported through its own mark);
//! - the event's object handle ([`Batch::object`]), which finds the item it
//!   touched through `items.local_handle`, or a pending row through its
//!   handle, wherever the event's name says;
//! - a directory to examine whole ([`Batch::dir`]): a handle the map does not
//!   know resolves to its nearest known parent;
//! - a new directory, with everything below it ([`Batch::tree`]);
//! - a written file ([`Batch::written`], `FAN_CLOSE_WRITE`): its content is
//!   hashed even if its size and time say nothing changed.
//!
//! The order of events is never read from a batch: unread events merge, and
//! only the disk says what happened.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;

/// How much of a directory to examine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirScope {
    Whole,
    Names(BTreeSet<OsString>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Batch {
    pub(super) full: bool,
    pub(super) dirs: BTreeMap<PathBuf, DirScope>,
    pub(super) trees: BTreeSet<PathBuf>,
    pub(super) objects: BTreeSet<FileHandle>,
    pub(super) written_handles: BTreeSet<FileHandle>,
    pub(super) written_rels: BTreeSet<PathBuf>,
}

impl Batch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every directory of the folder: the Full local scan.
    pub fn full() -> Self {
        Self { full: true, ..Self::default() }
    }

    pub fn is_full(&self) -> bool {
        self.full
    }

    pub fn is_empty(&self) -> bool {
        !self.full && self.dirs.is_empty() && self.trees.is_empty() && self.objects.is_empty()
    }

    /// `name` in `dir` (relative to the root) was touched. `"."` is `dir`
    /// itself: its own entry in its parent is examined. The root's own
    /// events are the watcher's (the folder moved or went), never a change.
    pub fn name(&mut self, dir: &Path, name: &OsStr) {
        if name == OsStr::new(".") {
            if let (Some(parent), Some(own)) = (dir.parent(), dir.file_name()) {
                self.name(parent, own);
            }
            return;
        }
        match self.dirs.entry(dir.to_path_buf()).or_insert_with(|| DirScope::Names(BTreeSet::new())) {
            DirScope::Whole => {}
            DirScope::Names(names) => {
                names.insert(name.to_owned());
            }
        }
    }

    /// Examine all of `dir`.
    pub fn dir(&mut self, dir: &Path) {
        self.dirs.insert(dir.to_path_buf(), DirScope::Whole);
    }

    /// `dir` and everything below it: a directory new to the folder, or
    /// moved into it.
    pub fn tree(&mut self, dir: &Path) {
        self.trees.insert(dir.to_path_buf());
    }

    /// An event's object handle.
    pub fn object(&mut self, handle: FileHandle) {
        self.objects.insert(handle);
    }

    /// `FAN_CLOSE_WRITE` on `name` in `dir`, with the object's handle when
    /// the event carried one.
    pub fn written(&mut self, dir: &Path, name: &OsStr, handle: Option<FileHandle>) {
        self.name(dir, name);
        self.written_rels.insert(dir.join(name));
        if let Some(handle) = handle {
            self.objects.insert(handle.clone());
            self.written_handles.insert(handle);
        }
    }

    /// Everything `other` holds, added to this batch.
    pub fn merge(&mut self, other: Batch) {
        self.full |= other.full;
        for (dir, scope) in other.dirs {
            match scope {
                DirScope::Whole => self.dir(&dir),
                DirScope::Names(names) => {
                    for name in names {
                        self.name(&dir, &name);
                    }
                    self.dirs.entry(dir).or_insert_with(|| DirScope::Names(BTreeSet::new()));
                }
            }
        }
        self.trees.extend(other.trees);
        self.objects.extend(other.objects);
        self.written_handles.extend(other.written_handles);
        self.written_rels.extend(other.written_rels);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_gather_per_directory_and_a_whole_directory_wins() {
        let mut batch = Batch::new();
        assert!(batch.is_empty());
        batch.name(Path::new("d"), OsStr::new("a"));
        batch.name(Path::new("d/e"), OsStr::new("."));
        batch.name(Path::new(""), OsStr::new("."));
        assert_eq!(batch.dirs[Path::new("d")], DirScope::Names(["a", "e"].into_iter().map(OsString::from).collect()));
        assert_eq!(batch.dirs.len(), 1, "the root's own event is not a change");
        let mut other = Batch::new();
        other.dir(Path::new("d"));
        batch.merge(other);
        batch.name(Path::new("d"), OsStr::new("b"));
        assert_eq!(batch.dirs[Path::new("d")], DirScope::Whole);
    }
}
