//! The reconcile in read-write mode (`docs/design/writes.md` §9, §7).
//!
//! The read phase makes the folder match the tree; in read-write mode the
//! folder also holds the user's own changes, which the outbox sends. So the
//! reconcile here:
//!
//! - **keeps its hands off what a local change holds** ([`Rw::held`]): an
//!   item with a live outbox row, in any state, and what the base has below
//!   a folder such a row moves — not moved, replaced or removed. Nor what it
//!   finds away from where the base has it, a local move or copy not
//!   examined yet, with everything below it. Their changes wait
//!   ([`Pending::unsettled`](super::Pending::unsettled)): the base keeps the
//!   version the disk holds;
//! - **places nothing below a folder being removed** ([`Rw::removing`]):
//!   below a live `delete` or `move-out` row the delta's changes go to the
//!   base, and nothing goes to the disk (the read-write reconcile must, item 3);
//! - **places a missing item again only when there is something to place**
//!   ([`Rw::revive`]): new in OneDrive, or with no local object on record (a
//!   rebuilt base, one the outbox forgot, a restore). Anything else missing is
//!   a delete or a move the examination has still to see — a changed one too:
//!   its object may be alive out of the folder, and only once the
//!   examination and the outbox have decided (delete × edit: OneDrive wins,
//!   §6) is it placed again;
//! - **keeps both where the read phase rescued** (§6): a local version in the
//!   way is renamed beside the cloud's, `name-<machine>.ext`, and uploaded
//!   as new ([`Materializer::copy_aside`]);
//! - **removes what OneDrive removed, in the cycle, and keeps what it never
//!   had**: what OneDrive had and the daemon placed goes — a file not
//!   downloaded, a download unchanged since, a download into it stopped, a
//!   folder once nothing is left in it. A file made here and a download
//!   changed here stay, with their folders, their attributes off, and go up
//!   as new; nothing is rescued out of the folder
//!   (`resyncChangesUploadDifferences` keeps every download too, §3.7);
//! - **lets what is no longer placed leave once its uploads are done**
//!   (issue #104): its placement is the base's at once, its object stays
//!   and is examined, and goes once nothing in it waits for the outbox
//!   ([`Materializer::leaving_rw`]);
//! - **forgets before it removes**: the local objects of everything it takes
//!   off the disk are forgotten in the store first, so that no examination
//!   can prove one gone and delete it in OneDrive
//!   ([`Materializer::take_off`], the only way anything is taken off);
//! - **never moves anything out of the folder**: what it moved to the holding
//!   directory — this run, or one a stop cut short — is placed from there or
//!   put back where it was, never rescued outside, where it would be taken
//!   for a move out and deleted in OneDrive.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder;

use super::removal::Policy;
use super::{is_leftover_replacement, is_misplaced, placed_by_the_new_tree, ApplyError, Copied, Materializer, Run, PLAN_BATCH};
use crate::folder::disk::{Probe, Scanned, HOLDING, NEW_PREFIX};
use crate::local::IgnoreList;
use crate::local::names::copy_name;
use konedrive_tree::outbox::{OutboxOp, SWAP_PREFIX};
use konedrive_tree::{Kind, Located, Placement, Planned, Table, TreeError, TreeStore};

/// The holding directory in read-write mode: nothing in it leaves the folder.
mod holding;
/// What is no longer placed and stays on disk until its uploads are done (issue #104).
mod leaving;

/// What a read-write folder's reconcile needs to know besides the tree.
#[derive(Debug, Default, Clone)]
pub struct Rw {
    /// Items a live outbox row concerns, and what the base or the new tree
    /// has below a folder such a row moves: hands off, their changes wait.
    pub held: HashSet<String>,
    /// Items a live `delete` or `move-out` row removes, and everything below
    /// them: nothing is placed there, and their changes go to the base.
    pub removing: HashSet<String>,
    /// Where rows with no item id stand — a `create` or a `mkdir` not landed
    /// yet: an object there without an id is the outbox's, not in the way.
    /// Each such place and every folder above it, so that a look is one
    /// lookup (issue #39).
    pub pending: HashSet<PathBuf>,
    /// Items placed again where missing: new in OneDrive, a file whose
    /// content changed there, an item with no local object on record — and
    /// every folder above them.
    pub revive: HashSet<String>,
    /// Items the base records no local object for (F82 (8)).
    pub unplaced: HashSet<String>,
    /// Items the new tree changes: what `staging` differs from `items` by.
    pub changed: HashSet<String>,
    /// For conflict copies: `name-<machine>.ext` (§6).
    pub machine: String,
    /// `resyncChangesUploadDifferences` (§3.7): what OneDrive's new listing
    /// left out and was downloaded here is uploaded again as new, and a
    /// downloaded file that differs from OneDrive's version is kept beside it.
    pub upload_differences: bool,
    /// The account's ignore list: an ignored name is never uploaded, so it
    /// keeps no folder that stopped being placed waiting on disk.
    pub ignore: IgnoreList,
}

