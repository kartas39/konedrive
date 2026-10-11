//! The failed tries the plan's task keeps for the life of its run: how many tries of a
//! step failed, when the next is due and why the last failed. They are of the steps the
//! store keeps no try of: the steps towards the folder, the steps of the rows alone, and
//! a step towards OneDrive planned from a row that is not unsent (a move owed to OneDrive
//! after a killed exchange of names; a content that is this folder's own upload, sent
//! again after the user put the agreed content back) — and, for any step towards OneDrive,
//! a try the store could not be written ([`LocalStep::describe`]). They are what makes
//! such a step wait, and the plan is told of them (`SyncPlanInput::next_tries`) together
//! with what the store keeps of the tries of what is sent to OneDrive.
//!
//! **State owned:** the tries, by the item and the kind of the step ([`LocalStepKind`]),
//! each with the versions of the rows the step was planned from. Changed by the plan's
//! task alone: at the settlement of a step that ended, and when a plan is made.
//!
//! **Why not in the store.** A remembered fact names what voids it, and the voider must
//! happen for every way the fact can stop being true. The tries of a step towards OneDrive
//! are of an unsent change, which the row's stamp tells from the next. A step towards the
//! folder is planned for what OneDrive has, and nothing a row keeps tells its work from
//! the next: the eTag is the same after the work ended (a content landed by a read, a pin
//! taken off) and another one while the work is the same (another client renamed the
//! item), and an item OneDrive removed has none. A step towards OneDrive planned from a
//! row that is not unsent has no stamp either: the store answers its try as one of a
//! change that is gone (`FailedTryStanding::ChangeGone`). So the tries are kept with the
//! versions the step was planned from, by the one who plans, and are void when:
//!
//! - the step is written ([`LocalStepTries::forget_tries`]);
//! - a row the step was planned from is another piece of work — its binding, its local
//!   version, its OneDrive version, what was agreed on, or whether its content is on disk
//!   is not what the step was planned from — or went
//!   ([`LocalStepTries::void_by_rows`], asked with every plan, of the rows the plan's
//!   reading is of);
//! - the daemon's run of the folder ends: nothing is written anywhere.
//!
//! An eTag alone that changed voids nothing: it tells no piece of work from another here.
//! A try whose step is not planned any more while its rows are as they were (a pin taken
//! off) stays, and makes nothing wait: the plan answers a wait only in place of a step.
//!
//! **After a restart** a step whose tries are kept here is tried again at once, with a
//! count that begins at one. For a step towards the folder or of the rows that is right:
//! its work is local — a rename, a removal, a download into the backing directory, a row
//! —, a try sends no request that changes OneDrive, and the failures that last (a place
//! held on disk, a backing file that is missing) are met again at the first try and wait
//! from then on. A step towards OneDrive from a row that is not unsent sends one request
//! more after a restart than its wait would have allowed, and waits from then on.
//!
//! **Tested without the rest**, a test per voider:
// test: a_failed_try_is_counted_and_told_to_the_plan_with_its_next_try
// test: the_tries_of_a_step_end_when_it_is_written
// test: the_tries_end_when_the_local_version_the_step_works_from_changes
// test: the_tries_end_when_the_onedrive_version_the_step_works_from_changes
// test: the_tries_end_when_the_content_comes_to_be_on_disk_by_another_way
// test: the_tries_end_when_what_the_two_sides_agreed_on_changes
// test: the_tries_end_when_the_items_row_goes
// test: the_tries_of_a_merge_end_when_either_row_changes
// test: an_etag_alone_that_changed_ends_no_tries
// test: the_step_for_an_item_onedrive_removed_waits_as_any_other
// test: a_try_of_a_step_planned_from_other_versions_is_counted_from_one
// test: the_tries_of_one_kind_of_step_are_not_those_of_another
// test: no_try_of_a_step_towards_onedrive_from_an_unsent_row_is_kept
// test: the_tries_of_a_step_towards_onedrive_from_a_row_that_is_not_unsent_are_kept_and_told_as_a_sending
// test: a_refresh_makes_every_try_due_and_keeps_its_count

