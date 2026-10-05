//! What the mode answers: the questions a pass asks where a read-only and
//! a read-write folder differ. The passes themselves ([`Materializer::full`],
//! [`Materializer::changed`], [`Materializer::place`]) are one for both.
//!
//! A read-only folder shows OneDrive: whatever is not where the tree has it
//! goes to the holding directory, a local version in the way is rescued out
//! of the folder, and nothing waits. A read-write folder also holds the
//! user's own changes ([`Rw`](crate::remote::materialize::Rw)): what a local change holds is left
//! as it is and its change waits, a local version in the way is kept beside
//! the cloud's, and nothing leaves the folder.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

use konedrive_tree::outbox::OutboxOp;
use konedrive_tree::{Located, Plan, Planned, Row};

use super::removal::Policy;
use super::rw::Scan;
use super::{holds_local_work, holds_local_work_rw, is_leftover_replacement, is_misplaced, ApplyError, Materializer, Run, Seen, Sorted};
use crate::folder::disk::{Probe, Scanned};
use crate::remote::mode::Mode;

impl Materializer {
    /// Whether deleting or replacing `file` would lose something only this
    /// machine has ([`holds_local_work`]); in read-write mode an emptied
    /// download counts too.
    pub(super) fn local_work(&self, file: &File) -> bool {
        match &self.mode {
            Mode::ReadOnly => holds_local_work(file),
            Mode::ReadWrite(_) => holds_local_work_rw(file),
        }
    }

    /// The plan of the entries of a Full scan that are misplaced by `rows`:
    /// what a read-write folder tells a move in OneDrive from a local one
    /// by. A read-only folder asks nothing: everything misplaced is moved.
    pub(super) fn plan_of_misplaced(&self, entries: &[Scanned], rows: &HashMap<String, Row>) -> Result<Plan, ApplyError> {
        match &self.mode {
            Mode::ReadOnly => Ok(Plan::default()),
            Mode::ReadWrite(_) => self.plan_misplaced(entries, rows),
        }
    }

    /// The Full scope, phase 1: what is done with an object of ours the
    /// scan found.
    pub(super) fn sort_scanned(&self, seen: &Seen, scan: &mut Scan, run: &mut Run) -> Result<Sorted, ApplyError> {
        match &self.mode {
            Mode::ReadOnly if is_leftover_replacement(seen.entry, seen.id, seen.counts) => Ok(Sorted::Leftover),
            Mode::ReadOnly if is_misplaced(seen.entry, seen.new) => Ok(Sorted::ToHolding),
            Mode::ReadOnly => Ok(Sorted::Stays),
            Mode::ReadWrite(rw) => self.sort_scanned_rw(rw, seen, scan, run),
        }
    }

    /// The Changed scope, phase 1: what is done with item `id`, which the
    /// base has at `old`. A read-only folder holds every item where the
    /// base has it, or does not match the stored tree; in a read-write one
    /// a local change explains a disagreement, and the item's change waits.
    pub(super) fn sort_changed(&self, id: &str, old: &Located, planned: &Planned, unplaced: &[&Located], run: &mut Run) -> Result<Sorted, ApplyError> {
        if let Mode::ReadWrite(rw) = &self.mode {
            return self.sort_changed_rw(rw, id, old, planned, unplaced, run);
        }
        let parent = old.rel.parent().unwrap_or(Path::new(""));
        let name = old.rel.file_name().ok_or_else(|| ApplyError::NeedFull(format!("{id} has no name")))?;
        let dir = self.disk.dir(parent).map_err(|e| ApplyError::NeedFull(format!("{}: {e}", parent.display())))?;
        match self.disk.probe(&dir, name)? {
            Probe::Managed { id: found, .. } if found == id => {}
            other => return Err(ApplyError::NeedFull(format!("{} should be {id} and is {other:?}", old.rel.display()))),
        }
        Ok(if planned.stays() { Sorted::Stays } else { Sorted::ToHolding })
    }