impl Rw {
    /// Read from the store once `staging` holds the new tree: the outbox's
    /// live rows, and what the new tree changes.
    pub fn read(s: &TreeStore, machine: String, upload_differences: bool, ignore: IgnoreList) -> Result<Self, TreeError> {
        let mut rw = Rw { machine, upload_differences, ignore, ..Rw::default() };
        for row in s.outbox_rows()? {
            let Some(id) = row.item_id.clone() else {
                rw.pending.extend(row.rel.ancestors().map(Path::to_path_buf));
                continue;
            };
            let folder = s.get(Table::Items, &id)?.is_some_and(|r| r.kind == Kind::Folder);
            if folder {
                // What the base has below the folder goes with the row; what
                // OneDrive moved in from elsewhere since is not the folder's:
                // the base still has it where it was, and its move waits.
                rw.extend_below(s, &id, row.kind.removes())?;
            }
            if row.kind.removes() {
                rw.removing.insert(id);
            } else {
                rw.held.insert(id);
            }
        }
        rw.unplaced = s.unplaced(Table::Staging)?.into_iter().collect();
        let mut wanted: Vec<String> = rw.unplaced.iter().cloned().collect();
        rw.changed = s.changed_ids()?.into_iter().collect();
        for id in &rw.changed {
            let id = id.clone();
            let Some(row) = s.get(Table::Staging, &id)? else { continue };
            let new = match s.get(Table::Items, &id)? {
                None => true,
                Some(base) => row.kind == Kind::File && (row.ctag != base.ctag || row.size != base.size),
            };
            if new {
                wanted.push(id);
            }
        }
        // Each with the folders above it, up to the root.
        let mut parents: HashMap<String, Option<String>> = HashMap::new();
        for id in wanted {
            let mut at = Some(id);
            let mut depth = 0;
            while let Some(id) = at.take() {
                if !rw.revive.insert(id.clone()) || depth > konedrive_fs::MAX_DEPTH {
                    break;
                }
                depth += 1;
                let parent = match parents.get(&id) {
                    Some(parent) => parent.clone(),
                    None => {
                        let parent = s.get(Table::Staging, &id)?.and_then(|r| r.parent_id);
                        parents.insert(id.clone(), parent.clone());
                        parent
                    }
                };
                at = parent;
            }
        }
        Ok(rw)
    }

    /// Everything below folder `id`, which a live row takes away (`removing`)
    /// or moves: the base's descendants go with the row; one the new tree
    /// moves in from elsewhere in the base is held where the base has it, its
    /// move waiting; one new to the base goes with the row too.
    fn extend_below(&mut self, s: &TreeStore, id: &str, removing: bool) -> Result<(), TreeError> {
        let base: HashSet<String> = s.descendants(Table::Items, id)?.into_iter().collect();
        for staged in s.descendants(Table::Staging, id)? {
            if base.contains(&staged) {
                continue;
            }
            if s.get(Table::Items, &staged)?.is_some() {
                self.held.insert(staged);
            } else if removing {
                self.removing.insert(staged);
            } else {
                self.held.insert(staged);
            }
        }
        if removing {
            self.removing.extend(base);
        } else {
            self.held.extend(base);
        }
        Ok(())
    }

    /// Whether a row with no item id stands at `rel`, or below it.
    pub(super) fn pending_at(&self, rel: &Path) -> bool {
        self.pending.contains(rel)
    }

    /// How what OneDrive removed is taken off the disk: what was changed
    /// or made here stays, and after a `resyncChangesUploadDifferences`
    /// listing, which does not mean removed, every download too (§3.7).
    pub(super) fn removed(&self) -> Policy {
        if self.upload_differences {
            Policy::Resync
        } else {
            Policy::Removed
        }
    }

    /// [`Self::removed`] for what is leaving, or is inside it.
    pub(super) fn removed_leaving(&self) -> Policy {
        if self.upload_differences {
            Policy::Resync
        } else {
            Policy::RemovedLeaving
        }
    }

