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
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::{self, read_state, stamp_matches, PlaceholderSpec, State, LOCKED_FILE_MODE, OPEN_FILE_MODE};
use tokio_util::sync::CancellationToken;

use crate::folder::disk::{Disk, Probe, Scanned, HOLDING, NEW_PREFIX};
use crate::status::activity::Kind as EventKind;
use crate::helper::HelperLink;
use crate::folder::locks::InodeLocks;
use crate::remote::mode::Mode;
use konedrive_tree::{Kind, Located, Placement, Plan, Row, Store, Table};

/// What the mode answers: where a read-only and a read-write folder differ.
mod answers;
/// What a pass did, left for later, and how it failed.
mod applied;
pub use applied::{Applied, ApplyError, Changed, Copied, Counts, Failed, Kept, OnDisk, Pending, Replacement, Rescued};
/// Read-write mode's rules (`docs/design/writes.md` §9).
mod rw;
pub use rw::Rw;
use rw::{Scan, Unplaced};

/// A file found where the tree wants it: left, updated, queued for replacement or rescued.
mod file;
/// The holding directory, and what is rescued or set aside from it.
mod holding;
/// The one way a managed object is taken off the disk: forgotten first.
mod removal;
#[cfg(test)]
pub(in crate::remote) use removal::testing::before_the_next_removal;
/// One downloaded file swapped for its new version.
mod replace;
use file::cloud_time;
pub use replace::{replace, replace_leased, replace_until, Failure, FailureReason, Leased, ReplaceOutcome};

/// Whether another account of this daemon claims an item id (`docs/design/writes.md` §8.3): an object
/// carrying it is never removed by this folder's reconcile.
pub type Claimed = std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>;

pub enum Scope {
    /// Scan the folder and match it to the whole tree.
    Full,
    /// Only these items (a delta's), and what comes into view with them.
    Changed(Vec<String>),
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
    /// The mode, with a read-write folder's rules (`docs/design/writes.md`
    /// §9), which the commit after the pass goes by too. Asked through the
    /// questions of `answers`.
    pub mode: Mode<std::sync::Arc<Rw>>,
    /// Asked before an object with an id this folder does not know is
    /// removed: another account's is set aside instead, alive, for that
    /// account's move out to download where it is. `None`
    /// where no other account can be (tests).
    pub claimed: Option<Claimed>,
}

/// What a pass that failed hands to the Full pass after it.
#[derive(Default)]
struct Handover {
    on_disk: OnDisk,
    moved_from: HashMap<String, PathBuf>,
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
    /// [`PLACED_BATCH`] at a time, and at the end of the run.
    placed: Vec<(String, konedrive_fs::handle::FileHandle)>,
    /// What removals left in place so far, as the user's own.
    kept: Kept,
    /// Read-write mode: items that can no longer be placed, whose objects
    /// this run takes off, or leaves, once everything else is placed. One
    /// that stands at a name another item takes in this run steps aside
    /// for it ([`Materializer::step_aside`]).
    leaving: HashSet<String>,
    /// Where each of those that stepped aside stands now.
    aside: HashMap<String, PathBuf>,
}

/// Placed items recorded in one transaction (a guess).
pub const PLACED_BATCH: usize = 500;

/// Items whose plan, or whose rows, are read in one store call (a guess):
/// the store's thread serves others between two calls.
const PLAN_BATCH: usize = 500;

impl Run {
    /// Notes what a Changed scope did to `rel`; a Full scope notes nothing
    /// (see [`Applied::changes`]).
    fn note(&mut self, kind: EventKind, rel: &Path, from: Option<PathBuf>) {
        if self.scope.is_some() {
            self.out.changes.push(Changed { kind, rel: rel.to_path_buf(), from });
        }
    }
}

/// An object of ours the Full scan found, with what phase 1 sorts it by.
pub(in crate::remote::materialize) struct Seen<'a> {
    pub entry: &'a Scanned,
    pub id: &'a String,
    /// The new tree's row of its item.
    pub new: Option<&'a Row>,
    /// The plan of the misplaced entries of its batch.
    pub plan: &'a Plan,
    /// How many objects the scan found carrying each id.
    pub counts: &'a HashMap<&'a str, usize>,
}

