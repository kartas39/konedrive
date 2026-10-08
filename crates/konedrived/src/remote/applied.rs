//! What a cycle's reconcile did to the folder: the numbers, what stands in the folder
//! whatever comes of the commit, and each change by name for the activity log.

use std::path::PathBuf;

use crate::status::activity::Kind as EventKind;

/// What a reconcile did to the folder, in numbers.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub created: u64,
    pub moved: u64,
    pub deleted: u64,
    pub updated: u64,
}

impl Counts {
    fn add(&mut self, other: Counts) {
        let Counts { created, moved, deleted, updated } = other;
        self.created += created;
        self.moved += moved;
        self.deleted += deleted;
        self.updated += updated;
    }
}

/// What a reconcile did in the folder that stands whatever comes next: when its commit
/// fails, and from one page of a first listing to the next.
#[derive(Debug, Default)]
pub struct OnDisk {
    /// Read-write mode: the user's own files and directories kept beside an item that came
    /// to their name (`docs/design/writes.md` §7).
    pub copies: Vec<Copied>,
    /// Read-write mode: folders gone from OneDrive whose directory stays
    /// here, holding local work, to be made again there.
    pub recreated: Vec<String>,
}

impl OnDisk {
    /// Adds what a `later` reconcile did to what this one did, this one's first.
    // Every field named: one added to `OnDisk` does not compile here until
    // it is handled.
    fn absorb(&mut self, later: OnDisk) {
        let OnDisk { copies, recreated } = later;
        self.copies.extend(copies);
        self.recreated.extend(recreated);
    }
}

#[derive(Debug, Default)]
pub struct Applied {
    pub counts: Counts,
    pub on_disk: OnDisk,
    /// What the reconcile did, item by item, for the activity log (`docs/design/desktop.md`
    /// §2.4).
    pub changes: Vec<Changed>,
}

impl Applied {
    /// Adds what one page of a first listing did to what the pages before
    /// it did. `changes` stays empty: a first listing is one `listed`
    /// event.
    pub fn add_page(&mut self, page: Applied) {
        let Applied { counts, on_disk, changes: _ } = page;
        self.counts.add(counts);
        self.on_disk.absorb(on_disk);
    }
}

/// A local version kept beside the cloud's under a new name (`docs/design/writes.md`
/// §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copied {
    /// Its name before, relative to the root: now the cloud's version.
    pub original: PathBuf,
    /// Where it is now, relative to the root.
    pub copy: PathBuf,
}

/// One thing a reconcile did to the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    /// `Added`, `Updated`, `Removed` or `Moved`.
    pub kind: EventKind,
    /// Relative to the root: where the item is now, or was, if removed.
    pub rel: PathBuf,
    /// Where a moved item was, relative to the root.
    pub from: Option<PathBuf>,
}
