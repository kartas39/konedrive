//! What a pass over the folder did, and left for later: the numbers, what
//! stands on disk whatever comes next, what waits, and how it failed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::status::activity::Kind as EventKind;
use konedrive_tree::TreeError;

/// A file downloaded here whose content changed in the cloud: fetches
/// the new version beside it and swaps it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    pub id: String,
    pub rel: PathBuf,
    pub ctag: String,
    /// The new version's size, to check the disk can hold it beside the old.
    pub size: u64,
}

/// What a pass did to the folder, in numbers.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub created: u64,
    pub moved: u64,
    pub deleted: u64,
    pub updated: u64,
    /// Files being filled or freed right now; the next cycle looks again.
    pub deferred: u64,
}

impl Counts {
    pub(super) fn add(&mut self, other: Counts) {
        let Counts { created, moved, deleted, updated, deferred } = other;
        self.created += created;
        self.moved += moved;
        self.deleted += deleted;
        self.updated += updated;
        self.deferred += deferred;
    }
}

/// What a pass did on disk that stands whatever comes next: when the pass
/// fails and a Full one follows, and from one page of a first listing to
/// the next. The only merge of two passes is [`OnDisk::absorb`].
#[derive(Debug, Default)]
pub struct OnDisk {
    /// Local versions moved out of the way, each a conflict.
    pub rescued: Vec<Rescued>,
    /// Read-write mode: local versions kept beside the cloud's (`docs/design/writes.md` §7).
    pub copies: Vec<Copied>,
    /// Read-write mode: places for the examination to look at, relative to
    /// the root (`true`: with everything below) — files and folders this
    /// reconcile kept, copied or took its attributes off, which no event the
    /// watcher keeps says (the daemon's own changes are dropped by pid).
    pub examine: Vec<(PathBuf, bool)>,
    /// Read-write mode: folders gone from OneDrive whose directory stays
    /// here, holding local work, to be made again there.
    pub recreated: Vec<String>,
    /// Items whose change the base takes in this cycle whatever a local
    /// change holds — removed in OneDrive, or no longer placed here, and
    /// taken off the disk. Never deferred.
    pub taken: HashSet<String>,
    /// Read-write mode: what was removed in OneDrive and stays here in
    /// part, relative to the root, with what stays. One entry for the
    /// outermost thing removed ([`OnDisk::note_kept`]).
    pub kept: Vec<(PathBuf, Kept)>,
}

/// What stays on this computer of something removed in OneDrive.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Kept {
    /// Files changed or made here, which the examination records as new.
    pub uploaded: u64,
    /// Files and folders that stay on this computer only: their name is
    /// ignored or refused by OneDrive, or a folder above them has such a
    /// name, so nothing uploads them.
    pub local: u64,
    /// How many of them were the daemon's until this pass took konedrive's
    /// attributes off them: a later pass finds them as the user's own.
    pub stripped: u64,
}

impl Kept {
    pub(in crate::remote::materialize) fn since(self, before: Kept) -> Kept {
        Kept { uploaded: self.uploaded - before.uploaded, local: self.local - before.local, stripped: self.stripped - before.stripped }
    }

    pub fn is_empty(self) -> bool {
        self.uploaded + self.local == 0
    }
}

impl OnDisk {
    /// Adds what a `later` pass did to what this one did, this one's first.
    // Every field named: one added to `OnDisk` does not compile here until
    // it is handled.
    pub fn absorb(&mut self, later: OnDisk) {
        let OnDisk { rescued, copies, examine, recreated, taken, kept } = later;
        for (rel, kept) in kept {
            self.note_kept(&rel, kept);
        }
        self.rescued.extend(rescued);
        self.copies.extend(copies);
        self.examine.extend(examine);
        self.recreated.extend(recreated);
        self.taken.extend(taken);
    }
}

impl OnDisk {
    /// `kept` stays of what was removed at `rel`. Said once, for the
    /// outermost thing removed: an entry at or below `rel` is replaced, and
    /// nothing is added below an entry there is.
    pub(in crate::remote::materialize) fn note_kept(&mut self, rel: &Path, kept: Kept) {
        if self.kept.iter().any(|(above, _)| rel != above && rel.starts_with(above)) {
            return;
        }
        // What an earlier pass took the attributes off is counted as that
        // still, though this pass found it the user's own.
        let stripped = self.kept.iter().filter(|(below, _)| below.starts_with(rel)).map(|(_, k)| k.stripped).sum::<u64>();
        self.kept.retain(|(below, _)| !below.starts_with(rel));
        self.kept.push((rel.to_path_buf(), Kept { stripped: kept.stripped + stripped, ..kept }));
    }
}

