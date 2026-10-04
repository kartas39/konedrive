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
//!    ([`identity::identify`]), whatever became of I's own object.
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
//! again later; its trouble never fails the batch.
//!
//! A run is four parts with one owner each. The [`Listing`] is what was
//! read of the disk: built first, read-only afterwards. [`Facts`] is the
//! store as the run reads it, the only part that asks the store before the
//! end. [`Decisions`] is who is who: which entry is which item, what is
//! settled, what is spoken for. [`Outcome`] is what the run leaves: the
//! rows, the skipped list, what it reports. Which object is an item is one
//! function of facts and entries that touches nothing
//! ([`identity::identify`]).
//!
//! The rows, the recorded objects and the skipped list are applied to the
//! store in one transaction, at the end (`finish`), and what is said in
//! Activity right after it. The disk is written on the way, while deciding,
//! only through [`Hands`]: a copy's marks are taken off, an empty copy is
//! removed, a cut placeholder gets its size back, a touched file's stamp is
//! renewed. A decision depends on whether each worked, and each is what the
//! next run would do again (`hands.rs` says why for each). Every listed
//! entry is opened through [`Hands::open`], which gives it up when its name
//! holds another object by then.

mod copies;
mod decisions;
mod detect;
mod facts;
mod finish;
mod found;
mod hands;
mod identity;
mod listing;
mod missing;
mod new;
mod place;

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::XATTR_ROOT;
use konedrive_fs::RESERVED_PREFIX;
use std::os::unix::ffi::OsStrExt;
use xattr::FileExt;

use self::decisions::{Decisions, Settle};
use self::facts::Facts;
use self::hands::Hands;
use self::listing::{EntryIx, Listing, Reader};
use super::batch::Batch;
use super::entry::{self, Entry};
use super::ignore::IgnoreList;
use super::liveness::Liveness;
use crate::folder::disk::{daemon_owned, gone, Disk};
use crate::folder::locks::InodeLocks;
use konedrive_fs::handle::FileHandle;
use konedrive_tree::outbox::{Base, Detection, Inode, LocalSkip, OutboxApplied, OutboxOp};
use konedrive_tree::{ActivityRow, Row, Store, TreeError};

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
    /// To examine again after [`RECHECK`](super::RECHECK): files open for writing, being
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
        let mut facts = Facts::new(self.store, root_id)?;
        if let Some(progress) = progress {
            progress.started();
        }
        let root_dev = stat.st_dev as u64;
        let listing = Listing::read(&Reader { disk: self.disk, ignore: self.ignore, root_dev, progress }, &mut facts, batch)?;
        let mut run = Run {
            ex: self,
            root_dev,
            root_path,
            handles_current: handles.current,
            helper_silent: Cell::new(false),
            listing: &listing,
            facts,
            decisions: Decisions::default(),
            outcome: Outcome::default(),
            hands: Hands { disk: self.disk, locks: self.locks },
        };
        run.outcome.out.renewed = handles.renewed;
        run.outcome.out.unreadable = listing.unread().to_vec();
        run.decide(batch)?;
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

/// What a run leaves behind: written by [`Run::finish`].
#[derive(Default)]
struct Outcome {
    detections: Vec<Detection>,
    ops: Vec<OutboxOp>,
    skipped: HashMap<PathBuf, LocalSkip>,
    /// What is said in Activity: written right after the rows.
    activity: Vec<ActivityRow>,
    out: Examined,
}

struct Run<'e, 'a, 'l> {
    ex: &'e Examiner<'a>,
    /// The folder's device: nothing on another one is uploaded (a nested
    /// Btrfs subvolume, a mount).
    root_dev: u64,
    root_path: Option<PathBuf>,
    /// Whether the recorded handles are this filesystem's: `ESTALE` is gone
    /// only then (the move-out step).
    handles_current: bool,
    /// An ask about an object's whereabouts timed out in this run: nothing
    /// more is asked in it, and what would have been asked is not decided
    /// ([`Run::place_of`]).
    helper_silent: Cell<bool>,
    listing: &'l Listing,
    facts: Facts<'e>,
    decisions: Decisions,
    outcome: Outcome,
    hands: Hands<'e>,
}

/// The listed entries sorted for the rules: what is passed over or only listed, what
/// carries an id (by id), and what carries none.
#[derive(Debug, Default, PartialEq, Eq)]
struct Sorted {
    listed: Vec<(EntryIx, LocalSkip)>,
    by_id: BTreeMap<String, Vec<EntryIx>>,
    unnamed: Vec<EntryIx>,
}

/// Rules 1 and 2 for every entry, and whether it carries an id.
fn sort(listing: &Listing, ignore: &IgnoreList, root_dev: u64) -> Sorted {
    let mut sorted = Sorted::default();
    for (ix, e) in listing.iter() {
        // 1. The daemon's own names; a user's `.konedrive-*` is listed.
        if daemon_owned(&e.name) {
            continue;
        }
        if e.name.as_bytes().starts_with(RESERVED_PREFIX.as_bytes()) {
            sorted.listed.push((ix, LocalSkip::ReservedName));
            continue;
        }
        // 2. Never a OneDrive object — unless its name is ignored anyway
        // (Emacs's `.#name` lock is a symlink).
        if let Some(reason) = e.ty.skip_reason() {
            if !ignore.matches(&e.name) {
                sorted.listed.push((ix, reason));
            }
            continue;
        }
        // 2b. On another device than the folder's (a nested Btrfs
        // subvolume, a mount): never uploaded, nor anything below it —
        // the helper cannot protect what is placed there (F72).
        if e.dev != root_dev {
            sorted.listed.push((ix, LocalSkip::OtherDevice));
            continue;
        }
        match &e.id {
            Some(id) => sorted.by_id.entry(id.clone()).or_default().push(ix),
            None => sorted.unnamed.push(ix),
        }
    }
    sorted
}