use std::collections::{BTreeMap, BTreeSet};

use konedrive_tree::{FolderStepKind, ItemVersions, LocalItemId, NextTry, Reason, RowStepKind, SyncStep, TriedStep};

/// Which step a step is, as its tries are kept here: what the tries of one step of an item
/// are told from those of another by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LocalStepKind {
    TowardsFolder(FolderStepKind),
    OfRows(RowStepKind),
    /// Whatever is sent of the item: a request that failed says that the next one waits,
    /// as the store's tries of an unsent change do (`TriedStep::TowardsOneDrive`).
    TowardsOneDrive,
}

impl LocalStepKind {
    /// The steps of an item whose next try the plan holds for a try of this kind.
    pub fn tried_step(self) -> TriedStep {
        match self {
            LocalStepKind::TowardsFolder(kind) => TriedStep::TowardsFolder(kind),
            LocalStepKind::OfRows(kind) => TriedStep::OfRows(kind),
            LocalStepKind::TowardsOneDrive => TriedStep::TowardsOneDrive,
        }
    }
}

/// A step as its tries are kept here: the item it is of, its kind, and the versions of
/// every row it was planned from — its item's, and for two items that are merged those of
/// both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalStep {
    item: LocalItemId,
    kind: LocalStepKind,
    planned_from: Vec<ItemVersions>,
}

impl LocalStep {
    /// `step` as its tries are kept here; `None` for a step towards OneDrive planned from
    /// an unsent row, whose tries the store keeps with the unsent change it sends
    /// (`StepMemory::remember_failed_try`). A step towards OneDrive planned from a row
    /// that is not unsent has no stamp for the store to keep a try under: its tries are
    /// the task's.
    pub fn of(step: &SyncStep) -> Option<Self> {
        match step {
            SyncStep::OneDrive(sent) if sent.item().unsent_since.is_some() => None,
            SyncStep::OneDrive(_) | SyncStep::Folder(_) | SyncStep::Row(_) => Some(Self::describe(step)),
        }
    }

    /// `step` as a try of it is kept here, whichever step it is: what a try the store
    /// could not be written is kept as, for a step towards OneDrive planned from an unsent
    /// row too.
    pub fn describe(step: &SyncStep) -> Self {
        let (kind, planned_from) = match step {
            SyncStep::OneDrive(sent) => (LocalStepKind::TowardsOneDrive, vec![sent.item().clone()]),
            SyncStep::Folder(step) => (LocalStepKind::TowardsFolder(step.kind()), vec![step.item().clone()]),
            SyncStep::Row(konedrive_tree::RowStep::MergeItems { local, onedrive }) => (LocalStepKind::OfRows(RowStepKind::MergeItems), vec![local.clone(), onedrive.clone()]),
            SyncStep::Row(step) => (LocalStepKind::OfRows(step.kind()), vec![step.item().clone()]),
        };
        Self { item: step.local_item_id(), kind, planned_from }
    }

    pub fn item(&self) -> LocalItemId {
        self.item
    }

    pub fn kind(&self) -> LocalStepKind {
        self.kind
    }
}

/// The failed tries of one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalStepTry {
    /// How many tries failed.
    pub count: u32,
    /// Unix seconds; `None` when the next try is due at once.
    pub next_try_at: Option<i64>,
    pub last_failure: Reason,
    /// The versions of the rows the step that failed was planned from.
    planned_from: Vec<ItemVersions>,
}

impl LocalStepTry {
    /// Whether a step planned from `planned_from` is the work these tries are of: the same
    /// rows, each the same piece of work ([`is_same_work`]).
    fn is_of_same_work(&self, planned_from: &[ItemVersions]) -> bool {
        self.planned_from.len() == planned_from.len() && self.planned_from.iter().zip(planned_from).all(|(kept, given)| is_same_work(kept, given))
    }
}

/// The failed tries the plan's task keeps, for one folder and one run of the daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalStepTries {
    tries: BTreeMap<(LocalItemId, LocalStepKind), LocalStepTry>,
}