/// What phase 1 does with an object of ours.
pub(in crate::remote::materialize) enum Sorted {
    /// It is left as it is.
    Stays,
    /// It goes to the holding directory: phase 2 places it from there, or
    /// the drain takes what is left.
    ToHolding,
    /// Removed in OneDrive: taken off the disk where it stands.
    Removed,
    /// OneDrive still has it and the folder cannot hold it any more, and it
    /// is the topmost such item of its subtree: once everything else is
    /// placed it goes whole or waits whole, from where it `stands` (with
    /// whether that is a directory). `None`: this pass does not look at it
    /// — a local change holds it, or it is not where the base has it — and
    /// what follows it goes its way all the same.
    Leaves { stands: Option<(PathBuf, bool)> },
    /// It is below an item that leaves, and follows it.
    Follows,
    /// The Full scope: a replacement's leftover link, discarded.
    Leftover,
}

impl Materializer {
    /// One pass over `scope`.
    pub fn apply(&self, scope: Scope) -> Result<Applied, ApplyError> {
        self.pass(scope, &mut Handover::default())
    }

    /// A pass over `scope`, and, when a Changed one finds that the folder
    /// does not match the stored tree, a Full one after it. What was
    /// applied, and whether a Full pass ran.
    ///
    /// What the failed pass did on disk stands whatever the Full one does
    /// ([`OnDisk`]): its rescues, conflict copies, places to examine,
    /// folders made local, and what it forgot and took off. What it left
    /// unsettled is not carried: the Full pass decides that again.
    pub fn apply_with_handover(&self, scope: Scope) -> Result<(Applied, bool), Box<Failed>> {
        let changed = matches!(scope, Scope::Changed(_));
        let mut over = Handover::default();
        let passed = match self.pass(scope, &mut over) {
            Err(ApplyError::NeedFull(why) | ApplyError::Io(why)) if changed => {
                tracing::info!("{why}; reconciling the whole folder");
                self.pass(Scope::Full, &mut over).map(|applied| (applied, true))
            }
            other => other.map(|applied| (applied, !changed)),
        };
        let (mut applied, full) = match passed {
            Ok(passed) => passed,
            Err(error) => return Err(Box::new(Failed { error, done: over.on_disk })),
        };
        let mut on_disk = over.on_disk;
        on_disk.absorb(std::mem::take(&mut applied.on_disk));
        applied.on_disk = on_disk;
        Ok((applied, full))
    }

    /// One pass. When it fails, what it did on disk goes into `over`, with
    /// where each item it moved to the holding directory came from, so that
    /// the pass after it shows, or puts back, there what it does not place.
    fn pass(&self, scope: Scope, over: &mut Handover) -> Result<Applied, ApplyError> {
        let mut run = Run { moved_from: std::mem::take(&mut over.moved_from), ..Run::default() };
        let result = self.apply_run(scope, &mut run);
        if result.is_err() {
            over.on_disk.absorb(std::mem::take(&mut run.out.on_disk));
            over.moved_from = std::mem::take(&mut run.moved_from);
        }
        result.map(|()| run.out)
    }

    fn apply_run(&self, scope: Scope, run: &mut Run) -> Result<(), ApplyError> {
        let result = self.apply_run_placing(scope, run);
        // What was placed is recorded, whatever became of the run.
        let recorded = self.record_placed_now(run);
        result.and(recorded)
    }

    /// Notes the inode item `id` was just placed as, `name` in `dir`, by
    /// name, opening nothing; recorded with the rest of its batch. A
    /// filesystem that gives no handles leaves it unrecorded: such an
    /// item is never deleted in OneDrive for being missing, since the
    /// examination cannot prove it gone.
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
        match scope {
            Scope::Full => self.full(run)?,
            Scope::Changed(ids) => self.changed(ids, run)?,
        }
        self.drain_holding(run)?;
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

    /// The plan of `ids`: what the base and the new tree have of each, read
    /// [`PLAN_BATCH`] items to a store call.
    fn plan(&self, ids: &[String]) -> Result<Plan, ApplyError> {
        let mut plan = Plan::default();
        for batch in ids.chunks(PLAN_BATCH) {
            let batch = batch.to_vec();
            plan.absorb(self.store.call_blocking(move |s| s.plan(&batch))?);
        }
        Ok(plan)
    }

