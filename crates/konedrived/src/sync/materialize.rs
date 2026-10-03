//! Makes the folder match the tree.
//!
//! Phase 1 moves every item of ours that is not where the new tree wants it
//! into the holding directory under its item id, deepest first, so that a move
//! never changes the path of something still to be moved. Phase 2 walks the
//! new tree top down: each item is found in place, taken back from the holding
//! directory, or created. Phase 3 deletes what is left in the holding
//! directory, rescuing any file that holds local work. A crash
//! anywhere leaves only things the next Full reconcile recognises by item id
//! — but for a new folder's temporary directory killed before its label,
//! which the next attempt to make that folder clears (`labelled_dir`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use konedrive_fs::placeholder::{self, read_state, stamp_matches, PlaceholderSpec, State, LOCKED_FILE_MODE, OPEN_FILE_MODE};
use tokio_util::sync::CancellationToken;

use super::disk::{Disk, Probe, Scanned, HOLDING, NEW_PREFIX};
use super::activity::Kind as EventKind;
use super::helper::HelperLink;
use super::source::ContentSource;
use super::InodeLocks;
use konedrive_tree::{Kind, Placement, Row, Store, Table, TreeError};

/// Read-write mode's rules (`docs/design/writes.md` §9).
mod rw;
pub use rw::Rw;

/// Whether another account of this daemon claims an item id (`docs/design/writes.md` §8.3): an object
/// carrying it is never removed by this folder's reconcile.
pub type Claimed = std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>;

pub enum Scope {
    /// Scan the folder and match it to the whole tree.
    Full,
    /// Only these items (a delta's), and what comes into view with them.
    Changed(Vec<String>),
}

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

#[derive(Debug, Default)]
pub struct Applied {
    pub created: u64,
    pub moved: u64,
    pub deleted: u64,
    pub updated: u64,
    /// Files being filled or freed right now; the next cycle looks again.
    pub deferred: u64,
    /// Local versions moved out of the way, each a conflict
    ///.
    pub rescued: Vec<Rescued>,
    pub replacements: Vec<Replacement>,
    /// What a Changed scope did, item by item, for the activity log (spec
    /// §16.1). A Full scope leaves it empty: it is one `listed` event, not
    /// one per item.
    pub changes: Vec<Changed>,
    /// Files made, and files or folders moved, inside a folder a pin keeps
    /// on this device, relative to the root: what the sync queues for
    /// download once the reconcile is done.
    pub pinned: Vec<PathBuf>,
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
    /// Read-write mode: local versions kept beside the cloud's (§6).
    pub copies: Vec<Copied>,
    /// Read-write mode: places for the examination to look at, relative to
    /// the root (`true`: with everything below) — files and folders this
    /// reconcile kept, copied or took its attributes off, which no event the
    /// watcher keeps says (the daemon's own changes are dropped by pid).
    pub examine: Vec<(PathBuf, bool)>,
    /// Read-write mode: folders gone from OneDrive whose directory stays
    /// here, holding local work, to be made again there: a
    /// `resyncChangesUploadDifferences` listing only (F116).
    pub recreated: Vec<String>,
    /// Read-write mode: items whose change the base takes in this cycle
    /// whatever a local change holds — removed in OneDrive and taken off the
    /// disk, or no longer placed here (issue #104). Never deferred.
    pub taken: HashSet<String>,
}

/// A local version kept beside the cloud's under a new name (write design
/// §6): what read-write mode does where the read phase rescued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copied {
    /// Its name before, relative to the root: now the cloud's version.
    pub original: PathBuf,
    /// Where it is now, relative to the root.
    pub copy: PathBuf,
}

/// A local version a reconcile moved out of the way (§16.3: "the
/// daemon records what the materializer rescued").
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

pub struct Materializer {
    pub disk: Disk,
    pub store: Store,
    /// For a folder with interception: every directory made is marked through
    /// it before anything is put in it (invariant M1).
    pub link: Option<HelperLink>,
    /// To wait for the helper from this blocking thread.
    pub runtime: tokio::runtime::Handle,
    pub locks: InodeLocks,
    pub root_item_id: String,
    /// `rescued/<timestamp>` for this cycle.
    pub rescue_into: PathBuf,
    pub cancel: CancellationToken,
    /// Read-write mode's rules (`docs/design/writes.md` §9), for a read-write
    /// folder; `None` keeps the read phase's.
    pub rw: Option<Rw>,
    /// Asked before an object with an id this folder does not know is
    /// removed: another account's is set aside instead, alive, for that
    /// account's move out to download where it is. `None`
    /// where no other account can be (tests).
    pub claimed: Option<Claimed>,
}

#[derive(Default)]
struct Run {
    out: Applied,
    /// Where each item moved to the holding directory came from — the path a
    /// rescued file is shown under.
    moved_from: HashMap<String, PathBuf>,
    /// Directories made this run, locked once everything is in them.
    made: Vec<PathBuf>,
    /// The Changed scope: an item of ours in the way that is not in it means
    /// the folder does not match the stored tree.
    scope: Option<HashSet<String>>,
    /// The holding directory has been marked by this run (invariant M1).
    holding_marked: bool,
    /// Whether a pin keeps the directory at each path on this device, as far
    /// as this run has asked: a directory's answer is read once.
    pinned_dirs: HashMap<PathBuf, bool>,
    /// Read-write mode: items of ours not where the base has them — a local
    /// move or copy not examined yet — left where they are, with what is
    /// below them.
    left: HashSet<String>,
    /// Read-write mode, Changed scope: items not found where the base has
    /// them (deleted or moved here, not examined yet).
    missing: HashSet<String>,
    /// The inodes items were placed as, not recorded yet: written
    /// [`PLACED_BATCH`] at a time, and at the end of the run (issue #39).
    placed: Vec<(String, konedrive_fs::handle::FileHandle)>,
    /// Files whose download the drain stopped (issue #104).
    stopped: Vec<super::InodeKey>,
    /// Read-write mode: items this run found no longer placed (issue #104):
    /// examined first, and removed by a later cycle at the earliest.
    unplaced: HashSet<String>,
}

fn survey_stopped(survey: &rw::Survey) -> Vec<super::InodeKey> {
    survey.stopped_keys().to_vec()
}

/// Placed items recorded in one transaction (issue #39; a guess).
pub const PLACED_BATCH: usize = 500;

impl Run {
    /// Notes what a Changed scope did to `rel`; a Full scope notes nothing
    /// (see [`Applied::changes`]).
    fn note(&mut self, kind: EventKind, rel: &Path, from: Option<PathBuf>) {
        if self.scope.is_some() {
            self.out.changes.push(Changed { kind, rel: rel.to_path_buf(), from });
        }
    }
}

impl Materializer {
    pub fn apply(&self, scope: Scope) -> Result<Applied, ApplyError> {
        self.apply_keeping(scope, &mut Vec::new())
    }