    /// Whether the cloud has something to put at item `id`'s name that the
    /// disk does not show: the new tree changes it, or no local object of it
    /// is on record. Otherwise an object of the user's there — a save by
    /// rename, say — is theirs until the examination sees it (§3.4 rule 7).
    pub(super) fn brings(&self, id: &str) -> bool {
        self.changed.contains(id) || self.unplaced.contains(id)
    }
}

/// Whether `rel` is directly in the holding directory.
fn in_holding(rel: &Path) -> bool {
    rel.parent() == Some(Path::new(HOLDING))
}

/// Whether `rel`'s name is a new folder's or a replacement's temporary one.
fn is_new_name(rel: &Path) -> bool {
    rel.file_name().is_some_and(|n| n.as_encoded_bytes().starts_with(NEW_PREFIX.as_bytes()))
}

/// Where a misplaced entry of the Full scan was, by the base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Was {
    /// At its base place, and the new tree places it elsewhere: moved in
    /// OneDrive — or not the base's at all, as the read phase takes it.
    Moved,
    /// At its base place, or where it stays while it leaves, and the new
    /// tree does not have it: removed in OneDrive.
    Removed,
    /// At its base place, and the new tree has it but does not place it: no
    /// longer placeable here (issue #104).
    Unplaced,
    /// Away from its base place: a local move or copy not examined yet.
    Elsewhere,
    /// Under the outbox's temporary name in OneDrive (F82 (5)): the local
    /// object stays where it is, and the examination moves the item back.
    Swapped,
    /// An id neither the base nor the new tree has: not ours to remove.
    Stranger,
}

/// Whether the new tree has the item under the outbox's temporary name.
fn swapped(planned: &Planned) -> bool {
    planned.new.as_ref().is_some_and(|new| new.row.name.starts_with(SWAP_PREFIX))
}

/// Where the misplaced `entry` was, by its item's plan. It reads nothing and
/// changes nothing: what stays while it leaves is told apart before it
/// ([`Materializer::is_leaving_object`]).
fn where_it_was(entry: &Scanned, planned: &Planned) -> Was {
    if swapped(planned) {
        return Was::Swapped;
    }
    let (staged, placed) = (planned.new.is_some(), planned.new_place().is_some());
    let Some(base) = &planned.base else {
        // Not the base's: one this very placement left (a cycle stopped
        // before its swap) is put where the tree has it; anything else is
        // a file from elsewhere — another folder, another account — the
        // user's, which the examination takes as new (§3.4 rule 6).
        return if staged && placed { Was::Moved } else { Was::Stranger };
    };
    let at_base_place = base.row.parent_id == entry.parent_id && entry.rel.file_name() == Some(OsStr::new(&base.row.name)) && (base.row.kind == Kind::Folder) == entry.is_dir;
    if !base.placed() {
        // Not placed by the base. Where the base has it, it is no longer
        // placeable; anywhere else it is the user's move out of what is
        // leaving, carried out as any other (issue #104).
        return match (staged, placed) {
            (false, _) => Was::Removed,
            (true, false) if at_base_place => Was::Unplaced,
            // Placed by the tree, and not by the base: as for anything
            // away from its base place, the examination decides first.
            (true, _) => Was::Elsewhere,
        };
    }
    if base.row.placement != Placement::Placed || !at_base_place {
        return Was::Elsewhere;
    }
    match (staged, placed) {
        (true, true) => Was::Moved,
        (true, false) => Was::Unplaced,
        (false, _) => Was::Removed,
    }
}