    /// The items of the Changed scope the base has at `here` that OneDrive
    /// still has and the new tree no longer places: what a read-write
    /// folder lets go or wait whole. None in a read-only one,
    /// where such an item goes like one removed.
    pub(super) fn no_longer_placed<'a>(&self, plan: &Plan, here: &[(&String, &'a Located)]) -> Vec<&'a Located> {
        match &self.mode {
            Mode::ReadOnly => Vec::new(),
            Mode::ReadWrite(_) => here.iter().filter(|(id, _)| super::rw::no_longer_placed(plan.of(id))).map(|(_, at)| *at).collect(),
        }
    }

    /// Phase 1: what OneDrive removed is taken off the disk where it stands,
    /// at `rel` ([`Materializer::take_off`]); whether its removal waits (for
    /// another filesystem mounted inside it to go). A read-write folder's
    /// way: a read-only one takes it to the holding directory, and the
    /// drain rescues what it holds.
    pub(super) fn remove_in_place(&self, rel: &Path, run: &mut Run) -> Result<bool, ApplyError> {
        let policy = self.mode.read_write().map_or(Policy::Removed, |rw| rw.removed());
        let parent = rel.parent().unwrap_or(Path::new(""));
        let name = rel.file_name().ok_or_else(|| ApplyError::Io(format!("{} has no name", rel.display())))?;
        Ok(self.take_off(&self.disk.dir(parent)?, name, rel, policy, run)?.waits.is_some())
    }

    /// Phase 2: whether nothing is placed for item `id`, and its change
    /// goes to the base all the same: a live `delete` or `move-out` row
    /// removes it, or a folder above it.
    pub(super) fn passes_over(&self, id: &str) -> bool {
        self.mode.read_write().is_some_and(|rw| rw.removing.contains(id))
    }

    /// Phase 2: whether item `id` is left as it is, with everything below
    /// it, and its change waits: a local change holds it, or phase 1 left
    /// it. A Full scope knows what it left by its scan; a Changed one by
    /// what it found unsettled.
    pub(super) fn left_alone(&self, id: &str, run: &Run) -> bool {
        let Some(rw) = self.mode.read_write() else { return false };
        let left = if run.scope.is_some() { &run.out.pending.unsettled } else { &run.left };
        rw.held.contains(id) || left.contains(id)
    }

    /// The Changed scope, phase 2: whether an item whose folder is not
    /// where the tree has it asks for the scan of a Full pass. Always in a
    /// read-only folder. In a read-write one only with something to place:
    /// only a scan can tell a folder deleted here from one moved (§3.7);
    /// with nothing to place, the change waits.
    pub(super) fn asks_for_the_scan(&self, id: &str) -> bool {
        self.mode.read_write().is_none_or(|rw| rw.revive.contains(id))
    }

    /// Whether item `id`, found where it belongs, has no local object on
    /// record (a rebuilt base, a forgotten one): the one found is recorded.
    pub(super) fn has_no_record(&self, id: &str) -> bool {
        self.mode.read_write().is_some_and(|rw| rw.unplaced.contains(id))
    }

    /// A local version at `dir/name` (at `rel`) is in the way of the
    /// cloud's: rescued out of the folder, or, in a read-write folder, kept
    /// beside it under a copy name, to be uploaded as new (§6).
    pub(super) fn out_of_the_way(&self, dir: &File, name: &OsStr, rel: &Path, run: &mut Run) -> Result<(), ApplyError> {
        match &self.mode {
            Mode::ReadOnly => self.rescue(dir, name, rel, run),
            Mode::ReadWrite(rw) => self.copy_aside(rw, dir, name, rel, run),
        }
    }

    /// Whether another item of ours, `other`, standing at `rel` where an
    /// item is to be placed, keeps the name: a local change holds it, or
    /// its own move waits in this run. What was to be placed waits behind
    /// it. Never in a read-only folder.
    pub(super) fn keeps_its_name(&self, other: &str, rel: &Path, run: &Run) -> Result<bool, ApplyError> {
        match &self.mode {
            Mode::ReadOnly => Ok(false),
            Mode::ReadWrite(rw) => Ok(self.holds_the_name(rw, other, rel, run)? || run.out.pending.unsettled.contains(other)),
        }
    }

    /// Whether an object with no id at `rel`, where `row` is to be placed,
    /// is left there and the item waits: a create or mkdir waiting there,
    /// which the outbox worker settles with the cloud's item (§6,
    /// create/create); an object of the user's the cloud has nothing new
    /// for; a local folder where OneDrive has a new one (`both_folders`),
    /// which merge by the `mkdir`'s `409`, never a copy. Never in a
    /// read-only folder.
    pub(super) fn is_the_users(&self, row: &Row, rel: &Path, both_folders: bool) -> bool {
        self.mode.read_write().is_some_and(|rw| rw.pending_at(rel) || !rw.brings(&row.id) || both_folders)
    }

    /// Whether `row`, missing from its place, is made there. Always in a
    /// read-only folder; see [`Self::place_again`] for a read-write one.
    pub(super) fn places_missing(&self, row: &Row, rel: &Path, run: &mut Run) -> Result<bool, ApplyError> {
        match &self.mode {
            Mode::ReadOnly => Ok(true),
            Mode::ReadWrite(rw) => self.place_again(rw, row, rel, run),
        }
    }

    /// Whether a downloaded file of another version than `row`'s is left as
    /// it is, its change waiting: a read-write folder's, with an outbox row
    /// recorded since the cycle began. The worker's guard settles it (§3.7,
    /// excluded).
    pub(super) fn changed_since_the_cycle_began(&self, row: &Row, run: &mut Run) -> Result<bool, ApplyError> {
        if self.mode.is_read_only() {
            return Ok(false);
        }
        let id = row.id.clone();
        if self.store.call_blocking(move |s| s.outbox_for_item(&id))?.is_empty() {
            return Ok(false);
        }
        run.out.pending.unsettled.insert(row.id.clone());
        Ok(true)
    }

    /// Whether a downloaded file that differs from OneDrive's version is
    /// kept beside it even with no local work in it: a version OneDrive may
    /// have lost (`resyncChangesUploadDifferences`, §3.7).
    pub(super) fn keeps_every_download(&self) -> bool {
        self.mode.read_write().is_some_and(|rw| rw.upload_differences)
    }

    /// The content of item `id` is still to land on disk. In a read-write
    /// folder the base keeps the version the file holds until it has (the
    /// read-write reconcile must, item 4); its place is the one the disk
    /// took.
    pub(super) fn content_waits(&self, id: &str, run: &mut Run) {
        if !self.mode.is_read_only() {
            run.out.pending.content_waits.insert(id.to_owned());
        }
    }

    /// A folder was moved from `from` to `to` by this pass. In a read-write
    /// folder the outbox rows of what waits below it follow it.
    pub(super) fn rows_follow(&self, from: PathBuf, to: PathBuf) -> Result<(), ApplyError> {
        if self.mode.is_read_only() {
            return Ok(());
        }
        let rebase = [OutboxOp::Rebase { from, to }];
        self.store.call_blocking(move |s| s.outbox_apply(&rebase, 0))?;
        Ok(())
    }

    /// What is left in the holding directory once everything is placed:
    /// removed, with local work rescued, or, in a read-write folder, put
    /// back into the folder unless OneDrive removed it.
    pub(super) fn drain_holding(&self, run: &mut Run) -> Result<(), ApplyError> {
        match &self.mode {
            Mode::ReadOnly => self.drain_rescuing(run),
            Mode::ReadWrite(rw) => self.drain_putting_back(rw, run),
        }
    }
}