    /// The new tree's rows of what the Full scan found in `entries`, in one
    /// store call: enough to tell what is misplaced.
    fn new_rows_of(&self, entries: &[Scanned]) -> Result<HashMap<String, Row>, ApplyError> {
        let ids: Vec<String> = entries.iter().filter_map(|entry| entry.id.clone()).collect();
        Ok(self.store.call_blocking(move |s| s.new_rows(&ids))?)
    }

    /// The plan of the entries of `entries` that are misplaced by `rows`
    /// ([`Self::new_rows_of`]): the only ones a Full scan asks it of.
    fn plan_misplaced(&self, entries: &[Scanned], rows: &HashMap<String, Row>) -> Result<Plan, ApplyError> {
        let ids: Vec<String> = entries.iter().filter(|entry| entry.id.as_ref().is_some_and(|id| is_misplaced(entry, rows.get(id)))).filter_map(|entry| entry.id.clone()).collect();
        self.plan(&ids)
    }

    /// The plan of the Changed scope, read once: the delta's items, and
    /// everything the new tree has below one that comes into view.
    fn plan_changed(&self, ids: &[String]) -> Result<Plan, ApplyError> {
        let mut plan = self.plan(ids)?;
        let shown: Vec<String> = ids.iter().filter(|id| plan.of(id).comes_into_view()).cloned().collect();
        if !shown.is_empty() {
            let below = self.store.call_blocking(move |s| {
                let mut below = Vec::new();
                for id in &shown {
                    below.extend(s.descendants(Table::Staging, id)?);
                }
                Ok(below)
            })?;
            // One below another that comes into view is planned already.
            let below: Vec<String> = below.into_iter().filter(|id| !plan.has(id)).collect();
            plan.absorb(self.plan(&below)?);
        }
        Ok(plan)
    }

    /// The Changed scope: every item of its plan but the root. The root's
    /// own entry changes with every change below it; it is the folder
    /// itself, never something to move.
    fn scope_of(&self, plan: &Plan) -> HashSet<String> {
        plan.ids().filter(|id| **id != self.root_item_id).cloned().collect()
    }

    /// The Full scope (§3.7), in three phases. Phase 1: the scan, and what
    /// it found sorted ([`Self::sort_scanned`]); then, deepest first, in one
    /// order, so that nothing is moved out from above what is still to be
    /// done below it, what moved goes to the holding directory and what
    /// OneDrive removed goes where it stands. Phase 2: the new tree top
    /// down. Phase 3: what can no longer be placed goes or waits
    /// ([`Self::after_placement`]).
    fn full(&self, run: &mut Run) -> Result<(), ApplyError> {
        self.check_cancel()?;
        let scanned = self.disk.scan(&self.root_item_id)?;
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for entry in &scanned {
            if let Some(id) = &entry.id {
                *counts.entry(id.as_str()).or_insert(0) += 1;
            }
        }
        let mut scan = Scan::default();
        let mut unplaced = Unplaced::default();
        let mut misplaced: Vec<(&Scanned, bool)> = Vec::new();
        for entries in scanned.chunks(PLAN_BATCH) {
            let rows = self.new_rows_of(entries)?;
            let plan = self.plan_of_misplaced(entries, &rows)?;
            for entry in entries {
                let Some(id) = &entry.id else { continue };
                let seen = Seen { entry, id, new: rows.get(id), plan: &plan, counts: &counts };
                match self.sort_scanned(&seen, &mut scan, run)? {
                    Sorted::Stays => {}
                    Sorted::Leftover => {
                        self.check_cancel()?;
                        self.discard_leftover_replacement(entry, run)?;
                    }
                    Sorted::ToHolding => misplaced.push((entry, false)),
                    Sorted::Removed => misplaced.push((entry, true)),
                    Sorted::Leaves { stands } => unplaced.top(id, &entry.rel, stands),
                    Sorted::Follows => unplaced.follows(id, &entry.rel, seen.new.is_some_and(|new| new.placement != Placement::Placed)),
                }
            }
        }
        misplaced.sort_by_key(|m| std::cmp::Reverse(m.0.depth));
        for (entry, removed) in misplaced {
            self.check_cancel()?;
            let id = entry.id.as_deref().expect("sorted by its id");
            if !removed {
                self.to_holding(&entry.rel, id, run)?;
            } else if self.remove_in_place(&entry.rel, run)? {
                // A mount inside it: its removal waits.
                run.left.insert(id.to_owned());
                run.out.pending.unsettled.insert(id.to_owned());
            }
        }
        let root = self.disk.dir(Path::new(""))?;
        self.disk.lock_dir(&root)?;
        let mut queue = VecDeque::from([(self.root_item_id.clone(), PathBuf::new())]);
        while let Some((id, rel)) = queue.pop_front() {
            self.check_cancel()?;
            let children = self.store.call_blocking(move |s| s.children(Table::Staging, &id))?;
            for row in children {
                if row.placement != Placement::Placed || self.passes_over(&row.id) {
                    continue;
                }
                if self.left_alone(&row.id, run) {
                    self.unsettle_tree(&row.id, run)?;
                    continue;
                }
                let Some(placed) = self.place(&row, &rel, run, true)? else {
                    self.unsettle_tree(&row.id, run)?;
                    continue;
                };
                if row.kind == Kind::Folder {
                    queue.push_back((row.id.clone(), placed));
                }
            }
        }
        self.after_placement(unplaced, run)
    }

