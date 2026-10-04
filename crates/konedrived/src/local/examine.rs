//! The examination (`docs/design/writes.md` §4, amended below): from a batch of
//! dirty places to outbox rows.
//!
//! Items are identified by their item id, never by path: a rename is a move
//! of an id, and a safe save is a new inode that takes over an id. For each
//! place in the batch the examination reads the disk by name (`lstat`,
//! `lgetxattr`), and then decides, in this order:
//!
//! 1. the daemon's own names (`.konedrive-holding`, `.konedrive-new-*`) are
//!    passed over; a user's `.konedrive-*` is listed, never uploaded;
//! 2. a symlink, FIFO, socket or device is listed in `local_skipped` (unless
//!    its name is ignored);
//! 3. a name on the ignore list, without an item id, stays local;
//! 4. a directory without an item id is a `mkdir`, and its contents are
//!    examined too; 5. a file without one is a `create`;
//! 6. an entry with item id I: unknown to the base, it is a file from
//!    elsewhere (stripped and created if downloaded, listed if not). Known,
//!    it is I only if the base places I and the entry is the object the base
//!    records for it — or, when that object is not among those seen or none
//!    is recorded, the one standing where I is expected (at I's base place,
//!    when a row says I was removed: the removal is taken back); any other
//!    entry carrying the id is a copy, a file from elsewhere
//!    (`Run::resolve`), whatever became of I's own object.
//!    Where the base has I, its content is checked; elsewhere, it is a
//!    `move` too;
//! 7. a base item missing from its place and from the whole batch is a
//!    save-by-rename when a new file stands at its name, and otherwise is
//!    asked after by its file handle: gone is a `delete`, alive outside the
//!    folder a `move-out`. With no recorded handle (a rebuilt base) it is
//!    never deleted. A folder that leaves takes what is inside it along,
//!    except what left it first, which leaves on its own.
//!
//! There is no base until a listing has completed, and nothing is examined
//! in a root that went away. The content check never fills a placeholder,
//! reads only downloaded files (WR1), and probes for a writer with a read
//! lease before trusting what it reads. A row the worker is running is never
//! taken from under it: what changed since waits behind it. An entry this
//! daemon is refused to open, strip or read is passed over and examined
//! again later; its trouble never fails the batch. Everything found is
//! applied to the store in one transaction.

mod classify;
mod finish;
mod found;
mod list;
mod missing;
mod run;

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::{self};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::XATTR_ROOT;
use xattr::FileExt;

use super::batch::Batch;
use super::entry::{self, Entry};
use super::ignore::IgnoreList;
use super::liveness::Liveness;
use crate::folder::disk::{Disk, HOLDING, NEW_PREFIX};
use crate::folder::locks::InodeLocks;
use konedrive_tree::outbox::{Base, Detection, Inode, LocalSkip, OutboxApplied, OutboxKind, OutboxOp, OutboxRow, OutboxState};
use konedrive_tree::{Located, Row, Store, TreeError};

/// How many of the places a run did not examine its one warning names.
const UNREADABLE_NAMED: usize = 20;