/// Whether `now`, the versions of a row as a plan's reading has them, are the piece of work
/// a step planned from `planned_from` was for: the same binding, local version, OneDrive
/// version, place and content agreed on, and the content on disk or not as it was. The
/// eTag is not asked, nor the stamp of what is unsent: neither tells one piece of such
/// work from another. Pure.
pub fn is_same_work(planned_from: &ItemVersions, now: &ItemVersions) -> bool {
    let ItemVersions { local_item_id, onedrive_item, local, onedrive, onedrive_etag: _, synced_place, synced_content, content_presence, unsent_since: _ } = planned_from;
    *local_item_id == now.local_item_id
        && *onedrive_item == now.onedrive_item
        && *local == now.local
        && *onedrive == now.onedrive
        && *synced_place == now.synced_place
        && *synced_content == now.synced_content
        && *content_presence == now.content_presence
}

impl LocalStepTries {
    /// How many tries of `step` failed so far: of this kind of step, for this item, planned
    /// from the same work. What the settlement computes the next wait from.
    pub fn count_tries(&self, step: &LocalStep) -> u32 {
        self.tries.get(&(step.item, step.kind)).filter(|kept| kept.is_of_same_work(&step.planned_from)).map_or(0, |kept| kept.count)
    }

    /// The tries kept of the step of `item` of this `kind`, when any are.
    pub fn find_tries(&self, item: LocalItemId, kind: LocalStepKind) -> Option<&LocalStepTry> {
        self.tries.get(&(item, kind))
    }

    /// A try of `step` failed for `reason`; the next is due at `next_try_at`, or at once.
    /// How many tries of it failed, with this one: counted on from the tries kept of the
    /// same kind of step planned from the same work, and from one otherwise.
    pub fn remember_failed_try(&mut self, step: &LocalStep, reason: Reason, next_try_at: Option<i64>) -> u32 {
        let count = self.count_tries(step).saturating_add(1);
        self.tries.insert((step.item, step.kind), LocalStepTry { count, next_try_at, last_failure: reason, planned_from: step.planned_from.clone() });
        count
    }

    /// `step` was written: its tries are over.
    pub fn forget_tries(&mut self, step: &LocalStep) {
        self.tries.remove(&(step.item, step.kind));
    }

    /// The items a row of which a kept try was planned from: what a plan's reading reads
    /// the versions of, to ask [`void_by_rows`](Self::void_by_rows).
    pub fn list_items(&self) -> BTreeSet<LocalItemId> {
        self.tries.values().flat_map(|kept| kept.planned_from.iter().map(|row| row.local_item_id)).collect()
    }

    /// `rows` are the versions of the rows of [`list_items`](Self::list_items) as a plan's
    /// reading has them, an item with no row being left out. Every try one of whose rows
    /// went, or is another piece of work than the step was planned from
    /// ([`is_same_work`]), is forgotten.
    pub fn void_by_rows(&mut self, rows: &BTreeMap<LocalItemId, ItemVersions>) {
        self.tries.retain(|_, kept| kept.planned_from.iter().all(|planned_from| rows.get(&planned_from.local_item_id).is_some_and(|now| is_same_work(planned_from, now))));
    }

    /// What the plan is told: for every try with a time, that time and the reason of the
    /// last failure, under the item and the kind of step it holds. Every one is given, due
    /// or not: the plan compares with its `now`.
    pub fn list_next_tries(&self) -> BTreeMap<(LocalItemId, TriedStep), NextTry> {
        let next_try = |((item, kind), kept): (&(LocalItemId, LocalStepKind), &LocalStepTry)| Some(((*item, kind.tried_step()), NextTry { not_before: kept.next_try_at?, reason: kept.last_failure.clone() }));
        self.tries.iter().filter_map(next_try).collect()
    }

    /// Every try is due now, with its count and its reason kept: the user asked for a
    /// refresh, or the network is back. As `StepMemory::clear_next_tries` for what is
    /// sent. How many waited.
    pub fn clear_next_tries(&mut self) -> usize {
        self.tries.values_mut().filter_map(|kept| kept.next_try_at.take()).count()
    }
}

#[cfg(test)]
mod tests;