    /// [`apply`](Self::apply), which also hands over, when it fails, what it
    /// had rescued by then (into `rescued`): those files are out of the way
    /// whatever comes next, and a Full reconcile run after a Changed one
    /// finds nothing left to rescue there.
    pub fn apply_keeping(&self, scope: Scope, rescued: &mut Vec<Rescued>) -> Result<Applied, ApplyError> {
        let mut run = Run::default();
        let result = self.apply_run(scope, &mut run);
        if result.is_err() {
            rescued.append(&mut run.out.rescued);
        }
        result.map(|()| run.out)
    }

    /// [`apply_keeping`](Self::apply_keeping) for read-write mode: what is
    /// done on disk whatever comes next — rescues, conflict copies, places
    /// to examine, folders made local — is handed over too. What a failed
    /// pass left unsettled is not: the pass after it decides that again.
    ///
    /// `moved_from` carries across the hand-over where each item the failed
    /// pass moved to the holding directory came from, so that the next pass
    /// puts back there what it does not place.
    pub fn apply_handing_over(&self, scope: Scope, done: &mut Applied, moved_from: &mut HashMap<String, PathBuf>) -> Result<Applied, ApplyError> {
        let mut run = Run { moved_from: std::mem::take(moved_from), ..Run::default() };
        let result = self.apply_run(scope, &mut run);
        if result.is_err() {
            done.rescued.append(&mut run.out.rescued);
            done.copies.append(&mut run.out.copies);
            done.examine.append(&mut run.out.examine);
            done.recreated.append(&mut run.out.recreated);
            done.taken.extend(std::mem::take(&mut run.out.taken));
            *moved_from = std::mem::take(&mut run.moved_from);
        }
        result.map(|()| run.out)
    }

    /// Whether deleting or replacing `file` would lose something only this
    /// machine has ([`holds_local_work`]); in read-write mode an emptied
    /// download counts too.
    pub(super) fn local_work(&self, file: &File) -> bool {
        if self.rw.is_some() {
            holds_local_work_rw(file)
        } else {
            holds_local_work(file)
        }
    }

    fn apply_run(&self, scope: Scope, run: &mut Run) -> Result<(), ApplyError> {
        let result = self.apply_run_placing(scope, run);
        // What was placed is recorded, whatever became of the run.
        let recorded = self.record_placed_now(run);
        result.and(recorded)
    }

    /// Notes the inode item `id` was just placed as, `name` in `dir`, by
    /// name, opening nothing; recorded with the rest of its batch. A
    /// filesystem that gives no handles leaves it unrecorded (see
    /// `local::record_placed`).
    fn record_placed(&self, run: &mut Run, dir: &File, name: &OsStr, id: &str) -> Result<(), ApplyError> {
        match konedrive_fs::handle::FileHandle::at(dir, name) {
            Ok(handle) => run.placed.push((id.to_owned(), handle)),
            Err(e) => tracing::debug!("no file handle for {}: {e}", name.to_string_lossy()),
        }
        if run.placed.len() >= PLACED_BATCH {
            self.record_placed_now(run)?;
        }
        Ok(())
    }

    /// Records the placements noted so far, in one transaction.
    fn record_placed_now(&self, run: &mut Run) -> Result<(), ApplyError> {
        if run.placed.is_empty() {
            return Ok(());
        }
        let placed = std::mem::take(&mut run.placed);
        self.store.call_blocking(move |s| s.set_local_handles(&placed))?;
        Ok(())
    }

    fn apply_run_placing(&self, scope: Scope, run: &mut Run) -> Result<(), ApplyError> {
        match (scope, &self.rw) {
            (Scope::Full, None) => self.full(run)?,
            (Scope::Changed(ids), None) => self.changed(ids, run)?,
            (Scope::Full, Some(rw)) => self.full_rw(rw, run)?,
            (Scope::Changed(ids), Some(rw)) => self.changed_rw(rw, ids, run)?,
        }
        match &self.rw {
            None => self.drain_holding(run)?,
            Some(rw) => {
                self.leaving_rw(rw, run)?;
                self.drain_holding_rw(rw, run)?;
            }
        }
        for rel in run.made.iter().rev() {
            if let Ok(dir) = self.disk.dir(rel) {
                self.disk.lock_dir(&dir)?;
            }
        }
        Ok(())
    }

    fn check_cancel(&self) -> Result<(), ApplyError> {
        if self.cancel.is_cancelled() {
            return Err(ApplyError::Cancelled);
        }
        Ok(())
    }

    fn full(&self, run: &mut Run) -> Result<(), ApplyError> {
        self.check_cancel()?;
        let scanned = self.disk.scan(&self.root_item_id)?;
        let mut id_counts: HashMap<&str, usize> = HashMap::new();
        for entry in &scanned {
            if let Some(id) = &entry.id {
                *id_counts.entry(id.as_str()).or_insert(0) += 1;
            }
        }
        let mut misplaced: Vec<&Scanned> = Vec::new();
        for entry in &scanned {
            let Some(id) = &entry.id else { continue };
            if is_leftover_replacement(entry, id, &id_counts) {
                self.check_cancel()?;
                self.discard_leftover_replacement(entry, run)?;
                continue;
            }
            if self.is_misplaced(entry, id)? {
                misplaced.push(entry);
            }
        }
        misplaced.sort_by(|a, b| b.depth.cmp(&a.depth));
        for entry in misplaced {
            self.check_cancel()?;
            self.to_holding(&entry.rel, entry.id.as_deref().expect("filtered above"), run)?;
        }
        let root = self.disk.dir(Path::new(""))?;
        self.disk.lock_dir(&root)?;
        let mut queue = VecDeque::from([(self.root_item_id.clone(), PathBuf::new())]);
        while let Some((id, rel)) = queue.pop_front() {
            self.check_cancel()?;
            let children = self.store.call_blocking(move |s| s.children(Table::Staging, &id))?;
            for row in children {
                if row.placement != Placement::Placed {
                    continue;
                }
                let Some(placed) = self.place(&row, &rel, run, true)? else { continue };
                if row.kind == Kind::Folder {
                    queue.push_back((row.id.clone(), placed));
                }
            }
        }
        Ok(())
    }

    fn is_misplaced(&self, entry: &Scanned, id: &str) -> Result<bool, ApplyError> {
        let Some(row) = self.store.call_blocking({ let id = id.to_owned(); move |s| s.get(Table::Staging, &id) })? else {
            return Ok(true);
        };
        Ok(row.placement != Placement::Placed
            || row.parent_id != entry.parent_id
            || entry.rel.file_name() != Some(OsStr::new(&row.name))
            || (row.kind == Kind::Folder) != entry.is_dir)
    }