impl<'l> Run<'_, '_, 'l> {
    /// The rules, in their order.
    fn decide(&mut self, batch: &Batch) -> Result<(), ExamineError> {
        let listing = self.listing;
        let sorted = sort(listing, self.ex.ignore, self.root_dev);
        for (ix, reason) in &sorted.listed {
            self.skip(&listing[*ix].rel, reason.clone());
        }
        // 6. Who is who, before anything is decided by place: a directory's
        // id says what its entries' parent is. Every id is decided first,
        // from what was listed; then each decision is carried out.
        let mut identities = Vec::with_capacity(sorted.by_id.len());
        for (id, carriers) in &sorted.by_id {
            identities.push((id.as_str(), self.identity(id, carriers)?));
        }
        let mut found: Vec<(&str, EntryIx)> = Vec::new();
        for (id, (identity, base)) in &identities {
            self.settle_identity(id, identity, base.as_deref())?;
            found.extend(identity.item.map(|ix| (*id, ix)));
        }
        found.sort_by_key(|(_, ix)| depth(&listing[*ix].rel));
        for (id, ix) in found {
            self.found(id, ix, batch)?;
        }
        // 7. What is missing from where it was.
        self.missing()?;
        // 3–5. What has no id (or no longer has one).
        let mut unnamed = sorted.unnamed;
        unnamed.extend_from_slice(self.decisions.new_objects());
        unnamed.sort_by_key(|&ix| depth(&listing[ix].rel));
        unnamed.dedup();
        for ix in unnamed {
            if !self.decisions.taken(ix) {
                self.new_object(ix)?;
            }
        }
        self.tidy_skipped()
    }

    /// Whether this run looked at the place `rel`: the listing read it
    /// ([`Listing::examined`]), and the run did not give it up while acting.
    /// The one place that says so: nothing at a place not examined counts as
    /// missing, nothing new below it gets a row, and no line of the skipped
    /// list is taken off for it.
    fn examined(&self, rel: &Path) -> bool {
        self.listing.examined(rel) && !self.decisions.gave_up(rel)
    }

    /// The one policy for an entry that cannot be opened, stripped or read
    /// (`LO3`). Gone since it was listed, it is skipped (`None`). Refused to
    /// this daemon (`EACCES`, `EPERM`: another user's file, an immutable
    /// one), the trouble is the entry's own: it is passed over
    /// ([`pass_over`](Self::pass_over), `None`), never the batch's failure.
    /// Any other error may be anybody's (no descriptors, no memory, the
    /// disk): the batch fails, as it always did, and is offered again.
    fn entry_io<T>(&mut self, e: &Entry, tried: io::Result<T>) -> Result<Option<T>, ExamineError> {
        match tried {
            Ok(value) => Ok(Some(value)),
            Err(err) if gone(&err) => Ok(None),
            Err(err) if denied(&err) => {
                self.pass_over(e, &err);
                Ok(None)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// `e` is not examined in this run: given up, so that nothing at its
    /// place counts as missing, reported, and asked for again
    /// ([`Examined::passed`]).
    fn pass_over(&mut self, e: &Entry, why: &io::Error) {
        if self.decisions.give_up(&e.rel) {
            // One line for the run says how many (`Examiner::examine_reporting`).
            tracing::debug!("{} cannot be read ({why}); it is not examined", e.rel.display());
            self.outcome.out.unreadable.push(e.rel.clone());
        }
        self.outcome.out.passed.name(e.dir_rel(), &e.name);
    }

    /// The item id of the directory at `rel` as this run decided it:
    /// `None` for a directory new to OneDrive (its `mkdir` is pending).
    fn dir_id(&self, rel: &Path) -> Option<String> {
        if rel.as_os_str().is_empty() {
            return Some(self.facts.root_id.clone());
        }
        let ix = self.listing.at(rel)?;
        let id = self.decisions.id_of(self.listing, ix)?;
        (self.decisions.item(id) == Some(ix)).then(|| id.to_owned())
    }

    fn skip(&mut self, rel: &Path, reason: LocalSkip) {
        self.outcome.skipped.insert(rel.to_path_buf(), reason);
    }

    fn recheck(&mut self, e: &Entry) {
        self.outcome.out.recheck.name(e.dir_rel(), &e.name);
    }

    fn recheck_at(&mut self, rel: &Path) {
        if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
            self.outcome.out.recheck.name(parent, name);
        }
    }

    /// Item `id` is decided without a row: remembered for a folder it is
    /// in, and, when `report`, listed as undecided or unproven.
    fn hold_back(&mut self, id: &str, settle: Settle, report: bool) {
        self.decisions.settle(id, settle);
        match (settle, report) {
            (Settle::Wait, true) => self.outcome.out.undecided.push(id.to_owned()),
            (Settle::Unproven, true) => self.outcome.out.unproven.push(id.to_owned()),
            _ => {}
        }
    }
}

fn depth(rel: &Path) -> usize {
    rel.components().count()
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
