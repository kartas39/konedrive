//! The folder's directories as the reader knows them: the root, the
//! directory map, the directories that left the folder still carrying a mark,
//! and the directory events that could not be settled yet.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::path::Path;

use konedrive_fs::handle::FileHandle;
use nix::fcntl::{openat2, OFlag, OpenHow};

use super::super::fan::Fid;
use super::super::map::DirMap;
use super::timers::Timers;
use crate::folder::disk::{daemon_owned, gone, open_subdir};

/// Directories that left the folder, remembered so that their events are
/// passed over. Past this many the memory starts again: an event from a
/// forgotten one costs a walk at most.
const LEFT_CAP: usize = 65_536;

/// Who made the change a directory event tells of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum By {
    /// The daemon itself (a reconcile): the event carries its pid.
    Daemon,
    Other,
}

/// What became of a directory the map has at a name and the disk has not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Gone {
    /// Deleted: its mark went with it.
    Deleted,
    /// Somewhere unwatched (out of the folder), still carrying the mark:
    /// remembered, so that its events are passed over.
    Left,
}

/// How a directory event is settled: who made the change, and what a
/// directory that is no longer at the name is taken for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Settled {
    pub by: By,
    pub gone: Gone,
}

/// An `ONDIR` record whose directory could not be opened where the map
/// says: settled again once the queue is drained and the map has caught up.
pub(super) struct Pending {
    pub parent: Fid,
    pub name: OsString,
    pub how: Settled,
}

/// What stands at a name, as far as the watcher can tell.
pub(super) enum Found {
    /// A directory, open.
    Dir(File, Fid),
    /// A directory this daemon may not open: known by its handle (taken by
    /// name), not marked, and nothing in it examined either.
    Closed(Fid),
    /// Something the watcher cannot look at now (the parent lost its search
    /// bit meanwhile, say): what the map has there stays.
    Unknown,
    Nothing,
}

pub(super) struct Tree {
    root: File,
    root_dev: u64,
    pub map: DirMap,
    /// Directories that left the folder still carrying this group's mark
    /// (only the kernel takes it off, when they go).
    left: HashSet<Fid>,
    deferred: Vec<Pending>,
}

fn dev_of(file: &File) -> Option<u64> {
    nix::sys::stat::fstat(file).ok().map(|st| st.st_dev)
}

/// The subdirectories of `dir`, by name. The type comes from the directory
/// entry, or from `lstat` where the filesystem gives none; a symlink is never
/// a directory here.
pub(super) fn subdirs(dir: &File) -> io::Result<Vec<OsString>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(konedrive_fs::proc_path(dir))? {
        let entry = entry?;
        if entry.file_type().is_ok_and(|t| t.is_dir()) && !daemon_owned(&entry.file_name()) {
            out.push(entry.file_name());
        }
    }
    Ok(out)
}

/// What stands at `name` in `parent`, which is open as `parent_dir`.
pub(super) fn look(parent_dir: &File, parent: &Fid, name: &OsStr) -> Found {
    match open_subdir(parent_dir, name) {
        Ok(dir) => match Fid::of(&dir) {
            Ok(key) => Found::Dir(dir, key),
            Err(e) => {
                tracing::warn!("{} gives no file handle, so it cannot be watched: {e}", Path::new(name).display());
                Found::Nothing
            }
        },
        Err(e) if gone(&e) => Found::Nothing,
        Err(e) => match FileHandle::at(parent_dir, name) {
            Ok(handle) => {
                tracing::debug!("{} cannot be opened to be watched: {e}", Path::new(name).display());
                Found::Closed(Fid { fsid: parent.fsid, handle })
            }
            Err(e) if gone(&e) => Found::Nothing,
            Err(_) => Found::Unknown,
        },
    }
}