    fn changed(&self, ids: Vec<String>, run: &mut Run) -> Result<(), ApplyError> {
        // What an earlier run left in the holding directory is not this
        // delta's to drain; a Full reconcile sorts it out by item id.
        if self.holding_if_any()?.is_some() {
            return Err(ApplyError::NeedFull(format!("{HOLDING} is left from an earlier run")));
        }
        let mut scope: HashSet<String> = ids.iter().cloned().collect();
        // The root's own entry changes with every change below it; it is the
        // folder itself, never something to move.
        scope.remove(&self.root_item_id);
        for id in &ids {
            let new = self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(Table::Staging, &id) })?;
            let old = self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(Table::Items, &id) })?;
            let comes_into_view = new.as_ref().is_some_and(|l| l.placed) && !old.as_ref().is_some_and(|l| l.placed);
            if comes_into_view {
                scope.extend(self.store.call_blocking({ let id = id.to_owned(); move |s| s.descendants(Table::Staging, &id) })?);
            }
        }
        run.scope = Some(scope.clone());

        // Phase 1, by where things are now, deepest first.
        let mut here = Vec::new();
        for id in &scope {
            if let Some(old) = self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(Table::Items, &id) })?.filter(|l| l.placed) {
                here.push((id.clone(), old));
            }
        }
        here.sort_by(|a, b| b.1.depth.cmp(&a.1.depth));
        for (id, old) in &here {
            self.check_cancel()?;
            let parent = old.rel.parent().unwrap_or(Path::new(""));
            let name = old.rel.file_name().ok_or_else(|| ApplyError::NeedFull(format!("{id} has no name")))?;
            let dir = self.disk.dir(parent).map_err(|e| ApplyError::NeedFull(format!("{}: {e}", parent.display())))?;
            match self.disk.probe(&dir, name)? {
                Probe::Managed { id: found, .. } if &found == id => {}
                other => return Err(ApplyError::NeedFull(format!("{} should be {id} and is {other:?}", old.rel.display()))),
            }
            let old_row = self.store.call_blocking({ let id = id.to_owned(); move |s| s.get(Table::Items, &id) })?;
            let new_row = self.store.call_blocking({ let id = id.to_owned(); move |s| s.get(Table::Staging, &id) })?;
            let stays = matches!((&old_row, &new_row), (Some(o), Some(n))
                if n.placement == Placement::Placed && n.parent_id == o.parent_id && n.name == o.name);
            if !stays {
                self.to_holding(&old.rel, id, run)?;
            }
        }

        // Phase 2, by where things belong, shallowest first.
        let mut there = Vec::new();
        for id in &scope {
            let row = self.store.call_blocking({ let id = id.to_owned(); move |s| s.get(Table::Staging, &id) })?;
            let new = self.store.call_blocking({ let id = id.to_owned(); move |s| s.locate(Table::Staging, &id) })?;
            if let (Some(row), Some(new)) = (row, new) {
                if new.placed {
                    there.push((row, new));
                }
            }
        }
        there.sort_by(|a, b| a.1.depth.cmp(&b.1.depth));
        // Folders placed — and so checked — by this phase.
        let mut placed: HashSet<String> = HashSet::new();
        for (row, new) in &there {
            self.check_cancel()?;
            let parent = new.rel.parent().unwrap_or(Path::new(""));
            self.check_parent(row, parent, &placed)?;
            if self.place(row, parent, run, false)?.is_some() && row.kind == Kind::Folder {
                placed.insert(row.id.clone());
            }
        }
        Ok(())
    }

    /// The Changed scope opens an item's folder by its path. Unless that is
    /// the root, or a folder this run has just placed, the directory there is
    /// checked first to be the folder the tree says — anything else there
    /// (a stranger of the same name, put there past the lock) is the folder
    /// not matching the stored tree.
    fn check_parent(&self, row: &Row, parent: &Path, placed: &HashSet<String>) -> Result<(), ApplyError> {
        let Some(parent_id) = row.parent_id.as_deref() else {
            return Ok(());
        };
        if parent.as_os_str().is_empty() || placed.contains(parent_id) {
            return Ok(());
        }
        let above = parent.parent().unwrap_or(Path::new(""));
        let name = parent.file_name().ok_or_else(|| ApplyError::NeedFull(format!("{} has no name", parent.display())))?;
        let dir = self.disk.dir(above).map_err(|e| ApplyError::NeedFull(format!("{}: {e}", above.display())))?;
        match self.disk.probe(&dir, name)? {
            Probe::Managed { id, is_dir: true } if id == parent_id => Ok(()),
            other => Err(ApplyError::NeedFull(format!("{} should be the folder {parent_id} and is {other:?}", parent.display()))),
        }
    }

    /// Makes `row` exist as `parent_rel/<name>` and returns that path;
    /// `None` when read-write mode leaves it as it is (see [`Rw`]).
    fn place(&self, row: &Row, parent_rel: &Path, run: &mut Run, full: bool) -> Result<Option<PathBuf>, ApplyError> {
        let rel = parent_rel.join(&row.name);
        let dir = self.disk.dir(parent_rel)?;
        let name = OsStr::new(&row.name);
        let is_folder = row.kind == Kind::Folder;
        match self.disk.probe(&dir, name)? {
            Probe::Managed { id, is_dir } if id == row.id && is_dir == is_folder => {
                if let Some(rw) = &self.rw {
                    // Found where it belongs: its object, if none is recorded
                    // (a rebuilt base, a forgotten one), is this one.
                    if rw.unplaced.contains(&row.id) {
                        self.record_placed(run, &dir, name, &row.id)?;
                    }
                }
                if !is_folder {
                    self.check_file(&dir, name, row, &rel, run)?;
                }
                if full {
                    // Not a file a fill holds (A-M4): it is left for the next
                    // Full reconcile.
                    self.disk.enforce_mode(&dir, name, |file| Ok(self.locks.try_lock(super::InodeKey::of(file)?)))?;
                }
                return Ok(Some(rel));
            }
            Probe::Managed { id, .. } if id == row.id => {
                // Its own id with the wrong kind: nothing a Graph id does.
                // Not trusted, not thrown away.
                match &self.rw {
                    None => self.rescue(&dir, name, &rel, run)?,
                    Some(rw) => self.copy_aside(rw, &dir, name, &rel, run)?,
                }
            }
            Probe::Managed { id, .. } => {
                if let Some(rw) = &self.rw {
                    if self.holds_the_name(rw, &id, &rel, run)? {
                        run.out.unsettled.insert(row.id.clone());
                        return Ok(None);
                    }
                }
                if run.scope.as_ref().is_some_and(|scope| !scope.contains(&id)) {
                    return Err(ApplyError::NeedFull(format!("{id} is in the way at {}", rel.display())));
                }
                self.to_holding(&rel, &id, run)?;
            }
            Probe::Unmanaged { is_dir } => match &self.rw {
                None => self.rescue(&dir, name, &rel, run)?,
                // A create or mkdir waiting here: the outbox worker settles
                // it with the cloud's item (§6, create/create).
                Some(rw) if rw.pending_at(&rel) || !rw.brings(&row.id) || (is_folder && is_dir) => {
                    // A local folder where OneDrive has a new one: the two
                    // merge, by the `mkdir`'s `409` (§6), never a copy.
                    run.out.unsettled.insert(row.id.clone());
                    return Ok(None);
                }
                Some(rw) => self.copy_aside(rw, &dir, name, &rel, run)?,
            },
            Probe::Absent => {}
        }
        if let Some(holding) = self.holding_if_any()? {
            if let Probe::Managed { id, is_dir } = self.disk.probe(&holding, OsStr::new(&row.id))? {
                if id == row.id && is_dir == is_folder {
                    if is_folder {
                        // A folder made by a cycle whose marking failed waits
                        // here unmarked; it is marked before its real name
                        // shows it (invariant M1). Marking twice is harmless.
                        let waiting = self.disk.open_subdir(&holding, OsStr::new(&row.id))?;
                        self.mark(&waiting, &rel)?;
                    }
                    self.disk.rename(&holding, OsStr::new(&row.id), &dir, name)?;
                    {
                        let (from, to) = (PathBuf::from(HOLDING).join(&row.id), rel.clone());
                        self.store.call_blocking(move |s| s.leaving_rebase(&from, &to))?;
                    }
                    self.record_placed(run, &dir, name, &row.id)?;
                    run.out.moved += 1;
                    let from = run.moved_from.get(&row.id).cloned();
                    run.note(EventKind::Moved, &rel, from);
                    self.note_if_pinned(&rel, run);
                    if !is_folder {
                        self.check_file(&dir, name, row, &rel, run)?;
                    }
                    return Ok(Some(rel));
                }
            }
        }
        if let Some(rw) = &self.rw {
            if !self.place_again(rw, row, &rel, run)? {
                run.out.unsettled.insert(row.id.clone());
                return Ok(None);
            }
        }
        self.create(&dir, row, &rel, run)?;
        run.note(EventKind::Added, &rel, None);
        Ok(Some(rel))
    }

    fn create(&self, dir: &File, row: &Row, rel: &Path, run: &mut Run) -> Result<(), ApplyError> {
        match row.kind {
            Kind::Folder => {
                let temp = format!("{NEW_PREFIX}{}", row.id);
                let made = self.labelled_dir(dir, &temp, row, rel, run)?;
                self.mark(&made, rel)?;
                self.disk.rename(dir, OsStr::new(&temp), dir, OsStr::new(&row.name))?;
                self.record_placed(run, dir, OsStr::new(&row.name), &row.id)?;
                run.made.push(rel.to_path_buf());
            }
            Kind::File => {
                let spec = PlaceholderSpec {
                    item_id: &row.id,
                    size: row.size,
                    mtime: cloud_time(row),
                    ctag: row.ctag.as_deref(),
                    mode: if self.disk.locked() { LOCKED_FILE_MODE } else { OPEN_FILE_MODE },
                };
                self.disk.writable(dir, || placeholder::create_placeholder_with(dir, &row.name, &spec))?;
                self.record_placed(run, dir, OsStr::new(&row.name), &row.id)?;
                self.note_if_pinned(rel, run);
            }
        }
        run.out.created += 1;
        Ok(())
    }

    /// Notes `rel` in [`Applied::pinned`] when the folder it is in is
    /// pinned — by its own pin or one above it. A pin that cannot be read is
    /// logged and not followed: the next sweep finds the file.
    fn note_if_pinned(&self, rel: &Path, run: &mut Run) {
        let parent = rel.parent().unwrap_or(Path::new(""));
        match self.dir_pinned(parent, run) {
            Ok(true) => run.out.pinned.push(rel.to_path_buf()),
            Ok(false) => {}
            Err(e) => tracing::warn!("cannot tell whether {} is kept on this device: {e}", parent.display()),
        }
    }

    /// Whether a pin keeps the directory at `rel` on this device: its own,
    /// or one on a directory above it up to the root.
    fn dir_pinned(&self, rel: &Path, run: &mut Run) -> std::io::Result<bool> {
        if let Some(&pinned) = run.pinned_dirs.get(rel) {
            return Ok(pinned);
        }
        let pinned = placeholder::read_pin(&self.disk.dir(rel)?)?
            || match rel.parent() {
                Some(above) => self.dir_pinned(above, run)?,
                None => false,
            };
        run.pinned_dirs.insert(rel.to_path_buf(), pinned);
        Ok(pinned)
    }

    /// A new folder under its temporary name `temp`, labelled with its id.
    ///
    /// The name can be taken already: a kill
    /// between `mkdirat` and the label, or a label the disk had no room for,
    /// leaves a directory there with no id. A scan passes over what has no
    /// id, so nothing cleared it, and every later reconcile failed `EEXIST`
    /// here — for good. So a taken name is looked at: this folder's own,
    /// already labelled, is used as it is; an empty directory with no id is
    /// removed; anything else is rescued — nobody but this daemon
    /// makes that name, so whatever is in it came from outside.
    fn labelled_dir(&self, dir: &File, temp: &str, row: &Row, rel: &Path, run: &mut Run) -> Result<File, ApplyError> {
        let make = || self.disk.writable(dir, || placeholder::create_dir_item(dir, temp, &row.id));
        match make() {
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {}
            other => return Ok(other?),
        }
        let name = OsStr::new(temp);
        match self.disk.probe(dir, name)? {
            Probe::Managed { id, is_dir: true } if id == row.id => return Ok(self.disk.open_subdir(dir, name)?),
            Probe::Unmanaged { is_dir: true } if self.disk.list(&self.disk.open_subdir(dir, name)?)?.is_empty() => {
                self.disk.remove(dir, name, true)?;
            }
            Probe::Absent => {}
            _ => self.rescue(dir, name, &rel.with_file_name(temp), run)?,
        }
        Ok(make()?)
    }

    fn mark(&self, dir: &File, rel: &Path) -> Result<(), ApplyError> {
        if let Some(link) = &self.link {
            self.runtime
                .block_on(link.mark_dir(dir))
                .map_err(|e| ApplyError::Mark(rel.to_path_buf(), e.to_string()))?;
        }
        Ok(())
    }

    fn holding_if_any(&self) -> Result<Option<File>, ApplyError> {
        let root = self.disk.dir(Path::new(""))?;
        match self.disk.probe(&root, OsStr::new(HOLDING))? {
            Probe::Absent => Ok(None),
            Probe::Unmanaged { is_dir: true } => Ok(Some(self.disk.dir(Path::new(HOLDING))?)),
            other => Err(ApplyError::Io(format!("{HOLDING} is {other:?}, not the daemon's holding directory"))),
        }
    }

    fn holding(&self, run: &mut Run) -> Result<File, ApplyError> {
        let holding = match self.holding_if_any()? {
            Some(holding) => holding,
            None => {
                let root = self.disk.dir(Path::new(""))?;
                self.disk.make_dir(&root, OsStr::new(HOLDING))?
            }
        };
        // Files wait here; an open of one must still be intercepted — also in
        // a holding directory left by a cycle whose marking failed.
        if !run.holding_marked {
            self.mark(&holding, Path::new(HOLDING))?;
            run.holding_marked = true;
        }
        Ok(holding)
    }

    fn to_holding(&self, rel: &Path, id: &str, run: &mut Run) -> Result<(), ApplyError> {
        let parent = rel.parent().unwrap_or(Path::new(""));
        let name = rel.file_name().ok_or_else(|| ApplyError::Io(format!("{} has no name", rel.display())))?;
        if parent == Path::new(HOLDING) && name == OsStr::new(id) {
            return Ok(());
        }
        let holding = self.holding(run)?;
        let dir = self.disk.dir(parent)?;
        self.disk.rename(&dir, name, &holding, OsStr::new(id))?;
        run.moved_from.entry(id.to_owned()).or_insert_with(|| rel.to_path_buf());
        // What is leaving inside it goes along (issue #104).
        let (from, to) = (rel.to_path_buf(), PathBuf::from(HOLDING).join(id));
        self.store.call_blocking(move |s| s.leaving_rebase(&from, &to))?;
        Ok(())
    }

    fn drain_holding(&self, run: &mut Run) -> Result<(), ApplyError> {
        let Some(holding) = self.holding_if_any()? else {
            return Ok(());
        };
        for name in self.disk.list(&holding)? {
            self.check_cancel()?;
            let shown = name
                .to_str()
                .and_then(|id| run.moved_from.get(id).cloned())
                .unwrap_or_else(|| PathBuf::from(HOLDING).join(&name));
            // What goes is forgotten first, and its downloads stop (issue
            // #104); what is rescued keeps its content, out of the folder.
            let survey = self.forget_before_removing(&holding, &name, true)?;
            let deleted = run.out.deleted;
            run.stopped = survey_stopped(&survey);
            let result = self.delete_tree(&holding, &name, &shown, run);
            if result.is_err() {
                self.settle_stopped(&holding, &name, &survey);
            }
            result?;
            // One event for what went, however much was inside it; what was
            // rescued instead is a conflict, not a removal.
            if run.out.deleted > deleted {
                run.note(EventKind::Removed, &shown, None);
            }
        }
        let root = self.disk.dir(Path::new(""))?;
        self.disk.remove(&root, OsStr::new(HOLDING), true)?;
        Ok(())
    }

    /// Deletes what the cloud no longer has, rescuing anything that would
    /// lose a local byte.
    fn delete_tree(&self, dir: &File, name: &OsStr, shown: &Path, run: &mut Run) -> Result<(), ApplyError> {
        match self.disk.probe(dir, name)? {
            Probe::Absent => Ok(()),
            Probe::Unmanaged { .. } => self.rescue(dir, name, shown, run),
            Probe::Managed { id, .. } if self.claimed_elsewhere(&id)? => {
                // It survives, out of the folder: a download stopped in it is
                // a placeholder again first (issue #104).
                if !run.stopped.is_empty() {
                    let survey = rw::Survey::stopped_only(run.stopped.clone());
                    self.settle_stopped(dir, name, &survey);
                }
                self.set_aside(dir, name, shown, run)
            }
            Probe::Managed { is_dir: true, .. } => {
                let sub = self.disk.open_subdir(dir, name)?;
                for child in self.disk.list(&sub)? {
                    self.delete_tree(&sub, &child, &shown.join(&child), run)?;
                }
                self.disk.remove(dir, name, true)?;
                run.out.deleted += 1;
                Ok(())
            }
            Probe::Managed { is_dir: false, .. } => {
                let file = self.disk.open_file(dir, name)?;
                if holds_local_work(&file) {
                    self.rescue(dir, name, shown, run)
                } else {
                    self.disk.remove(dir, name, false)?;
                    run.out.deleted += 1;
                    Ok(())
                }
            }
        }
    }

    fn rescue(&self, dir: &File, name: &OsStr, shown: &Path, run: &mut Run) -> Result<(), ApplyError> {
        let dest = self.disk.rescue(dir, name, shown, &self.rescue_into)?;
        tracing::warn!(
            "{} held local work the cloud's change would have lost; it is kept at {}",
            shown.display(),
            dest.display()
        );
        run.out.rescued.push(Rescued { original: shown.to_path_buf(), rescued: dest });
        Ok(())
    }

    /// Whether `id`, which is about to be removed, is another account's: one
    /// this folder's tree does not know, and another account claims.
    fn claimed_elsewhere(&self, id: &str) -> Result<bool, ApplyError> {
        let Some(claimed) = &self.claimed else { return Ok(false) };
        let known = self.store.call_blocking({ let id = id.to_owned(); move |s| Ok(s.get(Table::Items, &id)?.is_some() || s.get(Table::Staging, &id)?.is_some()) })?;
        Ok(!known && claimed(id))
    }

    /// Another account's object, moved here from its folder (write design
    /// §8.3): moved out of the folder like a rescue, but alive, attributes
    /// and all, so that the other account's move out finds it by its handle
    /// and downloads it where it is now. Never removed: that account's
    /// OneDrive may be the only other place its content is.
    fn set_aside(&self, dir: &File, name: &OsStr, shown: &Path, run: &mut Run) -> Result<(), ApplyError> {
        let dest = self.disk.set_aside(dir, name, shown, &self.rescue_into)?;
        tracing::warn!(
            "{} is another account's, moved here from its folder; it is kept at {}, where that account downloads it",
            shown.display(),
            dest.display()
        );
        run.out.rescued.push(Rescued { original: shown.to_path_buf(), rescued: dest });
        Ok(())
    }

    /// A name of the form `.konedrive-new-<id>` whose id
    /// also turns up somewhere else in the scan is not a folder waiting to be
    /// placed (that case is one entry per id, and is left to the usual
    /// misplaced/holding path) — it is the temporary link `swap_in` leaves
    /// when a crash, or a rename that fails after its `linkat` already
    /// succeeded, keeps a downloaded replacement from ever landing on the old
    /// name. Sending it to holding would collide with the real entry under
    /// the same id; it is discarded instead, like anything else the cloud
    /// does not know about, rescued first if it holds local work nobody made.
    fn discard_leftover_replacement(&self, entry: &Scanned, run: &mut Run) -> Result<(), ApplyError> {
        let parent = entry.rel.parent().unwrap_or(Path::new(""));
        let name = entry.rel.file_name().expect("is_leftover_replacement checked this");
        let dir = self.disk.dir(parent)?;
        let work = self.disk.open_file(&dir, name).map(|f| holds_local_work(&f)).unwrap_or(false);
        if work {
            self.rescue(&dir, name, &entry.rel, run)
        } else {
            self.disk.remove(&dir, name, false)?;
            Ok(())
        }
    }

    /// A file already in place, and what its content needs:
    /// a placeholder takes the new size, time and cTag in place; a downloaded
    /// file of another version is queued for replacement (§7.3), or rescued
    /// first when it holds local work (§9.3); a file being filled or freed up
    /// right now is left for the next cycle.
    ///
    /// A placeholder's content is told by its cTag and size alone, never by
    /// its time: every write of a fill moves the
    /// time to now, and a fill that stopped part-way — a restart, a failure —
    /// leaves the time wrong over a checkpoint worth keeping. Taken for a
    /// new version, the whole partial download was punched away at the first
    /// Full reconcile after a restart. Only the time is put back then.
    fn check_file(&self, dir: &File, name: &OsStr, row: &Row, rel: &Path, run: &mut Run) -> Result<(), ApplyError> {
        use std::os::unix::fs::MetadataExt;
        let file = self.disk.open_file(dir, name)?;
        let local_ctag = placeholder::read_ctag(&file).ok().flatten();
        match read_state(&file) {
            Ok(Some(State::OnlineOnly)) => {
                let meta = file.metadata()?;
                let same_content = local_ctag.as_deref() == row.ctag.as_deref() && meta.len() == row.size;
                if !same_content {
                    if self.update_placeholder(file, row, run)? {
                        run.note(EventKind::Updated, rel, None);
                    } else if self.rw.is_some() {
                        run.out.content_waits.insert(row.id.clone());
                    }
                } else if meta.mtime() != row.mtime {
                    self.put_time_back(file, row, run)?;
                }
                Ok(())
            }
            Ok(Some(State::Hydrated)) => {
                if local_ctag.is_some() && local_ctag.as_deref() == row.ctag.as_deref() {
                    return Ok(());
                }
                if let Some(rw) = &self.rw {
                    // An outbox row recorded since the cycle began: the
                    // worker's guard settles it (§3.7, excluded).
                    if !self.store.call_blocking({ let row_id = row.id.clone(); move |s| s.outbox_for_item(&row_id) })?.is_empty() {
                        run.out.unsettled.insert(row.id.clone());
                        return Ok(());
                    }
                    // Edit × edit (§6), or a version OneDrive may have lost
                    // (`resyncChangesUploadDifferences`): both are kept.
                    if self.local_work(&file) || rw.upload_differences {
                        drop(file);
                        self.copy_aside(rw, dir, name, rel, run)?;
                        self.create(dir, row, rel, run)?;
                        run.note(EventKind::Updated, rel, None);
                        return Ok(());
                    }
                } else if holds_local_work(&file) {
                    drop(file);
                    self.rescue(dir, name, rel, run)?;
                    self.create(dir, row, rel, run)?;
                    run.note(EventKind::Updated, rel, None);
                    return Ok(());
                }
                if let Some(ctag) = &row.ctag {
                    run.out.replacements.push(Replacement { id: row.id.clone(), rel: rel.to_path_buf(), ctag: ctag.clone(), size: row.size });
                    // The base keeps the version on disk until the new one
                    // is in place (the read-write reconcile must, item 4); its place is the one the
                    // disk took.
                    if self.rw.is_some() {
                        run.out.content_waits.insert(row.id.clone());
                    }
                }
                Ok(())
            }
            Ok(Some(State::Hydrating | State::Dehydrating)) => {
                run.out.deferred += 1;
                if self.rw.is_some() {
                    run.out.content_waits.insert(row.id.clone());
                }
                Ok(())
            }
            // Ours by its id, in no state anyone can vouch for.
            Ok(None) | Err(_) => {
                let work = self.local_work(&file);
                drop(file);
                if work {
                    match &self.rw {
                        None => self.rescue(dir, name, rel, run)?,
                        Some(rw) => self.copy_aside(rw, dir, name, rel, run)?,
                    }
                } else {
                    self.disk.remove(dir, name, false)?;
                }
                self.create(dir, row, rel, run)?;
                run.note(EventKind::Updated, rel, None);
                Ok(())
            }
        }
    }

    /// A placeholder takes its new size, time and cTag, in place (same inode).
    /// Under the per-inode lock, which a fill holds for its whole run: if one is
    /// running, this waits for the next cycle rather than for the download.
    /// Whether it was updated now.
    fn update_placeholder(&self, file: File, row: &Row, run: &mut Run) -> Result<bool, ApplyError> {
        let key = super::InodeKey::of(&file)?;
        let Some(_guard) = self.locks.try_lock(key) else {
            run.out.deferred += 1;
            return Ok(false);
        };
        let writable = placeholder::reopen_writable(&file)?;
        drop(file);
        // Looked at again under the lock: a fill may have finished meanwhile.
        if !matches!(read_state(&writable), Ok(Some(State::OnlineOnly))) {
            run.out.deferred += 1;
            return Ok(false);
        }
        // A checkpoint of another version goes, with its bytes; one of this
        // very version stays (A-I1). An online-only file carries no ignore
        // mark (the helper marks only what reads hydrated), so the punch
        // needs no ClearIgnore (question, answered).
        let stale = placeholder::read_progress(&writable)?.is_some_and(|p| Some(p.ctag.as_str()) != row.ctag.as_deref());
        if stale {
            placeholder::remove_progress(&writable)?;
            placeholder::punch_all(&writable)?;
        }
        writable.set_len(row.size)?;
        if let Some(ctag) = &row.ctag {
            placeholder::write_ctag(&writable, ctag)?;
        }
        placeholder::set_mtime(&writable, cloud_time(row))?;
        if row.size == 0 {
            // Nothing left to fetch: an empty file is a downloaded one.
            placeholder::write_stamp(&writable)?;
            placeholder::write_state(&writable, State::Hydrated)?;
        }
        writable.sync_all()?;
        run.out.updated += 1;
        Ok(true)
    }

    /// A placeholder of the tree's very version whose time is not the
    /// cloud's — a fill wrote into it and stopped — gets the cloud's time
    /// back, and keeps its checkpoint and the bytes it counts (A-I1). KIO
    /// checks a thumbnail against the file's time, so a wrong one also had
    /// Dolphin open the file to make its own thumbnail: a download. Under
    /// the per-inode lock, as [`Self::update_placeholder`]: a fill running
    /// now leaves it for the next cycle. The owner sets a time through a
    /// read-only descriptor, lock or not.
    fn put_time_back(&self, file: File, row: &Row, run: &mut Run) -> Result<(), ApplyError> {
        let key = super::InodeKey::of(&file)?;
        let Some(_guard) = self.locks.try_lock(key) else {
            run.out.deferred += 1;
            return Ok(());
        };
        if !matches!(read_state(&file), Ok(Some(State::OnlineOnly))) {
            run.out.deferred += 1;
            return Ok(());
        }
        placeholder::set_mtime(&file, cloud_time(row))?;
        Ok(())
    }
}