/// A reconcile that failed, and what it did on disk before it did
/// ([`OnDisk`]): that stands, and is still to be said and handed on.
#[derive(Debug)]
pub struct Failed {
    pub error: ApplyError,
    pub done: OnDisk,
}

/// What a pass left for later. A pass that fails hands none of it over: the
/// pass after it decides that again.
#[derive(Debug, Default)]
pub struct Pending {
    /// Read-write mode: items this reconcile left as they are on disk — a
    /// local change holds them, or their new version is still to land — so
    /// the base keeps the version the disk holds, and the delta's change
    /// waits (`docs/design/writes.md` §9).
    pub unsettled: HashSet<String>,
    /// Read-write mode: items placed where the tree has them whose content
    /// the disk has not taken yet — a replacement to land, a placeholder or
    /// a file being filled: the base takes the new place, and keeps the
    /// content the file holds.
    pub content_waits: HashSet<String>,
    /// Downloaded files whose content changed in the cloud, to be replaced
    /// once the cycle is done.
    pub replacements: Vec<Replacement>,
    /// Read-write mode: of the unsettled items, those OneDrive still has
    /// and the folder cannot hold any more, each with what keeps it here
    /// (a [`konedrive_tree::WaitsFor`], as stored).
    pub waits: Vec<(String, String)>,
}

impl Pending {
    pub(super) fn add(&mut self, other: Pending) {
        let Pending { unsettled, content_waits, replacements, waits } = other;
        self.waits.extend(waits);
        self.unsettled.extend(unsettled);
        self.content_waits.extend(content_waits);
        self.replacements.extend(replacements);
    }
}

#[derive(Debug, Default)]
pub struct Applied {
    pub counts: Counts,
    pub on_disk: OnDisk,
    pub pending: Pending,
    /// What a Changed scope did, item by item, for the activity log (`docs/design/desktop.md`
    /// §2.4). A Full scope leaves it empty: it is one `listed` event, not
    /// one per item.
    pub changes: Vec<Changed>,
    /// Files made, and files or folders moved, inside a folder a pin keeps
    /// on this device, relative to the root: what the sync queues for
    /// download once the reconcile is done.
    pub pinned: Vec<PathBuf>,
}

impl Applied {
    /// Adds what one page of a first listing did to what the pages before
    /// it did. `changes` stays empty: a first listing is one `listed`
    /// event, as a Full reconcile is.
    pub fn add_page(&mut self, page: Applied) {
        let Applied { counts, on_disk, pending, changes: _, pinned } = page;
        self.counts.add(counts);
        self.on_disk.absorb(on_disk);
        self.pending.add(pending);
        self.pinned.extend(pinned);
    }
}

/// A local version kept beside the cloud's under a new name (`docs/design/writes.md`
/// §7): what read-write mode does where the read phase rescued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copied {
    /// Its name before, relative to the root: now the cloud's version.
    pub original: PathBuf,
    /// Where it is now, relative to the root.
    pub copy: PathBuf,
}

/// A local version a reconcile moved out of the way (`docs/design/sync.md` §10.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rescued {
    /// Where it was, relative to the root.
    pub original: PathBuf,
    /// Where it is now, as a full path.
    pub rescued: PathBuf,
}

/// One thing an incremental reconcile did to the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    /// `Added`, `Updated`, `Removed` or `Moved`.
    pub kind: EventKind,
    /// Relative to the root: where the item is now, or was, if removed.
    pub rel: PathBuf,
    /// Where a moved item was, relative to the root.
    pub from: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("the folder does not match the stored tree ({0})")]
    NeedFull(String),
    #[error("cancelled")]
    Cancelled,
    #[error("the helper did not mark {0}: {1}")]
    Mark(PathBuf, String),
    #[error("{0}")]
    Io(String),
    #[error(transparent)]
    Tree(#[from] TreeError),
}

impl From<std::io::Error> for ApplyError {
    fn from(e: std::io::Error) -> Self {
        ApplyError::Io(e.to_string())
    }
}