#[derive(Debug, thiserror::Error)]
pub enum ExamineError {
    #[error(transparent)]
    Tree(#[from] TreeError),
    #[error("the folder: {0}")]
    Io(#[from] io::Error),
    /// No listing has completed yet (a new folder, or a store rebuilt and
    /// listing again): part-way through one, an item not listed yet cannot be
    /// told from one that is gone, nor a file of ours from a stranger. The
    /// watcher runs the Full local scan once the first cycle has completed.
    #[error("the folder has no completed listing yet, so there is no base to compare with")]
    NoBase,
    /// The root was deleted, or no longer carries its root id: nothing is
    /// examined, so nothing is deleted in the cloud because it went (§3.3).
    #[error("the OneDrive folder was moved or deleted")]
    RootGone,
}

/// What an examination did, beyond the rows it wrote.
#[derive(Debug, Default)]
pub struct Examined {
    /// The rows written and removed.
    pub applied: OutboxApplied,
    /// To examine again after [`RECHECK`]: files open for writing, being
    /// filled or freed, and items that moved somewhere the batch did not see.
    pub recheck: Batch,
    /// Placeholders a `truncate(2)` had cut: their size is the cloud's again
    /// (the cloud wins, nothing is uploaded).
    pub restored: Vec<PathBuf>,
    /// Base items missing with no recorded handle: never deleted in OneDrive
    /// (WR4); the reconcile places them again.
    pub unproven: Vec<String>,
    /// Items whose whereabouts could not be asked or placed (no helper, a
    /// path that says nothing): their places are in `recheck`, and examined
    /// again until an answer decides them.
    pub undecided: Vec<String>,
    /// Items the mass-delete guard held (0 when it did not trip).
    pub held: u64,
    /// Managed placeholders with more than one link, for `MarkFile`.
    pub mark_files: Vec<PathBuf>,
    /// Entries whose konedrive attributes were taken off: copies, files from
    /// elsewhere, editors' backups.
    pub stripped: Vec<PathBuf>,
    /// Places this daemon may not read (a directory set to `000`), and
    /// entries it was refused to open, strip or read (`Run::entry_io`): not
    /// examined, so nothing in or at them counts as missing.
    pub unreadable: Vec<PathBuf>,
    /// The entries passed over, to examine again: kept apart from `recheck`,
    /// since their cause may last, and the watcher backs their recheck off.
    pub passed: Batch,
    /// The folder's filesystem had changed: its file handles were taken again
    /// (a Full scan), and nothing was decided by the old ones.
    pub renewed: bool,
}

/// Told how a Full local scan goes (issue #8): once it has started — the base is there and
/// the root is — and then after each directory it listed, with what it has seen so far.
/// Only a Full scan is told; an examination of single places never is.
pub trait ScanProgress {
    fn started(&self);
    /// Directories and other entries (files, links, ...) read so far, the root not counted.
    fn seen(&self, directories: u64, files: u64);
}

pub struct Examiner<'a> {
    pub disk: &'a Disk,
    pub store: &'a Store,
    pub liveness: &'a dyn Liveness,
    pub ignore: &'a IgnoreList,
    /// The folder's per-inode locks, which a fill holds for its whole run.
    pub locks: &'a InodeLocks,
    /// Unix seconds: `next_try` and `local_skipped` are counted from it.
    pub now: i64,
}

impl Examiner<'_> {
    /// Examines `batch` and records what it found.
    pub fn examine(&self, batch: &Batch) -> Result<Examined, ExamineError> {
        self.examine_reporting(batch, None)
    }

    /// [`examine`](Self::examine), telling `progress` how a Full local scan goes.
    pub fn examine_reporting(&self, batch: &Batch, progress: Option<&dyn ScanProgress>) -> Result<Examined, ExamineError> {
        let (root_id, complete) = self.store.call_blocking(move |s| Ok((s.root_item_id()?, s.delta_link()?.is_some() && s.listing_next()?.is_none())))?;
        let root_id = root_id.filter(|_| complete).ok_or(ExamineError::NoBase)?;
        let root = self.disk.dir(Path::new(""))?;
        let stat = nix::sys::stat::fstat(&root).map_err(io::Error::from)?;
        if stat.st_nlink == 0 || root.get_xattr(XATTR_ROOT)?.is_none() {
            return Err(ExamineError::RootGone);
        }
        let root_path = std::fs::read_link(entry::proc_path(&root)).ok();
        // The folder's filesystem changed since its handles were recorded (a new disk, a
        // snapshot rolled back): they are taken again, by a Full scan of everything, before
        // anything is decided by them. Only a Full local scan asked for is told how it goes,
        // not one the renewed handles make.
        let progress = progress.filter(|_| batch.is_full());
        let full = Batch::full();
        let handles = super::handles::prepare(self.store, &root, self.now)?;
        let batch = if handles.renewed { &full } else { batch };
        let rows = Rows::new(self.store.call_blocking(move |s| s.outbox_rows())?);
        if let Some(progress) = progress {
            progress.started();
        }
        let mut run = Run {
            ex: self,
            progress,
            seen: (0, 0),
            root_id,
            root_dev: stat.st_dev as u64,
            root_path,
            handles_current: handles.current,
            helper_silent: Cell::new(false),
            rows,
            entries: Vec::new(),
            at: HashMap::new(),
            whole: BTreeSet::new(),
            named: BTreeMap::new(),
            unreadable: HashSet::new(),
            base: HashMap::new(),
            recorded: HashMap::new(),
            expected: HashMap::new(),
            chosen: HashMap::new(),
            decided: HashSet::new(),
            deferred: HashMap::new(),
            consumed: HashSet::new(),
            fresh: Vec::new(),
            skipped: HashMap::new(),
            detections: Vec::new(),
            ops: Vec::new(),
            out: Examined::default(),
        };
        run.out.renewed = handles.renewed;
        run.list(batch)?;
        run.probe_expected()?;
        run.classify(batch)?;
        let out = run.finish()?;
        if !out.unreadable.is_empty() {
            let shown: Vec<String> = out.unreadable.iter().take(UNREADABLE_NAMED).map(|rel| rel.display().to_string()).collect();
            let more = out.unreadable.len() - shown.len();
            let rest = if more > 0 { format!(", and {more} more") } else { String::new() };
            tracing::warn!(
                "{} place(s) this daemon may not read or change are not examined, and nothing in them is uploaded: {}{rest}",
                out.unreadable.len(),
                shown.join(", ")
            );
        }
        Ok(out)
    }