/// The time the cloud gives an item, as a placeholder carries it.
fn cloud_time(row: &Row) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(row.mtime.max(0) as u64)
}

/// Whether a scanned entry is the temporary link `swap_in` leaves when its
/// rename over the old name never lands (see [`Materializer::discard_leftover_replacement`]):
/// its name is exactly `.konedrive-new-<id>`, and that same id is also
/// carried by another entry the scan found — the real file the replacement
/// was for. A folder waiting under this name to be placed (the ordinary,
/// single-entry case) is never a file, and its id appears nowhere else yet.
fn is_leftover_replacement(entry: &Scanned, id: &str, counts: &HashMap<&str, usize>) -> bool {
    !entry.is_dir && entry.rel.file_name() == Some(OsStr::new(&format!("{NEW_PREFIX}{id}"))) && counts.get(id).copied().unwrap_or(0) > 1
}

/// Whether deleting or replacing this file would lose something only this
/// machine has: a downloaded file changed since (its stamp does not match), or
/// a file of ours in a state nobody can vouch for that holds data.
pub(crate) fn holds_local_work(file: &File) -> bool {
    use std::os::unix::fs::MetadataExt;
    let blocks = file.metadata().map(|m| m.blocks()).unwrap_or(1);
    match read_state(file) {
        Ok(Some(State::Hydrated)) => {
            let empty = file.metadata().map(|m| m.len() == 0).unwrap_or(false);
            !empty && !matches!(stamp_matches(file), Ok(true))
        }
        Ok(Some(State::OnlineOnly | State::Hydrating | State::Dehydrating)) => false,
        Ok(None) | Err(_) => blocks > 0,
    }
}

