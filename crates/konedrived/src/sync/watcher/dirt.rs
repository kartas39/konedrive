//! What the events since the last hand-over made dirty, and when it is time
//! to hand it over (`docs/design/writes.md` §3 "Batches", amended by §3.4).
//!
//! Dirt is kept by directory *handle*, not by path, and turned into a
//! [`Batch`] of paths only at hand-over, through the map as it is then: a
//! directory renamed while the batch gathers is examined where it now is.
//! The event's object handle is kept too, so the examination finds the item
//! it touched wherever the name says (a `#<inode>` pseudo-name of an
//! `O_TMPFILE` write, a merged event). Nothing here reads an order from the
//! events: a mask is a set.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::time::Instant;

use konedrive_fs::handle::FileHandle;

use super::fan::Fid;
use super::map::DirMap;
use super::Timing;
use crate::sync::local::Batch;

#[derive(Debug, Default)]
pub struct Dirt {
    full: bool,
    names: HashMap<Fid, BTreeSet<OsString>>,
    trees: HashSet<Fid>,
    objects: BTreeSet<FileHandle>,
    written: Vec<(Fid, OsString, Option<FileHandle>)>,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Dirt {
    /// An event arrived at `now` that made something dirty.
    pub fn touch(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    /// `name` in `dir`; `"."` is `dir` itself.
    pub fn name(&mut self, dir: &Fid, name: &OsStr) {
        self.names.entry(dir.clone()).or_default().insert(name.to_owned());
    }

    /// `dir` and everything below it.
    pub fn tree(&mut self, dir: &Fid) {
        self.trees.insert(dir.clone());
    }

    pub fn object(&mut self, handle: &FileHandle) {
        self.objects.insert(handle.clone());
    }

    /// `FAN_CLOSE_WRITE`: the content is hashed even if size and time say
    /// nothing changed.
    pub fn written(&mut self, dir: &Fid, name: &OsStr, handle: Option<&FileHandle>) {
        self.written.push((dir.clone(), name.to_owned(), handle.cloned()));
    }

    /// Everything: the Full local scan.
    pub fn full(&mut self) {
        self.full = true;
    }

    pub fn is_empty(&self) -> bool {
        !self.full && self.names.is_empty() && self.trees.is_empty() && self.objects.is_empty() && self.written.is_empty()
    }

    /// When to hand over: [`Timing::quiet`] after the last event, at the
    /// latest [`Timing::ceiling`] after the first.
    pub fn due(&self, timing: &Timing) -> Option<Instant> {
        if self.is_empty() {
            return None;
        }
        let (first, last) = (self.first?, self.last?);
        Some((last + timing.quiet).min(first + timing.ceiling))
    }

    /// The batch, with every directory at its path in `map` now, and the
    /// dirt cleared. A directory the map no longer knows went away since
    /// (deleted, or out of the folder): its own removal dirtied the name it
    /// had, so nothing is lost by passing over what was inside it.
    pub fn take(&mut self, map: &DirMap) -> Batch {
        let dirt = std::mem::take(self);
        let mut batch = if dirt.full { Batch::full() } else { Batch::new() };
        for (dir, names) in dirt.names {
            if let Some(path) = map.path(&dir) {
                for name in names {
                    batch.name(&path, &name);
                }
            }
        }
        for (dir, name, handle) in dirt.written {
            if let Some(path) = map.path(&dir) {
                batch.written(&path, &name, handle);
            }
        }
        for dir in dirt.trees {
            if let Some(path) = map.path(&dir) {
                batch.tree(&path);
            }
        }
        for handle in dirt.objects {
            batch.object(handle);
        }
        batch
    }
}