    /// The Full local scan: every directory of the folder.
    pub fn full_scan(&self) -> Result<Examined, ExamineError> {
        self.examine(&Batch::full())
    }
}

/// Where an item is expected to be.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Expect {
    At(PathBuf),
    /// Deleted or moved out already (a row says so), or not placed.
    Nowhere,
    /// Not in the base.
    Unknown,
}

/// Where an object is, as far as the examination can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Place {
    Gone,
    /// Beneath the root, proved by its handle at that place.
    Inside(PathBuf),
    /// Outside the folder, at this absolute path.
    Outside(PathBuf),
    /// Could not be asked, or the answer says nothing sure.
    Unknown,
}

/// How an item that left its place was decided. Ordered: a folder waits
/// as long as the least settled item inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Settle {
    /// Its row is written.
    Done,
    /// Held back until it can be placed: examined again (undecided).
    Wait,
    /// Held back until the reconcile places it again: no recorded handle,
    /// so nothing can prove it gone (WR4, unproven).
    Unproven,
}

/// What a content check found.
enum Content {
    Changed,
    /// Checked and the base's.
    Same,
    /// Could not tell (busy, not ours to read, an upload of it running).
    Unknown,
    /// Changed or not, a writer has it open.
    Waiting,
}

struct Run<'e, 'a> {
    ex: &'e Examiner<'a>,
    /// Told after each directory a Full local scan lists.
    progress: Option<&'e dyn ScanProgress>,
    /// Directories and other entries read in whole listings so far.
    seen: (u64, u64),
    root_id: String,
    /// The folder's device: nothing on another one is uploaded, nor looked
    /// into (a nested Btrfs subvolume, a mount).
    root_dev: u64,
    root_path: Option<PathBuf>,
    /// Whether the recorded handles are this filesystem's: `ESTALE` is gone
    /// only then (the move-out step).
    handles_current: bool,
    /// An ask about an object's whereabouts timed out in this run: nothing
    /// more is asked in it, and what would have been asked is not decided
    /// ([`Run::place_of`]).
    helper_silent: Cell<bool>,
    /// The live rows before this examination, and what they are looked up by.
    rows: Rows,
    entries: Vec<Entry>,
    at: HashMap<PathBuf, usize>,
    whole: BTreeSet<PathBuf>,
    named: BTreeMap<PathBuf, BTreeSet<OsString>>,
    /// Places not examined in this run: unreadable, passed over, or a
    /// directory with an id not its own that could not be stripped or went
    /// while the run looked at it. Nothing at them counts as missing, and
    /// nothing new below them gets a row.
    unreadable: HashSet<PathBuf>,
    base: HashMap<String, Option<Row>>,
    /// Item id → the local object the base records (`items.local_handle`),
    /// and where the base places the item: asked with the item's row, in one
    /// job of the store's thread (issue #38).
    recorded: HashMap<String, (Option<FileHandle>, Option<Located>)>,
    expected: HashMap<String, Expect>,
    /// Item id → the entry that is the item.
    chosen: HashMap<String, usize>,
    /// Items this batch decided without choosing an entry: removed, or left
    /// for later (undecided, rechecked).
    decided: HashSet<String>,
    /// The decided items that got no row, and why: a folder they are in
    /// must not be removed before them.
    deferred: HashMap<String, Settle>,
    /// Entries taken by an item: its own, its links, a save-by-rename's new file.
    consumed: HashSet<usize>,
    /// Entries stripped of an id they had no right to: new objects now.
    fresh: Vec<usize>,
    skipped: HashMap<PathBuf, LocalSkip>,
    detections: Vec<Detection>,
    ops: Vec<OutboxOp>,
    out: Examined,
}

/// The live rows as an examination looks them up (issue #38): by item, by
/// local object, by place, by parent directory, built once per run, so that
/// no step walks every row for each entry, item or directory.
struct Rows {
    /// In `seq` order.
    all: Vec<OutboxRow>,
    /// Item id → its rows, oldest first.
    by_item: HashMap<String, Vec<usize>>,
    /// Rows without an item id, by the handle of their object...
    by_handle: HashMap<FileHandle, Vec<usize>>,
    /// ... and by its inode.
    by_inode: HashMap<(u64, u64), Vec<usize>>,
    /// Every row by its place, in path order: what is below a directory is
    /// one range.
    by_rel: BTreeMap<PathBuf, Vec<usize>>,
    /// Running `mkdir` rows without an item id.
    making: Vec<usize>,
}