/// [`holds_local_work`] for read-write mode, where a download can be edited:
/// one emptied here (its stamp no longer matching) holds local work too.
/// An empty file from the cloud is stamped as it is made, so it never does.
pub(crate) fn holds_local_work_rw(file: &File) -> bool {
    match read_state(file) {
        Ok(Some(State::Hydrated)) => !matches!(stamp_matches(file), Ok(true)),
        _ => holds_local_work(file),
    }
}
#[derive(Debug)]
pub enum ReplaceOutcome {
    Replaced,
    /// Nothing to do any more: the file moved, changed, was freed up or is
    /// already this version. The next cycle looks again.
    Current,
    Failed(String),
    /// Failed for want of disk space: the disk cannot hold both versions, or
    /// filled up while the new one downloaded. Said and retried as `Failed`
    /// is; told apart so that the activity log can say exactly "not enough
    /// disk space".
    NoSpace(String),
    /// Read-write mode: someone has the file open, so no write lease (write
    /// design §3.7). The old version stays, and so does its base; the next
    /// cycle tries again. Not a failure.
    Busy,
}

/// Read-write mode's replacement (`docs/design/writes.md` §9): the swap runs
/// under the per-root tree lock and a write lease on the old file, and the
/// new version's deferred change becomes the base as it lands.
pub struct Leased<'a> {
    pub tree_lock: &'a tokio::sync::Mutex<()>,
    pub store: &'a Store,
}