impl Materializer {
    /// The Full scope (§3.7): the scan, then what moved in OneDrive to the
    /// holding directory, then what OneDrive removed, in place, deepest
    /// first; then the new tree top down, as in the read phase.
    pub(super) fn full_rw(&self, rw: &Rw, run: &mut Run) -> Result<(), ApplyError> {
        self.check_cancel()?;
        let scanned = self.disk.scan(&self.root_item_id)?;
        let mut id_counts: HashMap<&str, usize> = HashMap::new();
        for entry in &scanned {
            if let Some(id) = &entry.id {
                *id_counts.entry(id.as_str()).or_insert(0) += 1;
            }
        }
        // Directories a local change holds, with everything below them. The
        // scan lists a directory's entry before what is in it.
        let mut left_dirs: HashSet<PathBuf> = HashSet::new();
        // Directories no longer placed, which stay on disk for now with
        // everything in them, as it is (issue #104).
        let mut leaving_dirs: HashSet<PathBuf> = HashSet::new();
        // What moved in OneDrive goes to the holding directory, what it
        // removed goes in place: deepest first, in one order, so that nothing
        // is moved out from above what is still to be done below it.
        let mut misplaced: Vec<(&Scanned, bool)> = Vec::new();
        let mut leaving = self.leaving_objects()?;
        for entries in scanned.chunks(PLAN_BATCH) {
            let rows = self.new_rows_of(entries)?;
            let plan = self.plan_misplaced(entries, &rows)?;
            for entry in entries {
                let Some(id) = &entry.id else { continue };
                // Left where it is, and so is what is below it. The item is left
                // too — not placed, its change waiting — unless another object
                // carries its id: a copy that kept the attributes, which the
                // examination tells from the item.
                let leave = |run: &mut Run, left_dirs: &mut HashSet<PathBuf>| {
                    if id_counts.get(id.as_str()).copied().unwrap_or(0) <= 1 {
                        run.left.insert(id.clone());
                        // Its change waits too, whatever the tree does with it:
                        // the examination takes the local move on, and the
                        // outbox meets OneDrive's side (§6).
                        run.out.pending.unsettled.insert(id.clone());
                    }
                    if entry.is_dir {
                        left_dirs.insert(entry.rel.clone());
                    }
                };
                // What a reconcile moved to the holding directory — this one's, or
                // one a stop or a crash cut short — is the daemon's own: the
                // placement takes it from there, or the drain puts it back where
                // it was. Never a local move, never out of the folder.
                if in_holding(&entry.rel) {
                    continue;
                }
                // A new folder a stop left under its temporary name goes to the
                // holding directory like anything misplaced: placed from there, or
                // drained. A replacement's leftover link (a file) is the read
                // phase's to recognise, next to its real file.
                if entry.is_dir && is_new_name(&entry.rel) && id_counts.get(id.as_str()).copied().unwrap_or(0) <= 1 {
                    misplaced.push((entry, false));
                    continue;
                }
                if entry.rel.ancestors().skip(1).any(|a| leaving_dirs.contains(a)) {
                    continue;
                }
                if entry.rel.ancestors().skip(1).any(|a| left_dirs.contains(a)) {
                    leave(run, &mut left_dirs);
                    continue;
                }
                if is_leftover_replacement(entry, id, &id_counts) {
                    self.check_cancel()?;
                    self.discard_leftover_replacement(entry, run)?;
                    continue;
                }
                if !entry.is_dir && is_new_name(&entry.rel) {
                    continue;
                }
                let held = rw.held.contains(id) || rw.removing.contains(id);
                if !is_misplaced(entry, rows.get(id)) {
                    if held {
                        leave(run, &mut left_dirs);
                    }
                    continue;
                }
                let planned = plan.of(id);
                // What is below it stays with it, as it is: the leaving pass
                // decides.
                if self.is_leaving_object(&mut leaving, entry, id, planned)? {
                    leaving_dirs.insert(entry.rel.clone());
                    continue;
                }
                match (where_it_was(entry, planned), held) {
                    // Whatever a local change holds, what OneDrive removed goes,
                    // and what is no longer placed is the base's (issue #104).
                    (Was::Removed, _) => misplaced.push((entry, true)),
                    (Was::Unplaced, _) => {
                        self.unplace(&entry.rel, id, entry.is_dir, run)?;
                        leaving = self.leaving_objects()?;
                        leaving_dirs.insert(entry.rel.clone());
                    }
                    (_, true) | (Was::Elsewhere, false) => leave(run, &mut left_dirs),
                    (Was::Moved, false) => misplaced.push((entry, false)),
                    (Was::Swapped | Was::Stranger, false) => {
                        if entry.is_dir {
                            left_dirs.insert(entry.rel.clone());
                        }
                    }
                }
            }
        }
        misplaced.sort_by_key(|m| std::cmp::Reverse(m.0.depth));
        for (entry, removed) in misplaced {
            self.check_cancel()?;
            if removed {
                let parent = entry.rel.parent().unwrap_or(Path::new(""));
                let name = entry.rel.file_name().expect("a scanned entry has a name");
                self.take_off(&self.disk.dir(parent)?, name, &entry.rel, rw.removed(), run)?;
            } else {
                self.to_holding(&entry.rel, entry.id.as_deref().expect("filtered above"), run)?;
            }
        }
        let root = self.disk.dir(Path::new(""))?;
        self.disk.lock_dir(&root)?;
        let mut queue = std::collections::VecDeque::from([(self.root_item_id.clone(), PathBuf::new())]);
        while let Some((id, rel)) = queue.pop_front() {
            self.check_cancel()?;
            let children = self.store.call_blocking(move |s| s.children(Table::Staging, &id))?;
            for row in children {
                if row.placement != Placement::Placed || rw.removing.contains(&row.id) {
                    continue;
                }
                if rw.held.contains(&row.id) || run.left.contains(&row.id) {
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
        Ok(())
    }

    /// The Changed scope (§3.7): as the read phase's, but a disagreement a
    /// local change explains never turns it Full — an item not where the base
    /// has it is left to the examination, and its change waits. Only
    /// something to place again below a folder that is not where the tree
    /// says asks for the scan.
    pub(super) fn changed_rw(&self, rw: &Rw, ids: Vec<String>, run: &mut Run) -> Result<(), ApplyError> {
        if self.holding_if_any()?.is_some() {
            return Err(ApplyError::NeedFull(format!("{HOLDING} is left from an earlier run")));
        }
        let plan = self.plan_changed(&ids)?;
        let scope = self.scope_of(&plan);
        run.scope = Some(scope.clone());

        // Phase 1, by where things are now, deepest first.
        let mut here: Vec<(&String, &Located)> = scope.iter().filter_map(|id| Some((id, plan.of(id).base_place()?))).collect();
        here.sort_by_key(|h| std::cmp::Reverse(h.1.depth));
        for (id, old) in here {
            self.check_cancel()?;
            if rw.removing.contains(id) {
                continue;
            }
            let planned = plan.of(id);
            let placed_now = planned.new_place().is_some();
            let parent = old.rel.parent().unwrap_or(Path::new(""));
            let Some(name) = old.rel.file_name() else { continue };
            let found = match self.disk.dir(parent) {
                Ok(dir) => self.disk.probe(&dir, name)?,
                Err(_) => Probe::Absent,
            };
            let at_place = matches!(&found, Probe::Managed { id: found, .. } if found == id);
            // A local change holds it — unless OneDrive removed it, or it is
            // no longer placed, and its object is where the base has it: that
            // goes, or is the base's, whatever holds it (issue #104).
            if rw.held.contains(id) && (placed_now || !at_place) {
                run.out.pending.unsettled.insert(id.clone());
                continue;
            }
            if !at_place {
                // Deleted or moved here, not examined yet. Where the tree
                // still places it, phase 2 decides; where it removes it, the
                // removal waits: the examination takes the local change
                // on, and the outbox meets OneDrive's side (§6).
                run.missing.insert(id.clone());
                if !placed_now {
                    run.out.pending.unsettled.insert(id.clone());
                }
                continue;
            }
            if planned.stays() {
                continue;
            }
            match (&planned.new, placed_now) {
                (Some(_), true) => self.to_holding(&old.rel, id, run)?,
                (Some(_), false) => self.unplace(&old.rel, id, matches!(found, Probe::Managed { is_dir: true, .. }), run)?,
                (None, _) => {
                    self.take_off(&self.disk.dir(parent)?, name, &old.rel, rw.removed(), run)?;
                }
            }
        }

        // Phase 2, by where things belong, shallowest first.
        let mut there = placed_by_the_new_tree(&plan, &scope);
        there.sort_by_key(|t| t.1.depth);
        let mut placed: HashSet<String> = HashSet::new();
        for (row, new) in there {
            self.check_cancel()?;
            if rw.removing.contains(&row.id) {
                continue;
            }
            if rw.held.contains(&row.id) || run.out.pending.unsettled.contains(&row.id) {
                self.unsettle_tree(&row.id, run)?;
                continue;
            }
            let parent = new.rel.parent().unwrap_or(Path::new(""));
            if let Err(e) = self.check_parent(row, parent, &placed) {
                // Its folder is not where the tree has it. With nothing to
                // place, the change waits; with something, only a scan can
                // tell a folder deleted here from one moved (§3.7).
                if rw.revive.contains(&row.id) {
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
        Ok(())
    }

    /// Whether another item of ours at `rel` is there by a local change —
    /// with a row, below one, or away from where the base has it — so that
    /// it keeps the name: the outbox settles the two (§6).
    pub(super) fn holds_the_name(&self, rw: &Rw, other: &str, rel: &Path, run: &Run) -> Result<bool, ApplyError> {
        if rw.held.contains(other) || rw.removing.contains(other) || run.left.contains(other) {
            return Ok(true);
        }
        let base = self.store.call_blocking({ let other = other.to_owned(); move |s| s.locate(Table::Items, &other) })?;
        Ok(!base.is_some_and(|l| l.placed && l.rel == rel))
    }

    /// Whether `row`, missing from its place, is made again: only with
    /// something to place ([`Rw::revive`]), and never while its local object
    /// is on record. Such an object may still be alive:
    /// elsewhere in the folder, or out of it — a placeholder moved out, which
    /// only the move-out step's `move-out` row marks and downloads; placed again here, the
    /// item would be found at its place and the object outside would read
    /// zeros for good. Its base place goes to the examination instead, which
    /// decides by the object; the outbox then meets OneDrive's change (a
    /// delete or a move out answered `412` is dropped, its object forgotten)
    /// and the next cycle places the item again.
    pub(super) fn place_again(&self, rw: &Rw, row: &konedrive_tree::Row, rel: &Path, run: &mut Run) -> Result<bool, ApplyError> {
        if !rw.revive.contains(&row.id) {
            return Ok(false);
        }
        if self.store.call_blocking({ let row_id = row.id.clone(); move |s| s.local_handle(&row_id) })?.is_some() {
            let base = self.store.call_blocking({ let row_id = row.id.clone(); move |s| s.locate(Table::Items, &row_id) })?.filter(|l| l.placed).map(|l| l.rel);
            run.out.on_disk.examine.push((base.unwrap_or_else(|| rel.to_path_buf()), false));
            return Ok(false);
        }
        Ok(true)
    }

    /// `id` and everything the base or the new tree has below it wait.
    fn unsettle_tree(&self, id: &str, run: &mut Run) -> Result<(), ApplyError> {
        run.out.pending.unsettled.insert(id.to_owned());
        let folder = id.to_owned();
        let below = self.store.call_blocking(move |s| {
            let mut ids = s.descendants(Table::Staging, &folder)?;
            ids.extend(s.descendants(Table::Items, &folder)?);
            Ok(ids)
        })?;
        run.out.pending.unsettled.extend(below);
        Ok(())
    }

    /// Keeps both (§6): the object at `dir/name` (at `rel`) is renamed in its
    /// directory to the first free `name-<machine>.ext`, never over anything,
    /// and becomes the user's own — konedrive's attributes off — to be
    /// uploaded as new; the name is the cloud's again. Rows below a directory
    /// follow it.
    pub(super) fn copy_aside(&self, rw: &Rw, dir: &File, name: &OsStr, rel: &Path, run: &mut Run) -> Result<(), ApplyError> {
        let original = name.to_str().ok_or_else(|| ApplyError::Io(format!("{} has a name that is not UTF-8", rel.display())))?;
        let is_dir = matches!(self.disk.probe(dir, name)?, Probe::Managed { is_dir: true, .. } | Probe::Unmanaged { is_dir: true });
        let mut copy = None;
        for n in 1..=100 {
            let candidate = copy_name(original, &rw.machine, n);
            match self.disk.rename(dir, name, dir, OsStr::new(&candidate)) {
                Ok(()) => {
                    copy = Some(candidate);
                    break;
                }
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        let copy = copy.ok_or_else(|| ApplyError::Io(format!("no free name for a copy of {}", rel.display())))?;
        let copied = OsStr::new(&copy);
        if is_dir {
            placeholder::strip_konedrive_xattrs(&self.disk.open_subdir(dir, copied)?)?;
        } else if let Ok(file) = self.disk.open_file(dir, copied) {
            placeholder::strip_konedrive_xattrs(&file)?;
        }
        let copy_rel = rel.with_file_name(&copy);
        if is_dir {
            let rebase = [OutboxOp::Rebase { from: rel.to_path_buf(), to: copy_rel.clone() }];
            self.store.call_blocking(move |s| s.outbox_apply(&rebase, 0))?;
        }
        tracing::info!("{} changed here and in OneDrive: the local version is kept as {}", rel.display(), copy_rel.display());
        run.out.on_disk.copies.push(Copied { original: rel.to_path_buf(), copy: copy_rel.clone() });
        run.out.on_disk.examine.push((copy_rel, is_dir));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
