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
//!    elsewhere (stripped and created if downloaded, listed if not); on
//!    several inodes, the one whose handle the base records is I and the
//!    others are copies; where the base has it, its content is checked;
//!    elsewhere, it is a `move` too;
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
//! taken from under it: what changed since waits behind it. Everything found
//! is applied to the store in one transaction.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::lease;
use konedrive_fs::placeholder::{self, State, XATTR_ROOT};
use xattr::FileExt;

use super::batch::{Batch, DirScope};
use super::entry::{self, Entry, StateAttr, Type};
use super::ignore::IgnoreList;
use super::liveness::{Liveness, Whereabouts};
use super::names;
use super::{snapshot, MASS_DELETE_FLOOR, MASS_DELETE_ITEMS, MASS_DELETE_PERCENT, RECHECK};
use crate::drive::item::RESERVED_PREFIX;
use crate::sync::disk::{Disk, HOLDING, NEW_PREFIX};
use crate::sync::{InodeKey, InodeLocks};
use crate::tree::outbox::{is_under, Base, Detection, Inode, OutboxApplied, OutboxKind, OutboxOp, OutboxRow, OutboxState};
use crate::tree::{Kind, Placement, Row, Store, Table, TreeError};

/// A row's reason while a writer has the file open (§4.3).
pub const OPEN_FOR_WRITING: &str = "open-for-writing";
/// A held removal's reason (§3.4, the mass-delete guard).
pub const MASS_DELETE: &str = "mass-delete";
/// `local_skipped`'s reason for what is on another device than the folder
/// (a nested Btrfs subvolume, a mount): never uploaded (F72).
pub const OTHER_DEVICE: &str = "other-device";

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
    /// path that says nothing): examined again when it can answer.
    pub undecided: Vec<String>,
    /// Items the mass-delete guard held (0 when it did not trip).
    pub held: u64,
    /// Managed placeholders with more than one link, for `MarkFile`.
    pub mark_files: Vec<PathBuf>,
    /// Entries whose konedrive attributes were taken off: copies, files from
    /// elsewhere, editors' backups.
    pub stripped: Vec<PathBuf>,
    /// Places this daemon may not read (a directory set to `000`): not
    /// examined, so nothing in them counts as missing.
    pub unreadable: Vec<PathBuf>,
    /// The folder's filesystem had changed: its file handles were taken again
    /// (a Full scan), and nothing was decided by the old ones.
    pub renewed: bool,
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
        let (root_id, complete) = self.store.with(|s| Ok((s.root_item_id()?, s.delta_link()?.is_some() && s.listing_next()?.is_none())))?;
        let root_id = root_id.filter(|_| complete).ok_or(ExamineError::NoBase)?;
        let root = self.disk.dir(Path::new(""))?;
        let stat = nix::sys::stat::fstat(&root).map_err(io::Error::from)?;
        if stat.st_nlink == 0 || root.get_xattr(XATTR_ROOT)?.is_none() {
            return Err(ExamineError::RootGone);
        }
        let root_path = std::fs::read_link(entry::proc_path(&root)).ok();
        // The folder's filesystem changed since its handles were recorded (a new disk, a
        // snapshot rolled back): they are taken again, by a Full scan of everything, before
        // anything is decided by them.
        let full = Batch::full();
        let (batch, renewed) = match super::liveness::handles(self.store, &root) {
            super::liveness::Handles::Changed(now) => {
                let dropped = super::liveness::renew_handles(self.store, &now)?;
                tracing::warn!(
                    "the folder's filesystem is not the one its file handles were taken on: they are taken again, and \
                     {dropped} move(s) out of the folder whose object is not where it was are left to OneDrive"
                );
                (&full, true)
            }
            _ => (batch, false),
        };
        let handles_current = super::liveness::handles_current(self.store, &root);
        let rows = self.store.with(|s| s.outbox_rows())?;
        let mut run = Run {
            ex: self,
            root_id,
            root_dev: stat.st_dev as u64,
            root_path,
            handles_current,
            rows,
            entries: Vec::new(),
            at: HashMap::new(),
            whole: BTreeSet::new(),
            named: BTreeMap::new(),
            unreadable: HashSet::new(),
            base: HashMap::new(),
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
        run.out.renewed = renewed;
        run.list(batch)?;
        run.probe_expected()?;
        run.classify(batch)?;
        run.finish()
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
    root_id: String,
    /// The folder's device: nothing on another one is uploaded, nor looked
    /// into (a nested Btrfs subvolume, a mount).
    root_dev: u64,
    root_path: Option<PathBuf>,
    /// Whether the recorded handles are this filesystem's: `ESTALE` is gone
    /// only then (the move-out step).
    handles_current: bool,
    /// The live rows before this examination.
    rows: Vec<OutboxRow>,
    entries: Vec<Entry>,
    at: HashMap<PathBuf, usize>,
    whole: BTreeSet<PathBuf>,
    named: BTreeMap<PathBuf, BTreeSet<OsString>>,
    unreadable: HashSet<PathBuf>,
    base: HashMap<String, Option<Row>>,
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
    skipped: HashMap<PathBuf, String>,
    detections: Vec<Detection>,
    ops: Vec<OutboxOp>,
    out: Examined,
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

impl Run<'_, '_> {
    fn store<T>(&self, f: impl FnOnce(&mut crate::tree::TreeStore) -> Result<T, TreeError>) -> Result<T, TreeError> {
        self.ex.store.with(f)
    }

    fn base_row(&mut self, id: &str) -> Result<Option<Row>, TreeError> {
        if let Some(row) = self.base.get(id) {
            return Ok(row.clone());
        }
        let row = self.store(|s| s.get(Table::Items, id))?;
        self.base.insert(id.to_owned(), row.clone());
        Ok(row)
    }

    /// The live rows of item `id`, oldest first.
    fn rows_of(&self, id: &str) -> Vec<&OutboxRow> {
        self.rows.iter().filter(|row| row.item_id.as_deref() == Some(id)).collect()
    }

    /// The live row of a local object with no item id yet.
    fn pending_row(&self, e: &Entry) -> Option<&OutboxRow> {
        let inode = e.inode();
        self.rows.iter().rev().find(|row| row.item_id.is_none() && row.inode.as_ref().is_some_and(|i| i.same_object(&inode)))
    }

    /// Whether the worker is creating `e`'s object in OneDrive right now.
    fn being_created(&self, e: &Entry) -> bool {
        let inode = e.inode();
        self.rows
            .iter()
            .any(|row| row.item_id.is_none() && row.state == OutboxState::Running && row.inode.as_ref().is_some_and(|i| i.same_object(&inode)))
    }

    /// Where item `id` should be: where its live row last saw it, or else
    /// its base name under where its parent should be — so the items in a
    /// folder with a pending move are looked for where the folder is now.
    /// Cached, and derived from the parent's answer, so that a Full scan
    /// costs one lookup per item rather than one walk up the tree.
    fn expected(&mut self, id: &str) -> Result<Expect, TreeError> {
        self.expected_at(id, 0)
    }

    fn expected_at(&mut self, id: &str, depth: usize) -> Result<Expect, TreeError> {
        if let Some(expect) = self.expected.get(id) {
            return Ok(expect.clone());
        }
        let expect = match self.rows_of(id).last() {
            Some(row) if row.kind.removes() => Expect::Nowhere,
            Some(row) => Expect::At(row.rel.clone()),
            None => match self.base_row(id)? {
                None => Expect::Unknown,
                Some(row) if row.placement != Placement::Placed => Expect::Nowhere,
                Some(row) => match row.parent_id.as_deref() {
                    None => Expect::At(PathBuf::new()),
                    Some(parent) if parent == self.root_id => Expect::At(PathBuf::from(&row.name)),
                    // A cycle or corruption, not a drive.
                    Some(_) if depth > konedrive_fs::MAX_DEPTH => Expect::Nowhere,
                    Some(parent) => match self.expected_at(parent, depth + 1)? {
                        Expect::At(dir) => Expect::At(dir.join(&row.name)),
                        _ => Expect::Nowhere,
                    },
                },
            },
        };
        self.expected.insert(id.to_owned(), expect.clone());
        Ok(expect)
    }

    fn push(&mut self, e: Entry) -> usize {
        match self.at.get(&e.rel) {
            Some(&i) => {
                self.entries[i] = e;
                i
            }
            None => {
                self.at.insert(e.rel.clone(), self.entries.len());
                self.entries.push(e);
                self.entries.len() - 1
            }
        }
    }

    fn mark_unreadable(&mut self, rel: &Path, why: &io::Error) {
        if self.unreadable.insert(rel.to_path_buf()) {
            tracing::warn!("{} cannot be read ({why}); it is not examined", rel.display());
            self.out.unreadable.push(rel.to_path_buf());
        }
    }

    /// `name` in `dir`, or `None` when there is nothing there, or nothing
    /// this daemon may read (then noted as unreadable).
    fn read_entry(&mut self, dir: &File, dir_rel: &Path, name: &OsStr) -> Result<Option<Entry>, ExamineError> {
        match entry::read(dir, dir_rel, name) {
            Ok(e) => Ok(e),
            Err(e) if denied(&e) => {
                self.mark_unreadable(&dir_rel.join(name), &e);
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Reads `name` in the directory at `dir_rel`, if both are there.
    fn read_one(&mut self, dir_rel: &Path, name: &OsStr) -> Result<Option<usize>, ExamineError> {
        if let Some(&i) = self.at.get(&dir_rel.join(name)) {
            return Ok(Some(i));
        }
        let dir = match self.ex.disk.dir(dir_rel) {
            Ok(dir) => dir,
            Err(e) if gone(&e) => return Ok(None),
            Err(e) if denied(&e) => {
                self.mark_unreadable(dir_rel, &e);
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        Ok(self.read_entry(&dir, dir_rel, name)?.map(|e| self.push(e)))
    }

    /// Whether a directory's contents are new to the folder: no id, an id
    /// the base does not have as a folder, or a folder's id on another inode
    /// than the one recorded (a copy that kept its attributes).
    fn is_new_dir(&mut self, e: &Entry) -> Result<bool, TreeError> {
        let Some(id) = &e.id else { return Ok(true) };
        match self.base_row(id)? {
            Some(row) if row.kind == Kind::Folder => {
                let recorded = self.store(|s| s.local_handle(id))?;
                Ok(recorded.is_some() && e.handle.is_some() && recorded != e.handle)
            }
            _ => Ok(true),
        }
    }

    /// Reads every place the batch names, shallowest first; a new
    /// directory's contents with it.
    fn list(&mut self, batch: &Batch) -> Result<(), ExamineError> {
        let mut seeds: BTreeMap<PathBuf, (DirScope, bool)> = BTreeMap::new();
        fn seed(seeds: &mut BTreeMap<PathBuf, (DirScope, bool)>, rel: PathBuf, scope: DirScope, recurse: bool) {
            let slot = seeds.entry(rel).or_insert((DirScope::Names(BTreeSet::new()), false));
            slot.1 |= recurse;
            slot.0 = match (std::mem::replace(&mut slot.0, DirScope::Whole), scope) {
                (DirScope::Names(mut a), DirScope::Names(b)) => {
                    a.extend(b);
                    DirScope::Names(a)
                }
                _ => DirScope::Whole,
            };
        }
        if batch.full {
            seed(&mut seeds, PathBuf::new(), DirScope::Whole, true);
        }
        for (dir, scope) in &batch.dirs {
            seed(&mut seeds, dir.clone(), scope.clone(), false);
        }
        for dir in &batch.trees {
            seed(&mut seeds, dir.clone(), DirScope::Whole, true);
        }
        for handle in &batch.objects {
            if let Some(rel) = self.expected_of_handle(handle)? {
                if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
                    seed(&mut seeds, parent.to_path_buf(), DirScope::Names([name.to_owned()].into()), false);
                }
            }
        }
        let mut seeds: Vec<_> = seeds.into_iter().collect();
        seeds.sort_by_key(|(rel, _)| depth(rel));
        let mut queue: VecDeque<(PathBuf, DirScope, bool)> = seeds.into_iter().map(|(rel, (scope, recurse))| (rel, scope, recurse)).collect();
        let mut recursed: HashSet<PathBuf> = HashSet::new();
        while let Some((rel, scope, recurse)) = queue.pop_front() {
            if recurse && !recursed.insert(rel.clone()) {
                continue;
            }
            if !recurse && self.whole.contains(&rel) {
                continue;
            }
            self.list_dir(&rel, scope, recurse, &mut queue)?;
        }
        Ok(())
    }

    fn list_dir(&mut self, rel: &Path, scope: DirScope, recurse: bool, queue: &mut VecDeque<(PathBuf, DirScope, bool)>) -> Result<(), ExamineError> {
        if depth(rel) >= konedrive_fs::MAX_DEPTH {
            tracing::warn!("{} is deeper than {} levels; not examined", rel.display(), konedrive_fs::MAX_DEPTH);
            return Ok(());
        }
        if rel.components().any(|c| daemon_owned(c.as_os_str())) {
            return Ok(());
        }
        let dir = match self.ex.disk.dir(rel) {
            Ok(dir) => dir,
            Err(e) if gone(&e) => {
                // Gone since the event: what matters is that it is missing
                // from its parent.
                if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
                    queue.push_back((parent.to_path_buf(), DirScope::Names([name.to_owned()].into()), false));
                }
                return Ok(());
            }
            Err(e) if denied(&e) => {
                self.mark_unreadable(rel, &e);
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
            self.read_one(parent, name)?;
        }
        // On another device than the folder's: listed once as not uploaded
        // (its own entry, just read), and nothing inside it is examined.
        if nix::sys::stat::fstat(&dir).map_err(io::Error::from)?.st_dev as u64 != self.root_dev {
            return Ok(());
        }
        let mut whole = matches!(scope, DirScope::Whole);
        let mut read = Vec::new();
        if let DirScope::Names(names) = &scope {
            for name in names {
                match self.read_entry(&dir, rel, name)? {
                    Some(e) => read.push(e),
                    // Not there (a delete, a rename's old side, an O_TMPFILE
                    // pseudo-name, a merged event): the whole directory (§17).
                    None if !self.unreadable.contains(&rel.join(name)) => {
                        whole = true;
                        break;
                    }
                    None => {}
                }
            }
            if !whole {
                self.named.entry(rel.to_path_buf()).or_default().extend(names.iter().cloned());
            }
        }
        if whole {
            read.clear();
            let names = match self.ex.disk.list(&dir) {
                Ok(names) => names,
                Err(e) if denied(&e) => {
                    self.mark_unreadable(rel, &e);
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            };
            for name in names {
                if let Some(e) = self.read_entry(&dir, rel, &name)? {
                    read.push(e);
                }
            }
            self.whole.insert(rel.to_path_buf());
            self.named.remove(rel);
        }
        for e in read {
            // A directory of the user's own whose name is ignored stays local
            // with everything in it (the outbox on the bus): nothing below it is looked
            // at, so nothing below it waits for a folder never made in OneDrive.
            let ignored = e.id.is_none() && self.ex.ignore.matches(&e.name);
            if e.ty == Type::Dir && e.dev == self.root_dev && !daemon_owned(&e.name) && !ignored && (recurse || self.is_new_dir(&e)?) {
                queue.push_back((e.rel.clone(), DirScope::Whole, true));
            }
            self.push(e);
        }
        Ok(())
    }

    /// Where the item or pending row an event's object handle names is
    /// expected: the place to look.
    fn expected_of_handle(&mut self, handle: &FileHandle) -> Result<Option<PathBuf>, TreeError> {
        if let Some(item) = self.store(|s| s.item_by_handle(handle))? {
            if let Expect::At(rel) = self.expected(&item.id)? {
                return Ok(Some(rel));
            }
        }
        Ok(self.store(|s| s.outbox_by_handle(handle))?.map(|row| row.rel))
    }

    /// An item seen away from where it is expected is looked for there too:
    /// a copy that kept its attributes must meet its original.
    fn probe_expected(&mut self) -> Result<(), ExamineError> {
        let ids: BTreeSet<String> = self.entries.iter().filter_map(|e| e.id.clone()).collect();
        for id in ids {
            let Expect::At(rel) = self.expected(&id)? else { continue };
            let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else { continue };
            if self.at.contains_key(&rel) || self.whole.contains(parent) {
                continue;
            }
            self.read_one(parent, name)?;
            // The directory's own entry, for its id.
            if let (Some(above), Some(own)) = (parent.parent(), parent.file_name()) {
                self.read_one(above, own)?;
            }
        }
        Ok(())
    }

    /// The item id of the directory at `rel` as this batch decided it:
    /// `None` for a directory new to OneDrive (its `mkdir` is pending).
    fn dir_id(&self, rel: &Path) -> Option<String> {
        if rel.as_os_str().is_empty() {
            return Some(self.root_id.clone());
        }
        let &i = self.at.get(rel)?;
        let id = self.entries[i].id.as_ref()?;
        (self.chosen.get(id) == Some(&i)).then(|| id.clone())
    }

    /// Where the object `handle` names is: the liveness answer, placed. An
    /// answer is "outside" only when the path is sure and not beneath the
    /// root; "inside" only when the object's own handle stands at that place
    /// beneath the root. Anything else decides nothing.
    fn place_of(&self, handle: &FileHandle) -> Place {
        let path = match self.ex.liveness.whereabouts(handle) {
            Ok(Whereabouts::Gone) if self.handles_current => return Place::Gone,
            Ok(Whereabouts::Gone) => return Place::Unknown,
            Ok(Whereabouts::At(path)) => path,
            Err(err) => {
                tracing::debug!("an object's whereabouts cannot be asked yet: {err}");
                return Place::Unknown;
            }
        };
        let Some(root) = &self.root_path else { return Place::Unknown };
        let sure = path.is_absolute()
            && !path.as_os_str().as_bytes().ends_with(b" (deleted)")
            && path.components().all(|c| !matches!(c, Component::ParentDir | Component::CurDir));
        if !sure {
            return Place::Unknown;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            return Place::Outside(path);
        };
        let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else { return Place::Unknown };
        match self.ex.disk.dir(parent).ok().and_then(|dir| FileHandle::at(&dir, name).ok()) {
            Some(there) if &there == handle => Place::Inside(rel.to_path_buf()),
            _ => Place::Unknown,
        }
    }

    /// Whether the object `handle` names is proved absent from its place `rel`
    /// beneath the root ([`super::liveness::absent_below`]).
    fn absent(&self, rel: &Path, handle: &FileHandle) -> bool {
        let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else { return false };
        super::liveness::absent_below(self.ex.disk.dir(parent), name, handle)
    }

    /// [`absent`](Self::absent) for `at`, inside the folder at `folder` — which
    /// is now at `went_to` if it left the folder.
    fn absent_with(&self, at: &Path, folder: &Path, went_to: Option<&Path>, handle: &FileHandle) -> bool {
        match (went_to, at.strip_prefix(folder)) {
            (Some(went), Ok(inside)) => super::liveness::absent_at(&went.join(inside), handle),
            (Some(_), Err(_)) => false,
            (None, _) => self.absent(at, handle),
        }
    }

    fn skip(&mut self, rel: &Path, reason: &str) {
        self.skipped.insert(rel.to_path_buf(), reason.to_owned());
    }

    fn recheck(&mut self, e: &Entry) {
        self.out.recheck.name(e.dir_rel(), &e.name);
    }

    /// Item `id` is decided without a row: remembered for a folder it is
    /// in, and, when `report`, listed as undecided or unproven.
    fn hold_back(&mut self, id: &str, settle: Settle, report: bool) {
        self.decided.insert(id.to_owned());
        self.deferred.insert(id.to_owned(), settle);
        match (settle, report) {
            (Settle::Wait, true) => self.out.undecided.push(id.to_owned()),
            (Settle::Unproven, true) => self.out.unproven.push(id.to_owned()),
            _ => {}
        }
    }

    fn recheck_at(&mut self, rel: &Path) {
        if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
            self.out.recheck.name(parent, name);
        }
    }

    fn classify(&mut self, batch: &Batch) -> Result<(), ExamineError> {
        let mut by_id: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut unnamed: Vec<usize> = Vec::new();
        for i in 0..self.entries.len() {
            let e = &self.entries[i];
            // 1. The daemon's own names; a user's `.konedrive-*` is listed.
            if daemon_owned(&e.name) {
                continue;
            }
            if e.name.as_bytes().starts_with(RESERVED_PREFIX.as_bytes()) {
                let rel = e.rel.clone();
                self.skip(&rel, "reserved-name");
                continue;
            }
            // 2. Never a OneDrive object — unless its name is ignored anyway
            // (Emacs's `.#name` lock is a symlink).
            if let Some(reason) = e.ty.skip_reason() {
                if !self.ex.ignore.matches(&e.name) {
                    let rel = e.rel.clone();
                    self.skip(&rel, reason);
                }
                continue;
            }
            // 2b. On another device than the folder's (a nested Btrfs
            // subvolume, a mount): never uploaded, nor anything below it —
            // the helper cannot protect what is placed there (F72).
            if e.dev != self.root_dev {
                let rel = e.rel.clone();
                self.skip(&rel, OTHER_DEVICE);
                continue;
            }
            match &e.id {
                Some(id) => by_id.entry(id.clone()).or_default().push(i),
                None => unnamed.push(i),
            }
        }
        // 6. Who is who, before anything is decided by place: a directory's
        // id says what its entries' parent is.
        let mut found: Vec<(String, usize)> = Vec::new();
        for (id, entries) in &by_id {
            if let Some(chosen) = self.resolve(id, entries)? {
                found.push((id.clone(), chosen));
            }
        }
        found.sort_by_key(|(_, i)| depth(&self.entries[*i].rel));
        for (id, i) in found {
            self.found(&id, i, batch)?;
        }
        // 7. What is missing from where it was.
        self.missing()?;
        // 3–5. What has no id (or no longer has one).
        unnamed.extend(std::mem::take(&mut self.fresh));
        unnamed.sort_by_key(|&i| depth(&self.entries[i].rel));
        unnamed.dedup();
        for i in unnamed {
            if !self.consumed.contains(&i) {
                self.unnamed(i)?;
            }
        }
        self.tidy_skipped(batch.full)?;
        Ok(())
    }

    /// Decides which of the entries carrying `id` is the item, and what the
    /// others are. Returns the item's entry, if one is.
    fn resolve(&mut self, id: &str, entries: &[usize]) -> Result<Option<usize>, ExamineError> {
        let Some(base) = self.base_row(id)? else {
            // Unknown to the base.
            let placing = self.store(|s| s.get(Table::Staging, id))?.is_some();
            for &i in entries {
                let e = self.entries[i].clone();
                if placing {
                    // A reconcile is placing it right now; its swap follows.
                    self.recheck(&e);
                } else if self.pending_row(&e).is_some() {
                    // A create or mkdir between its two commit steps (§5): its
                    // replay adopts it.
                } else {
                    self.stranger(i)?;
                }
            }
            return Ok(None);
        };
        let want = if base.kind == Kind::Folder { Type::Dir } else { Type::File };
        let mut same: Vec<usize> = Vec::new();
        for &i in entries {
            if self.entries[i].ty == want {
                same.push(i);
            } else {
                // Its own id with the other kind: not the item.
                self.stranger(i)?;
            }
        }
        if same.is_empty() {
            return Ok(None);
        }
        // Group by object: one inode may have several names (hard links).
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for &i in &same {
            match groups.iter_mut().find(|g| self.entries[g[0]].same_object(&self.entries[i])) {
                Some(group) => group.push(i),
                None => groups.push(vec![i]),
            }
        }
        for group in &mut groups {
            group.sort_by(|&a, &b| self.entries[a].rel.cmp(&self.entries[b].rel));
        }
        let recorded = self.store(|s| s.local_handle(id))?;
        let expect = self.expected(id)?;
        let expected_rel = match &expect {
            Expect::At(rel) => Some(rel.clone()),
            _ => None,
        };
        let base_rel = self.store(|s| s.locate(Table::Items, id))?.filter(|l| l.placed).map(|l| l.rel);
        let is_at = |run: &Self, g: &[usize], rel: &Option<PathBuf>| rel.as_ref().is_some_and(|r| g.iter().any(|&i| &run.entries[i].rel == r));
        let original = recorded.as_ref().and_then(|h| groups.iter().position(|g| self.entries[g[0]].handle.as_ref() == Some(h)));

        // Rename to a backup, write new (vim's `file~`): the recorded inode
        // sits under an ignored name beside where the item is expected, and
        // another file stands there.
        if base.kind == Kind::File {
            if let (Some(o), Some(rel)) = (original, &expected_rel) {
                let group = groups[o].clone();
                let beside = group.iter().all(|&i| self.entries[i].dir_rel() == rel.parent().unwrap_or(Path::new("")))
                    && group.iter().any(|&i| self.ex.ignore.matches(&self.entries[i].name));
                // A new file, or one that copied the old one's attributes —
                // not one the worker is creating right now.
                let newcomer = self.at.get(rel).copied().filter(|&s| {
                    let e = &self.entries[s];
                    !group.contains(&s) && e.ty == Type::File && (e.id.is_none() || e.id.as_deref() == Some(id)) && !self.being_created(e)
                });
                if let (true, Some(s)) = (beside && !is_at(self, &group, &expected_rel), newcomer) {
                    self.backup(id, &base, &group, s)?;
                    return Ok(None);
                }
            }
        }

        let at_place = groups.iter().position(|g| is_at(self, g, &expected_rel)).or_else(|| groups.iter().position(|g| is_at(self, g, &base_rel)));
        let pick = match (original, at_place, recorded) {
            (Some(o), _, _) => o,
            // The one where the item is: an editor's new inode that copied
            // its attributes.
            (None, Some(p), _) => p,
            (None, None, None) => {
                tracing::warn!("{id} is on {} inodes, none of them where it was: the first by path is taken", groups.len());
                0
            }
            // None of these is the inode the base records, and none stands
            // where the item is: where is the recorded one (§3.4)?
            (None, None, Some(handle)) => match self.place_of(&handle) {
                Place::Gone => {
                    tracing::warn!("{id} is on {} inodes, none of them where it was: the first by path is taken", groups.len());
                    0
                }
                Place::Outside(to) => {
                    // It left the folder: these are copies it left behind.
                    for &i in &same {
                        self.stranger(i)?;
                    }
                    if let Some(rel) = &expected_rel {
                        self.removal(OutboxKind::MoveOut, id, &base, rel, Some(object(handle)), Some(to.as_path()))?;
                    }
                    self.decided.insert(id.to_owned());
                    return Ok(None);
                }
                Place::Inside(now) => {
                    self.recheck_at(&now);
                    self.hold_back(id, Settle::Wait, false);
                    return Ok(None);
                }
                Place::Unknown => {
                    self.hold_back(id, Settle::Wait, true);
                    return Ok(None);
                }
            },
        };
        let group = groups[pick].clone();
        let chosen = group
            .iter()
            .copied()
            .find(|&i| Some(&self.entries[i].rel) == expected_rel.as_ref())
            .unwrap_or(group[0]);
        self.chosen.insert(id.to_owned(), chosen);
        self.consumed.insert(chosen);
        // Other names of the same object: a hard link OneDrive cannot hold.
        for &i in group.iter().filter(|&&i| i != chosen) {
            self.consumed.insert(i);
            let e = self.entries[i].clone();
            if !self.ex.ignore.matches(&e.name) {
                self.skip(&e.rel, "hard-link");
            }
        }
        // Other objects with its id: copies that kept its attributes.
        for (n, other) in groups.iter().enumerate() {
            if n != pick {
                for &i in other {
                    self.stranger(i)?;
                }
            }
        }
        Ok(Some(chosen))
    }

    /// An entry with an item id that is not the item's (a copy, a file from
    /// another folder or account, the wrong kind): downloaded, it is the
    /// user's own file, stripped and uploaded as new; a directory likewise,
    /// with its contents. A placeholder cannot be read here and is listed; so
    /// is a file with other links, whose other names stripping would change
    /// too.
    fn stranger(&mut self, i: usize) -> Result<(), ExamineError> {
        let e = self.entries[i].clone();
        let dir = match self.ex.disk.dir(e.dir_rel()) {
            Ok(dir) => dir,
            Err(err) if gone(&err) => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let listed = |run: &mut Self, reason: &str| {
            if !run.ex.ignore.matches(&e.name) {
                run.skip(&e.rel, reason);
            }
            run.consumed.insert(i);
        };
        match e.ty {
            Type::Dir => {
                placeholder::strip_konedrive_xattrs(&self.ex.disk.open_subdir(&dir, &e.name)?)?;
                if !self.whole.contains(&e.rel) {
                    self.out.recheck.tree(&e.rel);
                }
            }
            Type::File if e.hydrated() && e.nlink > 1 => {
                listed(self, "hard-link");
                return Ok(());
            }
            Type::File if e.hydrated() => placeholder::strip_konedrive_xattrs(&self.ex.disk.open_file(&dir, &e.name)?)?,
            _ => {
                listed(self, "not-downloaded");
                return Ok(());
            }
        }
        self.out.stripped.push(e.rel.clone());
        self.entries[i].id = None;
        self.entries[i].state = StateAttr::Absent;
        self.fresh.push(i);
        Ok(())
    }

    /// Save-by-rename with a backup: item `id`'s inode (`group`) now sits
    /// under an ignored name, and `s` stands where the item is. `s` is the
    /// item's new content; the backup stays the user's, stripped if it was
    /// downloaded (a placeholder stays managed and fills on open).
    fn backup(&mut self, id: &str, base: &Row, group: &[usize], s: usize) -> Result<(), ExamineError> {
        for &i in group {
            self.consumed.insert(i);
            let e = self.entries[i].clone();
            if e.hydrated() {
                let dir = self.ex.disk.dir(e.dir_rel())?;
                placeholder::strip_konedrive_xattrs(&self.ex.disk.open_file(&dir, &e.name)?)?;
                self.out.stripped.push(e.rel);
            }
        }
        self.consumed.insert(s);
        self.chosen.insert(id.to_owned(), s);
        if self.entries[s].id.is_some() {
            // A new file that copied the old one's attributes (vim does):
            // its stamp and cTag are the old version's, not its own.
            self.entries[s].state = StateAttr::Absent;
        }
        self.save_by_rename(id, base, s)
    }

    /// Item `id`'s content is now the file `s` at its place: an `update`
    /// from the new inode, which takes over the item at commit. A pending
    /// create of that file goes (never one being sent: callers exclude it).
    fn save_by_rename(&mut self, id: &str, base: &Row, s: usize) -> Result<(), ExamineError> {
        let e = self.entries[s].clone();
        if let Some(row) = self.pending_row(&e).filter(|row| row.state != OutboxState::Running) {
            self.ops.push(OutboxOp::Remove(row.seq));
        }
        let (state, reason, next_try) = self.probe_writer(&e)?;
        let mut d = self.detection(OutboxKind::Update, id, base, &e, None);
        (d.state, d.reason, d.next_try) = (state, reason, next_try);
        self.detections.push(d);
        Ok(())
    }

    fn detection(&self, kind: OutboxKind, id: &str, base: &Row, e: &Entry, local_ctag: Option<&str>) -> Detection {
        // The version the local content derives from: the file's own cTag
        // when it names another than the base's (a download not yet
        // replaced); the eTag guards only the base's own version.
        let same_version = local_ctag.is_none_or(|c| Some(c) == base.ctag.as_deref());
        Detection {
            kind,
            item_id: Some(id.to_owned()),
            inode: Some(e.inode()),
            rel: e.rel.clone(),
            base: Some(Base {
                etag: if same_version { base.etag.clone() } else { None },
                ctag: if same_version { base.ctag.clone() } else { local_ctag.map(str::to_owned) },
                parent: base.parent_id.clone(),
                name: Some(base.name.clone()),
            }),
            target_parent: self.dir_id(e.dir_rel()),
            target_name: Some(lossy(&e.name)),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
        }
    }

    /// Item `id` found as entry `i`: where it is, and its content.
    fn found(&mut self, id: &str, i: usize, batch: &Batch) -> Result<(), ExamineError> {
        let e = self.entries[i].clone();
        let Some(base) = self.base_row(id)? else { return Ok(()) };
        let recorded = self.store(|s| s.local_handle(id))?;
        if e.handle.is_some() && e.handle != recorded {
            self.ops.push(OutboxOp::SetHandle { item_id: id.to_owned(), handle: e.handle.clone() });
        }
        if e.ty == Type::File && e.nlink > 1 && matches!(e.state, StateAttr::Known(State::OnlineOnly | State::Hydrating | State::Dehydrating)) {
            self.out.mark_files.push(e.rel.clone());
        }
        if e.ty == Type::Dir {
            if let Expect::At(was) = self.expected(id)? {
                if was != e.rel {
                    self.ops.push(OutboxOp::Rebase { from: was, to: e.rel.clone() });
                }
            }
        }
        let content = if e.ty == Type::File { self.content(id, &base, &e, batch)? } else { Content::Same };
        let mut d = self.detection(OutboxKind::Move, id, &base, &e, e.ctag.as_deref());
        match content {
            Content::Changed => d.kind = OutboxKind::Update,
            Content::Waiting => {
                d.kind = OutboxKind::Update;
                d.state = OutboxState::Waiting;
                d.reason = Some(OPEN_FOR_WRITING.into());
                d.next_try = Some(self.ex.now + RECHECK.as_secs() as i64);
            }
            Content::Same => d.same_content = true,
            Content::Unknown => {}
        }
        // Renamed to a name OneDrive refuses: blocked until renamed again.
        if e.name != OsStr::new(&base.name) {
            if let Some(refused) = names::refused(&e.name) {
                d.state = OutboxState::Blocked;
                d.reason = Some(refused.as_str().into());
                d.next_try = None;
            }
        }
        let at_base = d.target_parent.as_deref() == base.parent_id.as_deref() && d.target_name.as_deref() == Some(base.name.as_str());
        // In place, unchanged or unknown, with no row: nothing to record.
        if d.kind == OutboxKind::Move && at_base && self.rows_of(id).is_empty() {
            return Ok(());
        }
        self.detections.push(d);
        Ok(())
    }

    /// The content check (§3.4) of item `id`'s file `e` against `base`.
    fn content(&mut self, id: &str, base: &Row, e: &Entry, batch: &Batch) -> Result<Content, ExamineError> {
        match e.state {
            StateAttr::Known(State::Hydrating | State::Dehydrating) => {
                self.recheck(e);
                Ok(Content::Unknown)
            }
            StateAttr::Known(State::OnlineOnly) => {
                if e.size != base.size {
                    self.restore(e, base)?;
                }
                Ok(Content::Same)
            }
            StateAttr::Absent | StateAttr::Corrupt => {
                tracing::warn!("{} carries an item id and no state konedrive can read; it is left alone", e.rel.display());
                Ok(Content::Unknown)
            }
            StateAttr::Known(State::Hydrated) => self.hydrated(id, base, e, batch),
        }
    }

    fn hydrated(&mut self, id: &str, base: &Row, e: &Entry, batch: &Batch) -> Result<Content, ExamineError> {
        let now = snapshot(e.size, e.mtime.0, e.mtime.1);
        if self.rows_of(id).iter().any(|row| row.state == OutboxState::Running && row.snapshot.as_deref() == Some(now.as_str())) {
            // Being uploaded as it is now.
            return Ok(Content::Unknown);
        }
        let size_changed = e.stamp.is_some_and(|s| s.size != e.size);
        let time_changed = e.stamp.is_none_or(|s| (s.mtime_sec, s.mtime_nsec) != e.mtime);
        let written = e.handle.as_ref().is_some_and(|h| batch.written_handles.contains(h)) || batch.written_rels.contains(&e.rel);
        if !size_changed && !time_changed && !written {
            return Ok(Content::Same);
        }
        let Some(file) = self.open_same(e)? else {
            self.recheck(e);
            return Ok(Content::Unknown);
        };
        if !matches!(placeholder::read_state(&file), Ok(Some(State::Hydrated))) {
            self.recheck(e);
            return Ok(Content::Unknown);
        }
        match lease::open_for_writing(&file) {
            Ok(true) => {
                self.recheck(e);
                return Ok(Content::Waiting);
            }
            Ok(false) => {}
            Err(err) => tracing::warn!("cannot tell whether {} is open for writing ({err}); examined as it is", e.rel.display()),
        }
        if size_changed {
            return Ok(Content::Changed);
        }
        // Same size: only the hash tells an edit from a touch. Against the
        // base's hash only when the file is the base's version.
        let same_version = e.ctag.as_deref().is_none_or(|c| Some(c) == base.ctag.as_deref());
        let (true, Some(expected)) = (same_version, base.quickxor.as_deref()) else {
            return Ok(Content::Changed);
        };
        let before = size_and_time(&file)?;
        if hash(&file)? != expected {
            return Ok(Content::Changed);
        }
        if size_and_time(&file)? != before {
            // Written while it was being read: the hash says nothing.
            self.recheck(e);
            return Ok(Content::Unknown);
        }
        // Only the time changed: a `touch` uploads nothing.
        if let Err(err) = placeholder::write_stamp(&file) {
            tracing::warn!("cannot refresh the stamp of {}: {err}", e.rel.display());
        }
        Ok(Content::Same)
    }

    /// `e`, opened read-only, if it is still the same object.
    fn open_same(&self, e: &Entry) -> Result<Option<File>, ExamineError> {
        let dir = match self.ex.disk.dir(e.dir_rel()) {
            Ok(dir) => dir,
            Err(err) if gone(&err) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let file = match self.ex.disk.open_file(&dir, &e.name) {
            Ok(file) => file,
            Err(err) if gone(&err) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let stat = nix::sys::stat::fstat(&file).map_err(io::Error::from)?;
        let same = match (FileHandle::of(&file).ok(), &e.handle) {
            (Some(a), Some(b)) => &a == b,
            _ => stat.st_dev as u64 == e.dev && stat.st_ino as u64 == e.ino,
        };
        Ok(same.then_some(file))
    }

    /// A placeholder whose size a `truncate(2)` changed gets the cloud's
    /// size and time back. Through the daemon's own descriptor (its opens
    /// are never intercepted, so nothing is filled), never a name a symlink
    /// could redirect, and under the per-inode lock a fill holds for its
    /// whole run, with the state read again under it.
    fn restore(&mut self, e: &Entry, base: &Row) -> Result<(), ExamineError> {
        let Some(file) = self.open_same(e)? else {
            self.recheck(e);
            return Ok(());
        };
        let Some(_guard) = self.ex.locks.try_lock(InodeKey::of(&file)?) else {
            self.recheck(e);
            return Ok(());
        };
        let writable = placeholder::reopen_writable(&file)?;
        drop(file);
        if !matches!(placeholder::read_state(&writable), Ok(Some(State::OnlineOnly))) {
            self.recheck(e);
            return Ok(());
        }
        writable.set_len(base.size)?;
        placeholder::set_mtime(&writable, SystemTime::UNIX_EPOCH + Duration::from_secs(base.mtime.max(0) as u64))?;
        tracing::info!("{} was cut to {} bytes while not downloaded; it has the cloud's size again", e.rel.display(), e.size);
        self.out.restored.push(e.rel.clone());
        Ok(())
    }

    /// Whether a writer holds the new file `e`: `waiting` then, `ready`
    /// otherwise.
    fn probe_writer(&mut self, e: &Entry) -> Result<(OutboxState, Option<String>, Option<i64>), ExamineError> {
        let busy = match self.open_same(e)? {
            Some(file) => lease::open_for_writing(&file).unwrap_or_else(|err| {
                tracing::warn!("cannot tell whether {} is open for writing ({err})", e.rel.display());
                false
            }),
            None => false,
        };
        if busy {
            self.recheck(e);
            return Ok((OutboxState::Waiting, Some(OPEN_FOR_WRITING.into()), Some(self.ex.now + RECHECK.as_secs() as i64)));
        }
        Ok((OutboxState::Ready, None, None))
    }

    /// Rule 7: base items (and pending rows) expected in the examined
    /// places and not found anywhere in the batch.
    fn missing(&mut self) -> Result<(), ExamineError> {
        let mut places: Vec<(PathBuf, Option<BTreeSet<OsString>>)> = self.whole.iter().map(|d| (d.clone(), None)).collect();
        places.extend(self.named.iter().map(|(d, n)| (d.clone(), Some(n.clone()))));
        for (dir, names) in places {
            let in_scope = |run: &Self, rel: &Path| {
                !run.unreadable.contains(rel) && names.as_ref().is_none_or(|n| rel.file_name().is_some_and(|f| n.contains(f)))
            };
            let mut items: Vec<(String, PathBuf)> = Vec::new();
            if let Some(parent) = self.dir_id(&dir) {
                for child in self.store(|s| s.children(Table::Items, &parent))? {
                    if child.placement != Placement::Placed {
                        continue;
                    }
                    self.base.entry(child.id.clone()).or_insert_with(|| Some(child.clone()));
                    if let Expect::At(rel) = self.expected(&child.id)? {
                        if rel.parent() == Some(dir.as_path()) && in_scope(self, &rel) {
                            items.push((child.id.clone(), rel));
                        }
                    }
                }
            }
            let mut pending: Vec<OutboxRow> = Vec::new();
            for row in &self.rows {
                if row.kind.removes() || row.rel.parent() != Some(dir.as_path()) || !in_scope(self, &row.rel) {
                    continue;
                }
                match &row.item_id {
                    Some(id) => {
                        if !items.iter().any(|(i, _)| i == id) && self.rows_of(id).last().map(|r| r.seq) == Some(row.seq) {
                            items.push((id.clone(), row.rel.clone()));
                        }
                    }
                    None => pending.push(row.clone()),
                }
            }
            for (id, rel) in items {
                if !self.chosen.contains_key(&id) && !self.decided.contains(&id) {
                    self.missing_item(&id, &rel)?;
                }
            }
            for row in pending {
                let seen = row.inode.as_ref().is_some_and(|inode| self.entries.iter().any(|e| e.inode().same_object(inode)));
                if !seen {
                    self.missing_pending(&row)?;
                }
            }
        }
        Ok(())
    }

    /// A pending create's or mkdir's object is gone, with what waited
    /// inside a new directory.
    fn missing_pending(&mut self, row: &OutboxRow) -> Result<(), ExamineError> {
        self.gone_pending(row);
        if row.kind != OutboxKind::Mkdir {
            return Ok(());
        }
        let inside: Vec<OutboxRow> = self.rows.iter().filter(|r| is_under(&r.rel, &row.rel)).cloned().collect();
        for r in inside {
            match &r.item_id {
                None => self.gone_pending(&r),
                Some(id) if !r.kind.removes() && !self.chosen.contains_key(id) && !self.decided.contains(id) => {
                    let id = id.clone();
                    self.missing_item(&id, &r.rel)?;
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// A pending row whose object is gone: never sent, so nothing to take
    /// back. One the worker is sending right now is not taken from under it:
    /// a delete waits behind it, and learns the item id at its commit.
    fn gone_pending(&mut self, row: &OutboxRow) {
        if row.state != OutboxState::Running {
            self.ops.push(OutboxOp::Remove(row.seq));
            return;
        }
        self.detections.push(Detection {
            kind: OutboxKind::Delete,
            item_id: None,
            inode: row.inode.clone(),
            rel: row.rel.clone(),
            base: None,
            target_parent: None,
            target_name: None,
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
        });
    }

    /// Item `id`, expected at `rel`, is not in the batch.
    fn missing_item(&mut self, id: &str, rel: &Path) -> Result<(), ExamineError> {
        let Some(base) = self.base_row(id)? else { return Ok(()) };
        // Save-by-rename: a new file now stands at its name — not one the
        // worker is creating right now.
        if base.kind == Kind::File {
            if let Some(&s) = self.at.get(rel) {
                let e = &self.entries[s];
                if e.ty == Type::File && e.id.is_none() && !self.consumed.contains(&s) && !self.being_created(e) {
                    self.consumed.insert(s);
                    self.chosen.insert(id.to_owned(), s);
                    return self.save_by_rename(id, &base, s);
                }
            }
        }
        let Some(handle) = self.store(|s| s.local_handle(id))? else {
            // A rebuilt base cannot prove a delete (WR4).
            self.hold_back(id, Settle::Unproven, true);
            return Ok(());
        };
        match self.place_of(&handle) {
            // `ESTALE` is a delete only with its evidence: nothing, or another object, at its
            // place.
            Place::Gone if self.absent(rel, &handle) => self.removal(OutboxKind::Delete, id, &base, rel, None, None).map(drop),
            Place::Gone => {
                self.hold_back(id, Settle::Wait, true);
                Ok(())
            }
            Place::Outside(to) => self.removal(OutboxKind::MoveOut, id, &base, rel, Some(object(handle)), Some(to.as_path())).map(drop),
            // Moved within the folder, somewhere this batch did not look.
            Place::Inside(now) => {
                self.recheck_at(&now);
                self.hold_back(id, Settle::Wait, false);
                Ok(())
            }
            Place::Unknown => {
                self.hold_back(id, Settle::Wait, true);
                Ok(())
            }
        }
    }

    /// Item `id` leaves OneDrive (`delete`, or `move-out` to `went_to`). A
    /// folder takes along what the base has inside it — except what left it
    /// first, which leaves on its own and is ordered in front of it (rule
    /// 3). Every item still with the folder is asked where it is (§3.4 rule
    /// 7: by the object, not the events); while any cannot be placed, the
    /// folder waits. Rows that already say an item left are kept. What never
    /// reached the cloud goes; an item moved in from elsewhere, which the
    /// cloud has elsewhere, leaves by its own object. Rows the worker is
    /// running are never removed; a moved-in item's gets its follow-up.
    /// Whether its row was written, or why not.
    fn removal(&mut self, kind: OutboxKind, id: &str, base: &Row, rel: &Path, inode: Option<Inode>, went_to: Option<&Path>) -> Result<Settle, ExamineError> {
        if self.decided.contains(id) {
            return Ok(self.deferred.get(id).copied().unwrap_or(Settle::Done));
        }
        self.decided.insert(id.to_owned());
        if base.kind == Kind::Folder {
            match self.left_before(id, rel, went_to)? {
                Settle::Done => {}
                // Something inside is elsewhere in the folder, or cannot be
                // placed: the folder is examined again, and removed then.
                Settle::Wait => {
                    self.hold_back(id, Settle::Wait, true);
                    self.recheck_at(rel);
                    return Ok(Settle::Wait);
                }
                // Something inside has no recorded handle: nothing can prove
                // it gone until the reconcile places it again.
                Settle::Unproven => {
                    self.hold_back(id, Settle::Unproven, true);
                    return Ok(Settle::Unproven);
                }
            }
            let inside: HashSet<String> = self.store(|s| s.descendants(Table::Items, id))?.into_iter().collect();
            let rows: Vec<OutboxRow> =
                self.rows.iter().filter(|r| is_under(&r.rel, rel) || r.item_id.as_ref().is_some_and(|i| inside.contains(i))).cloned().collect();
            for r in rows {
                if r.state == OutboxState::Running {
                    match r.item_id.clone() {
                        // A create under its path had its object go with it.
                        None if is_under(&r.rel, rel) => self.gone_pending(&r),
                        // Moved in from elsewhere while being sent: it leaves
                        // by its own object, behind the running row.
                        Some(item) if !inside.contains(&item) && !self.chosen.contains_key(&item) && !self.decided.contains(&item) => {
                            self.missing_item(&item, &r.rel)?;
                        }
                        // What the base has inside waits in front of the
                        // folder (rule 3).
                        _ => {}
                    }
                    continue;
                }
                match r.item_id.clone() {
                    None => self.ops.push(OutboxOp::Remove(r.seq)),
                    Some(item) if self.chosen.contains_key(&item) || self.decided.contains(&item) => {}
                    // Left before the folder: kept.
                    Some(_) if r.kind == OutboxKind::MoveOut => {}
                    Some(item) if inside.contains(&item) => {
                        // Gone with the folder in the cloud too — unless the
                        // row takes it elsewhere in the folder.
                        if r.kind == OutboxKind::Delete || is_under(&r.rel, rel) {
                            self.ops.push(OutboxOp::Remove(r.seq));
                        }
                    }
                    Some(_) if r.kind == OutboxKind::Delete => {}
                    Some(item) => self.missing_item(&item, &r.rel)?,
                }
            }
        }
        self.detections.push(Detection {
            kind,
            item_id: Some(id.to_owned()),
            inode,
            rel: rel.to_path_buf(),
            base: Some(base_of(base)),
            target_parent: None,
            // Where a move out went, proved: what a later `ESTALE` is checked against.
            target_name: went_to.filter(|_| kind == OutboxKind::MoveOut).and_then(Path::to_str).map(str::to_owned),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
        });
        Ok(Settle::Done)
    }

    /// Items the base has inside `folder` (at `rel`), which is leaving: each
    /// is asked where it is, top down, unless this batch decided it or a row
    /// of its own already takes it elsewhere or out. One alive outside the
    /// folder (and not where the folder went, `went_to`) left it first: its
    /// own `move-out`, which downloads it before anything is deleted (WR5).
    /// One gone went with the folder, and so did one under `went_to`; what is
    /// inside those is asked too. How the folder may go: one alive elsewhere
    /// in the folder or unplaceable keeps it waiting, one with no
    /// recorded handle keeps it unproven, and so does an item held back
    /// on its own — a subfolder that left and waits itself.
    fn left_before(&mut self, folder: &str, rel: &Path, went_to: Option<&Path>) -> Result<Settle, ExamineError> {
        let mut settled = Settle::Done;
        let mut queue: VecDeque<String> = VecDeque::from([folder.to_owned()]);
        while let Some(parent) = queue.pop_front() {
            for child in self.store(|s| s.children(Table::Items, &parent))? {
                if child.placement != Placement::Placed {
                    continue;
                }
                let id = child.id.clone();
                self.base.entry(id.clone()).or_insert_with(|| Some(child.clone()));
                if self.chosen.contains_key(&id) {
                    continue;
                }
                if self.decided.contains(&id) {
                    settled = settled.max(self.deferred.get(&id).copied().unwrap_or(Settle::Done));
                    continue;
                }
                let Expect::At(at) = self.expected(&id)? else { continue };
                if !is_under(&at, rel) {
                    // A row of its own takes it elsewhere.
                    continue;
                }
                let Some(handle) = self.store(|s| s.local_handle(&id))? else {
                    settled = settled.max(Settle::Unproven);
                    continue;
                };
                match self.place_of(&handle) {
                    Place::Outside(to) if went_to.is_none_or(|went| !to.starts_with(went)) => {
                        let left = self.removal(OutboxKind::MoveOut, &id, &child, &at, Some(object(handle)), Some(to.as_path()))?;
                        settled = settled.max(left);
                    }
                    // Gone with the folder only with its evidence, where the folder is now.
                    Place::Gone if !self.absent_with(&at, rel, went_to, &handle) => settled = settled.max(Settle::Wait),
                    Place::Gone | Place::Outside(_) => {
                        if child.kind == Kind::Folder {
                            queue.push_back(id);
                        }
                    }
                    Place::Inside(now) => {
                        self.recheck_at(&now);
                        settled = settled.max(Settle::Wait);
                    }
                    Place::Unknown => settled = settled.max(Settle::Wait),
                }
            }
        }
        Ok(settled)
    }

    /// Whether `dir` is, or is inside, a directory without an item id whose
    /// name is ignored: what is in it stays local (the outbox on the bus). A folder
    /// from OneDrive syncs whatever its name.
    fn in_ignored_dir(&self, dir: &Path) -> bool {
        let mut at = dir;
        while let (Some(name), Some(parent)) = (at.file_name(), at.parent()) {
            if self.ex.ignore.matches(name) {
                let managed = self
                    .ex
                    .disk
                    .dir(parent)
                    .and_then(|d| self.ex.disk.probe(&d, name))
                    .map(|probe| matches!(probe, crate::sync::disk::Probe::Managed { .. }));
                // A directory the worker is making right now is an item
                // already, whatever its name (the outbox on the bus).
                if matches!(managed, Ok(false)) && !self.dir_being_made(at) {
                    return true;
                }
            }
            at = parent;
        }
        false
    }

    /// Whether the worker is making the directory at `rel` in OneDrive right
    /// now: a running `mkdir` of its object.
    fn dir_being_made(&self, rel: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let running = |row: &&OutboxRow| row.kind == OutboxKind::Mkdir && row.item_id.is_none() && row.state == OutboxState::Running;
        if !self.rows.iter().any(|row| running(&row)) {
            return false;
        }
        let Ok(meta) = self.ex.disk.dir(rel).and_then(|dir| dir.metadata()) else { return false };
        self.rows.iter().filter(running).any(|row| row.inode.as_ref().is_some_and(|i| i.dev == meta.dev() && i.ino == meta.ino()))
    }

    /// Rules 3–5: an entry without an item id.
    fn unnamed(&mut self, i: usize) -> Result<(), ExamineError> {
        let e = self.entries[i].clone();
        let pending = self.pending_row(&e).cloned();
        let target_parent = self.dir_id(e.dir_rel());
        // 3. Ignored — its name, or a directory of the user's own above it
        // (the outbox on the bus): stays local, and a create it had goes — unless the
        // worker is creating it right now: then it is an item already, which
        // syncs whatever its name, and the rename waits behind.
        if self.ex.ignore.matches(&e.name) || self.in_ignored_dir(e.dir_rel()) {
            let being_created = self.being_created(&e);
            if being_created {
                self.detections.push(Detection {
                    kind: OutboxKind::Move,
                    item_id: None,
                    inode: Some(e.inode()),
                    rel: e.rel.clone(),
                    base: None,
                    target_parent,
                    target_name: Some(lossy(&e.name)),
                    same_content: false,
                    state: OutboxState::Ready,
                    reason: None,
                    next_try: None,
                });
            } else if let Some(row) = pending.filter(|r| matches!(r.kind, OutboxKind::Create | OutboxKind::Mkdir)) {
                self.ops.push(OutboxOp::Remove(row.seq));
            }
            // A directory newly ignored takes the new things waiting inside it
            // along: their folder is never made in OneDrive (the outbox on the bus).
            // One being made keeps them: it is an item already.
            if e.ty == Type::Dir && !being_created {
                let inside: Vec<i64> = self
                    .rows
                    .iter()
                    .filter(|r| r.item_id.is_none() && r.state != OutboxState::Running && is_under(&r.rel, &e.rel))
                    .map(|r| r.seq)
                    .collect();
                self.ops.extend(inside.into_iter().map(OutboxOp::Remove));
            }
            return Ok(());
        }
        let is_dir = e.ty == Type::Dir;
        // A file over a name whose delete is still pending: save-by-rename
        // across batches (§3.5).
        if !is_dir && pending.is_none() {
            let delete = self
                .rows
                .iter()
                .find(|r| r.kind == OutboxKind::Delete && r.state != OutboxState::Running && r.rel == e.rel && r.item_id.is_some())
                .cloned();
            if let Some(row) = delete {
                let id = row.item_id.clone().expect("filtered above");
                if let Some(base) = self.base_row(&id)?.filter(|b| b.kind == Kind::File) {
                    self.consumed.insert(i);
                    return self.save_by_rename(&id, &base, i);
                }
            }
        }
        let mut d = Detection {
            kind: if is_dir { OutboxKind::Mkdir } else { OutboxKind::Create },
            item_id: None,
            inode: Some(e.inode()),
            rel: e.rel.clone(),
            base: None,
            target_parent,
            target_name: Some(lossy(&e.name)),
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
        };
        if let Some(row) = &pending {
            if is_dir && row.rel != e.rel {
                self.ops.push(OutboxOp::Rebase { from: row.rel.clone(), to: e.rel.clone() });
            }
            let now = snapshot(e.size, e.mtime.0, e.mtime.1);
            if !is_dir && row.state == OutboxState::Running && row.snapshot.as_deref() == Some(now.as_str()) {
                // Being uploaded as it is now: only where it is matters.
                d.kind = OutboxKind::Move;
            }
        }
        let refused = names::refused(&e.name).or((!is_dir && e.size > names::MAX_FILE_SIZE).then_some(names::Refused::TooLarge));
        if let Some(refused) = refused {
            d.state = OutboxState::Blocked;
            d.reason = Some(refused.as_str().into());
        } else if !is_dir {
            (d.state, d.reason, d.next_try) = self.probe_writer(&e)?;
        }
        self.detections.push(d);
        Ok(())
    }

    /// `local_skipped` lists what is there now: rows for examined places
    /// that no longer qualify go — after a Full scan, every such row.
    fn tidy_skipped(&mut self, full: bool) -> Result<(), ExamineError> {
        for s in self.store(|s| s.local_skipped())? {
            let dir = s.rel.parent().unwrap_or(Path::new(""));
            let examined = !self.unreadable.contains(&s.rel)
                && (full
                    || self.whole.contains(dir)
                    || self.named.get(dir).is_some_and(|names| s.rel.file_name().is_some_and(|n| names.contains(n))));
            if examined && !self.skipped.contains_key(&s.rel) {
                self.ops.push(OutboxOp::Unskip(s.rel));
            }
        }
        let skipped = std::mem::take(&mut self.skipped);
        self.ops.extend(skipped.into_iter().map(|(rel, reason)| OutboxOp::Skip { rel, reason }));
        Ok(())
    }

    /// Adds item `id` and, for a folder, everything the base has inside it.
    fn count_removed(&mut self, id: &str, removed: &mut HashSet<String>) -> Result<(), TreeError> {
        if !removed.insert(id.to_owned()) {
            return Ok(());
        }
        if self.base_row(id)?.is_some_and(|base| base.kind == Kind::Folder) {
            removed.extend(self.store(|s| s.descendants(Table::Items, id))?);
        }
        Ok(())
    }

    /// The mass-delete guard, then everything in one transaction.
    ///
    /// The guard counts the items removed from OneDrive — by deletes and
    /// moves out alike, the Trash included (§4.6) — by this batch and by the
    /// removals still waiting in the outbox, so that a trickle adds up. Each
    /// item counts once, and removals the user confirmed count no more.
    /// When it trips on something new, every removal not confirmed is
    /// held.
    ///
    /// Rows are written freers first: a row that frees a name in OneDrive
    /// before the row that takes it, otherwise shallowest first, in the order
    /// found.
    fn finish(mut self) -> Result<Examined, ExamineError> {
        let total = self.store(|s| s.counts(Table::Items))?.placed;
        let confirmed: HashSet<String> = self.rows.iter().filter(|r| r.kind.removes() && r.confirmed).filter_map(|r| r.item_id.clone()).collect();
        let waiting: Vec<OutboxRow> = self
            .rows
            .iter()
            .filter(|r| r.kind.removes() && !r.confirmed && r.state != OutboxState::Running)
            .filter(|r| !r.item_id.as_ref().is_some_and(|i| self.decided.contains(i)))
            .cloned()
            .collect();
        let new: Vec<String> = self
            .detections
            .iter()
            .filter(|d| d.kind.removes())
            .filter_map(|d| d.item_id.clone())
            .filter(|id| !confirmed.contains(id))
            .collect();
        let fresh = !new.is_empty() || waiting.iter().any(|r| r.state != OutboxState::Held);
        let mut removed: HashSet<String> = HashSet::new();
        for id in new.iter().chain(waiting.iter().filter_map(|r| r.item_id.as_ref())) {
            self.count_removed(id, &mut removed)?;
        }
        let n = removed.len() as u64;
        if fresh && (n > MASS_DELETE_ITEMS || (n >= MASS_DELETE_FLOOR && n * 100 > total * MASS_DELETE_PERCENT)) {
            tracing::warn!("{n} of {total} items would be removed from OneDrive; held until confirmed");
            for d in self.detections.iter_mut().filter(|d| d.kind.removes() && d.item_id.as_ref().is_some_and(|id| !confirmed.contains(id))) {
                d.state = OutboxState::Held;
                d.reason = Some(MASS_DELETE.into());
                d.next_try = None;
            }
            for row in waiting.iter().filter(|r| r.state != OutboxState::Held) {
                self.ops.push(OutboxOp::Hold { seq: row.seq, reason: MASS_DELETE.into() });
            }
            self.out.held = n;
        }
        let detections = ordered(std::mem::take(&mut self.detections));
        let mut ops: Vec<OutboxOp> = Vec::new();
        let (first, rest): (Vec<OutboxOp>, Vec<OutboxOp>) =
            std::mem::take(&mut self.ops).into_iter().partition(|op| matches!(op, OutboxOp::Rebase { .. } | OutboxOp::Remove(_)));
        ops.extend(first);
        ops.extend(detections.into_iter().map(OutboxOp::Record));
        ops.extend(rest);
        let now = self.ex.now;
        self.out.applied = self.ex.store.with(|s| s.outbox_apply(&ops, now))?;
        Ok(self.out)
    }
}

/// Whether a detection takes its item away from its base place.
fn moves_away(d: &Detection) -> bool {
    matches!(d.kind, OutboxKind::Move | OutboxKind::Update)
        && d.base.as_ref().is_some_and(|b| (b.parent.as_deref(), b.name.as_deref()) != (d.target_parent.as_deref(), d.target_name.as_deref()))
}

/// The OneDrive (parent, name) a detection frees and takes, compared
/// without case as OneDrive does.
fn frees(d: &Detection) -> Option<(String, String)> {
    if !(d.kind.removes() || moves_away(d)) {
        return None;
    }
    let base = d.base.as_ref()?;
    Some((base.parent.clone()?, base.name.as_ref()?.to_lowercase()))
}

fn takes(d: &Detection) -> Option<(String, String)> {
    if !(matches!(d.kind, OutboxKind::Mkdir | OutboxKind::Create) || moves_away(d)) {
        return None;
    }
    Some((d.target_parent.clone()?, d.target_name.as_ref()?.to_lowercase()))
}

/// A batch's detections in the order their rows are written: every row that
/// frees a name before a row that takes it, otherwise shallowest first and
/// in the order found. A cycle (a swap) is broken at its first row.
fn ordered(detections: Vec<Detection>) -> Vec<Detection> {
    let n = detections.len();
    let mut freeing: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (i, d) in detections.iter().enumerate() {
        if let Some(key) = frees(d) {
            freeing.entry(key).or_default().push(i);
        }
    }
    let mut after: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut before = vec![0usize; n];
    for (t, d) in detections.iter().enumerate() {
        for &f in takes(d).and_then(|key| freeing.get(&key)).map(Vec::as_slice).unwrap_or_default() {
            if f != t {
                after[f].push(t);
                before[t] += 1;
            }
        }
    }
    let priority = |i: usize| (depth(&detections[i].rel), i);
    let mut ready: BTreeSet<(usize, usize)> = (0..n).filter(|&i| before[i] == 0).map(priority).collect();
    let mut done = vec![false; n];
    let mut order = Vec::with_capacity(n);
    while order.len() < n {
        let next = match ready.pop_first() {
            Some((_, i)) if done[i] => continue,
            Some((_, i)) => i,
            None => (0..n).filter(|&i| !done[i]).min_by_key(|&i| priority(i)).expect("a row is left"),
        };
        done[next] = true;
        order.push(next);
        for &t in &after[next] {
            before[t] = before[t].saturating_sub(1);
            if before[t] == 0 && !done[t] {
                ready.insert(priority(t));
            }
        }
    }
    let mut slots: Vec<Option<Detection>> = detections.into_iter().map(Some).collect();
    order.into_iter().map(|i| slots[i].take().expect("each row once")).collect()
}

fn size_and_time(file: &File) -> io::Result<(i64, i64, i64)> {
    let stat = nix::sys::stat::fstat(file).map_err(io::Error::from)?;
    Ok((stat.st_size as i64, stat.st_mtime as i64, stat.st_mtime_nsec as i64))
}

/// The file's quickXorHash, base64, read once from the start.
fn hash(file: &File) -> io::Result<String> {
    let mut hasher = crate::quickxor::QuickXor::new();
    let mut reader = file;
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buffer[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(hasher.finish_base64())
}