/// The margin `replace` keeps free beside the new version's own bytes when
/// checking whether the disk can hold both at once: room for
/// filesystem bookkeeping and whatever else is filling or freeing up
/// concurrently, not just the new version's exact byte count.
const REPLACE_SPACE_MARGIN: u64 = 64 << 20;

/// `disk.dir(parent)` for a replacement already in flight: a folder above the
/// file having moved or been removed since is not a failure to report and
/// retry forever — it is nothing left to do here; the file
/// is wherever the tree now says it is, and the next cycle looks there. Any
/// other error is still an error.
fn replacement_dir(disk: &Disk, parent: &Path) -> std::io::Result<Option<File>> {
    match disk.dir(parent) {
        Ok(dir) => Ok(Some(dir)),
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// A file's identity strong enough to survive `old` being dropped and
/// reopened later, rather than kept open for the whole download (round 1,
/// issue 3). `(dev, ino)` alone is not enough (round 2's finding): once its
/// descriptor is closed, nothing pins the inode number — it can be freed and
/// reused by an unrelated file created while the new version downloads, and
/// the two would then compare equal. [`Fingerprint`] tells a reused inode
/// apart from the file that had it before.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    fingerprint: Fingerprint,
}

/// What distinguishes a file from another that happens to reuse its inode:
/// its birth time where the filesystem reports one (`statx`'s `btime` on
/// Linux, behind `Metadata::created()`), or — where it does not — its mtime
/// and size, which the stamp check already constrains for any file
/// `replace` considers taking as "the same old file".
#[derive(Debug, Clone, Copy, PartialEq)]
enum Fingerprint {
    Born(SystemTime),
    Stamp { mtime: SystemTime, size: u64 },
}

impl FileIdentity {
    fn of(file: &File) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        let fingerprint = match meta.created() {
            Ok(created) => Fingerprint::Born(created),
            Err(_) => Fingerprint::Stamp { mtime: meta.modified()?, size: meta.len() },
        };
        Ok(Self { dev: meta.dev(), ino: meta.ino(), fingerprint })
    }
}