impl Rows {
    fn new(all: Vec<OutboxRow>) -> Self {
        let mut rows = Rows {
            all,
            by_item: HashMap::new(),
            by_handle: HashMap::new(),
            by_inode: HashMap::new(),
            by_rel: BTreeMap::new(),
            making: Vec::new(),
        };
        for (i, row) in rows.all.iter().enumerate() {
            rows.by_rel.entry(row.rel.clone()).or_default().push(i);
            match (&row.item_id, &row.inode) {
                (Some(id), _) => rows.by_item.entry(id.clone()).or_default().push(i),
                (None, Some(inode)) => {
                    if let Some(handle) = &inode.handle {
                        rows.by_handle.entry(handle.clone()).or_default().push(i);
                    }
                    rows.by_inode.entry((inode.dev, inode.ino)).or_default().push(i);
                    if row.kind == OutboxKind::Mkdir && row.state == OutboxState::Running {
                        rows.making.push(i);
                    }
                }
                (None, None) => {}
            }
        }
        rows
    }

    fn iter(&self) -> std::slice::Iter<'_, OutboxRow> {
        self.all.iter()
    }

    /// The live rows of item `id`, oldest first.
    fn of_item(&self, id: &str) -> impl DoubleEndedIterator<Item = &OutboxRow> + '_ {
        self.by_item.get(id).into_iter().flatten().map(|&i| &self.all[i])
    }

    /// The rows without an item id whose object is `e`'s ([`Inode::same_object`]), oldest first.
    fn of_object(&self, e: &Entry) -> Vec<&OutboxRow> {
        let mut found: Vec<usize> = Vec::new();
        if let Some(handle) = &e.handle {
            found.extend(self.by_handle.get(handle).into_iter().flatten());
        }
        found.extend(self.by_inode.get(&(e.dev, e.ino)).into_iter().flatten());
        found.sort_unstable();
        found.dedup();
        found
            .into_iter()
            .map(|i| &self.all[i])
            .filter(|row| {
                row.inode.as_ref().is_some_and(|i| match (&i.handle, &e.handle) {
                    (Some(a), Some(b)) => a == b,
                    _ => i.dev == e.dev && i.ino == e.ino,
                })
            })
            .collect()
    }

    /// The rows at `rel` exactly.
    fn at(&self, rel: &Path) -> impl Iterator<Item = &OutboxRow> + '_ {
        self.by_rel.get(rel).into_iter().flatten().map(|&i| &self.all[i])
    }

    /// The rows strictly below `dir`, in `seq` order.
    fn under(&self, dir: &Path) -> Vec<&OutboxRow> {
        let mut found: Vec<usize> = self
            .by_rel
            .range::<Path, _>((std::ops::Bound::Excluded(dir), std::ops::Bound::Unbounded))
            .take_while(|(rel, _)| rel.starts_with(dir))
            .flat_map(|(_, list)| list.iter().copied())
            .collect();
        found.sort_unstable();
        found.into_iter().map(|i| &self.all[i]).collect()
    }

    /// The rows whose place is directly in `dir`, in `seq` order.
    fn in_dir(&self, dir: &Path) -> Vec<&OutboxRow> {
        self.under(dir).into_iter().filter(|row| row.rel.parent() == Some(dir)).collect()
    }
}

/// The objects of the entries listed, to tell whether a pending row's object
/// was seen ([`Inode::same_object`]) without comparing it with every entry.
struct Objects {
    handles: HashSet<FileHandle>,
    /// Every entry's inode, and those of entries with no handle.
    inodes: HashSet<(u64, u64)>,
    unhandled: HashSet<(u64, u64)>,
}

impl Objects {
    fn of(entries: &[Entry]) -> Self {
        let mut objects = Objects { handles: HashSet::new(), inodes: HashSet::new(), unhandled: HashSet::new() };
        for e in entries {
            objects.inodes.insert((e.dev, e.ino));
            match &e.handle {
                Some(handle) => {
                    objects.handles.insert(handle.clone());
                }
                None => {
                    objects.unhandled.insert((e.dev, e.ino));
                }
            }
        }
        objects
    }

    fn seen(&self, inode: &Inode) -> bool {
        match &inode.handle {
            Some(handle) => self.handles.contains(handle) || self.unhandled.contains(&(inode.dev, inode.ino)),
            None => self.inodes.contains(&(inode.dev, inode.ino)),
        }
    }
}

fn depth(rel: &Path) -> usize {
    rel.components().count()
}

fn daemon_owned(name: &OsStr) -> bool {
    name == OsStr::new(HOLDING) || name.as_bytes().starts_with(NEW_PREFIX.as_bytes())
}

fn gone(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP))
}

fn denied(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EACCES | libc::EPERM))
}

fn lossy(name: &OsStr) -> String {
    name.to_string_lossy().into_owned()
}

fn base_of(base: &Row) -> Base {
    Base { etag: base.etag.clone(), ctag: base.ctag.clone(), parent: base.parent_id.clone(), name: Some(base.name.clone()) }
}

/// An item's object as a row keeps it when only its handle is known.
fn object(handle: FileHandle) -> Inode {
    Inode { dev: 0, ino: 0, handle: Some(handle) }
}