impl Tree {
    /// The tree of `root`, with only the root in its map, not marked yet.
    pub(super) fn new(root: File) -> io::Result<Self> {
        let key = Fid::of(&root)?;
        let root_dev = dev_of(&root).ok_or_else(|| io::Error::other("cannot stat the folder"))?;
        Ok(Self { map: DirMap::new(key, false), root, root_dev, left: HashSet::new(), deferred: Vec::new() })
    }

    /// The root, open once more.
    pub(super) fn root(&self) -> io::Result<File> {
        self.root.try_clone()
    }

    /// Whether `dir` is on another device than the folder's, and which.
    pub(super) fn other_device(&self, dir: &File) -> Option<u64> {
        dev_of(dir).filter(|dev| *dev != self.root_dev)
    }

    /// Whether a directory event names something the watcher follows: not
    /// one of the daemon's own directories, and in a directory the map has.
    pub(super) fn follows(&self, parent: &Fid, name: &OsStr) -> bool {
        !daemon_owned(name) && self.map.contains(parent)
    }

    /// The directory the map knows as `key`, opened beneath the root where
    /// the map says it is, and proved to be `key` by its handle.
    pub(super) fn open_known(&self, key: &Fid) -> Option<File> {
        let path = self.map.path(key)?;
        let dir = if path.as_os_str().is_empty() {
            self.root.try_clone().ok()?
        } else {
            let how = OpenHow::new().flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC).resolve(crate::folder::disk::beneath());
            File::from(openat2(self.root.as_fd(), path.as_path(), how).ok()?)
        };
        (Fid::of(&dir).ok()? == *key).then_some(dir)
    }

    /// How many names lie between the root and `key`; 0 for the root and for
    /// a directory the map does not have.
    pub(super) fn depth(&self, key: &Fid) -> usize {
        self.map.path(key).map_or(0, |p| p.components().count())
    }

    /// `key` is `name` in `parent`, `marked` or not. Whatever the map had
    /// there instead has left. `false` when the map is out of step with the
    /// disk (the place is below `key` itself, say): the folder is walked
    /// again, and the directory is not placed.
    pub(super) fn place_or_rewalk(&mut self, timers: &mut Timers, key: Fid, parent: &Fid, name: &OsStr, marked: bool) -> bool {
        match self.map.place(key.clone(), parent, name, marked) {
            Ok(displaced) => {
                self.leave(displaced);
                // It is in the folder (again): its events are the folder's.
                self.left.remove(&key);
                true
            }
            Err(_) => {
                tracing::debug!("the directory map is out of step; it is walked again");
                timers.lost();
                false
            }
        }
    }

    /// `key` and everything below it are not in the folder any more. What
    /// went, for whoever keeps something by directory.
    pub(super) fn forget(&mut self, key: &Fid, how: Gone) -> Vec<Fid> {
        let gone = self.map.remove(key);
        if how == Gone::Left {
            self.leave(gone.clone());
        }
        gone
    }

    fn leave(&mut self, keys: Vec<Fid>) {
        if self.left.len() + keys.len() > LEFT_CAP {
            self.left.clear();
        }
        self.left.extend(keys);
    }

    /// `dir` left the folder still carrying a mark: its events are not the
    /// folder's.
    pub(super) fn has_left(&self, dir: &Fid) -> bool {
        self.left.contains(dir)
    }

    /// What the map has and `seen` has not: gone since the map was built.
    pub(super) fn not_in(&self, seen: &HashSet<Fid>) -> Vec<Fid> {
        self.map.keys().filter(|k| !seen.contains(*k)).cloned().collect()
    }

    /// A directory event to settle again once the queue is drained.
    pub(super) fn defer(&mut self, pending: Pending) {
        self.deferred.push(pending);
    }

    /// The directory events put off.
    pub(super) fn take_deferred(&mut self) -> Vec<Pending> {
        std::mem::take(&mut self.deferred)
    }

    /// A walk of the whole folder settles everything: nothing is put off.
    pub(super) fn drop_deferred(&mut self) {
        self.deferred.clear();
    }
}