/// The new version of a downloaded file is fetched into a nameless
/// file beside it, verified, labelled and swapped in with one rename. Never
/// written over the old one in place — a reader would see a mix. If the disk
/// cannot hold both, the old version stays.
pub async fn replace(disk: &Disk, locks: &InodeLocks, source: &dyn ContentSource, r: &Replacement) -> ReplaceOutcome {
    replace_leased(disk, locks, source, r, None).await
}

/// [`replace`], in read-write mode when `leased` is given ([`Leased`]): a
/// file someone has open is not downloaded again nor swapped
/// ([`ReplaceOutcome::Busy`]) — a writer would lose what it writes into the
/// unlinked inode.
pub async fn replace_leased(disk: &Disk, locks: &InodeLocks, source: &dyn ContentSource, r: &Replacement, leased: Option<&Leased<'_>>) -> ReplaceOutcome {
    match replace_inner(disk, locks, source, r, leased).await {
        Ok(outcome) => outcome,
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT)) => ReplaceOutcome::NoSpace(format!(
            "not enough space to finish the new version of {}; the old version stays",
            r.rel.display()
        )),
        Err(e) => ReplaceOutcome::Failed(e.to_string()),
    }
}

async fn replace_inner(disk: &Disk, locks: &InodeLocks, source: &dyn ContentSource, r: &Replacement, leased: Option<&Leased<'_>>) -> std::io::Result<ReplaceOutcome> {
    let parent = r.rel.parent().unwrap_or(Path::new(""));
    let Some(name) = r.rel.file_name() else { return Ok(ReplaceOutcome::Current) };
    let Some(dir) = replacement_dir(disk, parent)? else { return Ok(ReplaceOutcome::Current) };
    // Read-write mode: an emptied download holds local work too.
    let local_work = |file: &File| if leased.is_some() { holds_local_work_rw(file) } else { holds_local_work(file) };
    // Downloaded, of another version, and holding nothing only this machine
    // has: looked at through the file itself, so that it can be asked again
    // under a lease, which the daemon's own open of it would break.
    let replaceable = |old: &File| -> std::io::Result<bool> {
        let hydrated = matches!(read_state(old), Ok(Some(State::Hydrated)));
        let other_version = placeholder::read_ctag(old)?.as_deref() != Some(r.ctag.as_str());
        Ok(hydrated && other_version && !local_work(old))
    };
    let still_there = |dir: &File| -> std::io::Result<Option<File>> {
        match disk.probe(dir, name)? {
            Probe::Managed { id, is_dir: false } if id == r.id => {}
            _ => return Ok(None),
        }
        let old = match disk.open_file(dir, name) {
            Ok(old) => old,
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR)) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(replaceable(&old)?.then_some(old))
    };
    let Some(old) = still_there(&dir)? else { return Ok(ReplaceOutcome::Current) };
    // Read-write mode: open somewhere now, it would be again at the swap — no
    // download for nothing.
    if leased.is_some() && konedrive_fs::lease::WriteLease::take(&old)?.is_none() {
        return Ok(ReplaceOutcome::Busy);
    }
    // Kept only as identity from here, not as a hold on the file: a Free up
    // space must be able to take the old file's write lease while the new
    // version downloads, which it could not while `old` stayed open for the
    // whole download.
    let old_identity = FileIdentity::of(&old)?;
    let old_key = super::InodeKey::of(&old)?;
    drop(old);

    // Both versions must fit at once; checked before anything is
    // downloaded, so that a full disk costs no bandwidth on every retry.
    let fs = nix::sys::statvfs::fstatvfs(&dir)?;
    let free = fs.blocks_available() as u64 * fs.fragment_size() as u64;
    if free < r.size.saturating_add(REPLACE_SPACE_MARGIN) {
        return Ok(ReplaceOutcome::NoSpace(format!(
            "not enough space to download the new version of {} beside the old one; the old version stays",
            r.rel.display()
        )));
    }

    let new = disk.tmpfile(&dir)?;
    placeholder::write_item_id(&new, &r.id)?;
    let downloaded = match super::source::download_into(&new, &r.id, source).await {
        Ok(downloaded) => downloaded,
        Err(errno) if errno == libc::ENOSPC || errno == libc::EDQUOT => {
            return Ok(ReplaceOutcome::NoSpace(format!(
                "not enough space to download the new version of {}; the old version stays",
                r.rel.display()
            )))
        }
        Err(errno) => {
            return Ok(ReplaceOutcome::Failed(format!(
                "the new version of {} could not be downloaded ({}); the old version stays",
                r.rel.display(),
                std::io::Error::from_raw_os_error(errno)
            )))
        }
    };
    new.set_len(downloaded.size)?;
    if let Err(e) = placeholder::set_mtime(&new, downloaded.mtime) {
        tracing::warn!("{}: cannot apply the new version's mtime: {e}", r.rel.display());
    }
    new.sync_data()?;
    if let Some(version) = &downloaded.version {
        placeholder::write_ctag(&new, &version.ctag)?;
    }
    placeholder::write_stamp(&new)?;
    placeholder::write_state(&new, State::Hydrated)?;
    placeholder::set_mode(&new, if disk.locked() { LOCKED_FILE_MODE } else { OPEN_FILE_MODE })?;
    new.sync_all()?;

    // The swap, under the old file's lock, after looking again: a Free up
    // space, a fill or a local edit may have happened while this downloaded.
    // Read-write mode takes the tree lock first, the worker's order.
    let _tree = match leased {
        Some(leased) => Some(leased.tree_lock.lock().await),
        None => None,
    };
    let _guard = locks.lock(old_key).await;
    let Some(dir) = replacement_dir(disk, parent)? else { return Ok(ReplaceOutcome::Current) };
    let now = still_there(&dir)?;
    let same_file = match &now {
        Some(now) => FileIdentity::of(now)? == old_identity,
        None => false,
    };
    if !same_file {
        return Ok(ReplaceOutcome::Current);
    }
    // Read-write mode: nobody has the old file open across the rename, or
    // what they write would land in the unlinked inode (§3.7). The lease
    // first; then, under it, the file is looked at again — through the
    // descriptor, and by name without opening it — so that a write that
    // landed before the lease is a stamp mismatch, never swapped away.
    let _lease = match (leased, &now) {
        (Some(_), Some(now)) => match konedrive_fs::lease::WriteLease::take(now)? {
            Some(lease) => {
                let meta = now.metadata()?;
                let named = nix::sys::stat::fstatat(dir.as_fd(), name, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW)
                    .is_ok_and(|at| (at.st_dev, at.st_ino) == (meta.dev(), meta.ino()));
                if !named || FileIdentity::of(now)? != old_identity || !replaceable(now)? {
                    return Ok(ReplaceOutcome::Current);
                }
                Some(lease)
            }
            None => return Ok(ReplaceOutcome::Busy),
        },
        _ => None,
    };
    // A pin of the file's own goes with it to the new version, which is
    // another inode.
    if let Some(now) = &now {
        if placeholder::read_pin(now)? {
            placeholder::write_pin(&new)?;
        }
    }
    let temp = format!("{NEW_PREFIX}{}", r.id);
    clear_leftover_link(disk, &dir, OsStr::new(&temp), &r.id)?;
    disk.swap_in(&dir, &new, OsStr::new(&temp), name)?;
    if let Some(leased) = leased {
        // The base takes the version the file now holds (the read-write reconcile must, items 1 and 4).
        let ctag = placeholder::read_ctag(&new).ok().flatten();
        let handle = konedrive_fs::handle::FileHandle::of(&new).ok();
        let id = r.id.clone();
        if let Err(e) = leased.store.call(move |s| s.land_deferred(&id, ctag.as_deref(), handle.as_ref())).await {
            tracing::warn!("{}: the new version is in place, and its base waits for the next cycle: {e}", r.rel.display());
        }
    }
    Ok(ReplaceOutcome::Replaced)
}

/// Removes the temporary link an earlier swap of the same file left — a
/// crash between its link and its rename — which made every later swap fail
/// `EEXIST` until a Full reconcile cleared it. Only
/// a plain file carrying this very item id and no local work goes; anything
/// else is left, and the swap fails as before. A Full reconcile rescues one
/// that holds work (`discard_leftover_replacement`).
fn clear_leftover_link(disk: &Disk, dir: &File, temp: &OsStr, id: &str) -> std::io::Result<()> {
    if let Probe::Managed { id: found, is_dir: false } = disk.probe(dir, temp)? {
        if found == id && !holds_local_work(&disk.open_file(dir, temp)?) {
            tracing::info!("clearing the temporary link {} an earlier replacement left", temp.to_string_lossy());
            disk.remove(dir, temp, false)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