    /// The Changed scope (§3.7), in the same three phases. Phase 1, by
    /// where things are now, deepest first: each item of the plan is sorted
    /// ([`Self::sort_changed`]) and moved to the holding directory or taken
    /// off where it stands. Phase 2, by where things belong, shallowest
    /// first. Phase 3 as in the Full scope.
    fn changed(&self, ids: Vec<String>, run: &mut Run) -> Result<(), ApplyError> {
        // What an earlier run left in the holding directory is not this
        // delta's to drain; a Full reconcile sorts it out by item id.
        if self.holding_if_any()?.is_some() {
            return Err(ApplyError::NeedFull(format!("{HOLDING} is left from an earlier run")));
        }
        let plan = self.plan_changed(&ids)?;
        let scope = self.scope_of(&plan);
        run.scope = Some(scope.clone());

        let mut here: Vec<(&String, &Located)> = scope.iter().filter_map(|id| Some((id, plan.of(id).base_place()?))).collect();
        here.sort_by_key(|h| std::cmp::Reverse(h.1.depth));
        let no_longer_placed = self.no_longer_placed(&plan, &here);
        let mut unplaced = Unplaced::default();
        for (id, old) in here {
            self.check_cancel()?;
            match self.sort_changed(id, old, plan.of(id), &no_longer_placed, run)? {
                Sorted::Stays | Sorted::Leftover => {}
                Sorted::ToHolding => self.to_holding(&old.rel, id, run)?,
                Sorted::Removed => {
                    if self.remove_in_place(&old.rel, run)? {
                        // A mount inside it: its removal waits.
                        run.out.pending.unsettled.insert(id.clone());
                    }
                }
                Sorted::Leaves { stands } => unplaced.top(id, &old.rel, stands),
                // Only what the new tree no longer places follows in this scope.
                Sorted::Follows => unplaced.follows(id, &old.rel, true),
            }
        }

        let mut there = placed_by_the_new_tree(&plan, &scope);
        there.sort_by_key(|t| t.1.depth);
        // Folders placed — and so checked — by this phase.
        let mut placed: HashSet<String> = HashSet::new();
        for (row, new) in there {
            self.check_cancel()?;
            if self.passes_over(&row.id) {
                continue;
            }
            if self.left_alone(&row.id, run) {
                self.unsettle_tree(&row.id, run)?;
                continue;
            }
            let parent = new.rel.parent().unwrap_or(Path::new(""));
            if let Err(e) = self.check_parent(row, parent, &placed) {
                if self.asks_for_the_scan(&row.id) {
                    return Err(e);
                }
                self.unsettle_tree(&row.id, run)?;
                continue;
            }
            match self.place(row, parent, run, false)? {
                Some(_) if row.kind == Kind::Folder => {
                    placed.insert(row.id.clone());
                }
                Some(_) => {}
                None => self.unsettle_tree(&row.id, run)?,
            }
        }
        self.after_placement(unplaced, run)
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
    /// `None` when it is left as it is and its change waits, which only a
    /// read-write folder does (see [`Rw`]).
    fn place(&self, row: &Row, parent_rel: &Path, run: &mut Run, full: bool) -> Result<Option<PathBuf>, ApplyError> {
        let rel = parent_rel.join(&row.name);
        let dir = self.disk.dir(parent_rel)?;
        let name = OsStr::new(&row.name);
        let is_folder = row.kind == Kind::Folder;
        match self.disk.probe(&dir, name)? {
            Probe::Managed { id, is_dir } if id == row.id && is_dir == is_folder => {
                if self.has_no_record(&row.id) {
                    self.record_placed(run, &dir, name, &row.id)?;
                }
                if !is_folder {
                    self.check_file(&dir, name, row, &rel, run)?;
                }
                if full {
                    // Not a file a fill holds: it is left for the next
                    // Full reconcile.
                    self.disk.enforce_mode(&dir, name, |file| Ok(self.locks.try_lock(crate::folder::locks::InodeKey::of(file)?)))?;
                }
                return Ok(Some(rel));
            }
            Probe::Managed { id, .. } if id == row.id => {
                // Its own id with the wrong kind: nothing a Graph id does.
                // Not trusted, not thrown away.
                self.out_of_the_way(&dir, name, &rel, run)?;
            }
            // What can no longer be placed yields its name: it steps aside
            // where it is, and leaves from there, or waits there.
            Probe::Managed { id, .. } if run.leaving.contains(&id) => {
                if !self.step_aside(&dir, name, &rel, &id, run)? {
                    run.out.pending.unsettled.insert(row.id.clone());
                    return Ok(None);
                }
            }
            Probe::Managed { id, .. } => {
                if self.keeps_its_name(&id, &rel, run)? {
                    run.out.pending.unsettled.insert(row.id.clone());
                    return Ok(None);
                }
                if run.scope.as_ref().is_some_and(|scope| !scope.contains(&id)) {
                    return Err(ApplyError::NeedFull(format!("{id} is in the way at {}", rel.display())));
                }
                self.to_holding(&rel, &id, run)?;
            }
            Probe::Unmanaged { is_dir } => {
                if self.is_the_users(row, &rel, is_folder && is_dir) {
                    run.out.pending.unsettled.insert(row.id.clone());
                    return Ok(None);
                }
                self.out_of_the_way(&dir, name, &rel, run)?;
            }
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
                    // The rows below a folder went to the holding directory
                    // with it, and follow it out.
                    if is_folder {
                        self.rows_follow(PathBuf::from(HOLDING).join(&row.id), rel.clone())?;
                    }
                    self.record_placed(run, &dir, name, &row.id)?;
                    run.out.counts.moved += 1;
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
        if !self.places_missing(row, &rel, run)? {
            run.out.pending.unsettled.insert(row.id.clone());
            return Ok(None);
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
        run.out.counts.created += 1;
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
}

/// Whether a scanned entry is not where the new tree has its item, whose
/// row there is `new`: the tree does not have the item, does not place it,
/// or has it in another folder, under another name, or as the other kind.
fn is_misplaced(entry: &Scanned, new: Option<&Row>) -> bool {
    let Some(new) = new else { return true };
    new.placement != Placement::Placed
        || new.parent_id != entry.parent_id
        || entry.rel.file_name() != Some(OsStr::new(&new.name))
        || (new.kind == Kind::Folder) != entry.is_dir
}

/// The items of `scope` the new tree places, each with its row and place.
fn placed_by_the_new_tree<'a>(plan: &'a Plan, scope: &HashSet<String>) -> Vec<(&'a Row, &'a Located)> {
    scope
        .iter()
        .filter_map(|id| {
            let new = plan.of(id).new.as_ref()?;
            Some((&new.row, new.place()?))
        })
        .collect()
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

#[cfg(test)]
mod tests;
