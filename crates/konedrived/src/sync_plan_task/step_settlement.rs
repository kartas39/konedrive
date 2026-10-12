//! What is written and asked for after a step, by what came of it: the one place that
//! counts tries, keeps a refusal and asks the listing for a cycle.
//!
//! **State owned:** none. `decide_step_settlement` is a pure function of the step and
//! its outcome; `write_step_settlements` writes the answers of the steps that ended
//! together in one job of the store.
//!
//! **Answers for:** design §7: "a step that OneDrive refuses decides nothing" — it is
//! remembered and waits; a taken name and a changed item ask for a cycle. The table:
//!
//! | Outcome | Step memory | Else |
//! |---|---|---|
//! | `Written`, towards OneDrive | — (its commit forgot the tries, the session it was told ended, the temporary name) | the events; a throttle's chosen wait reset |
//! | `Written`, towards the folder or of the rows | — | the task's tries of the step forgotten; the events |
//! | `RowsChanged(Nothing)` | — | — (the next plan decides) |
//! | `RowsChanged(CycleWanted \| MadeItemLeft)` | — | a cycle asked for |
//! | `RowsChanged(MadeItemRemoved)` | — | — |
//! | `Refused(NameTaken)` | `remember_taken_name` | a cycle asked for |
//! | `Refused(NameHeldByUploadSession)` | `remember_failed_try(NameHeld, next)` | — |
//! | `Refused(ItemChanged \| ItemGone \| DirectoryGone)` | `remember_failed_try(reason, next)` | a cycle asked for |
//! | `Refused(ContentMismatch)` | `remember_failed_try(Hash, next)` | a cycle asked for |
//! | `Refused(NoSpace { free_space: Some })`, of content | `remember_space_refusal` | — |
//! | `Refused(NoSpace { free_space: None })`, of content | — | OneDrive is full (`OneDriveSpace` said so already) |
//! | `Refused(NoSpace)`, of a step that sends no content | `remember_failed_try(Quota, next)` | — (no refusal for lack of space is kept: it is a content's) |
//! | `Refused(Throttled)` | — (no try counted) | all sending held until the time |
//! | `Refused(SignedOut)` | — (no try counted) | all sending held until the sign-in |
//! | `Refused(Forbidden \| RefusedForGood)` | `remember_refusal`, with `RefusedVersion::Shown` of the local version sent, or `Removal` for a removal | the event, once per reason |
//! | `Refused(Locked \| UploadSessionGone \| Transient)` | `remember_failed_try(reason, next)` | — |
//! | `NotRun(OpenForWriting \| SendingStopped)` | — | — (the close, or the end of the hold, wakes the plan) |
//! | `NotRun(ContentChanged \| BackingFileMissing \| PlaceHeldOnDisk)` | `remember_failed_try(reason, next)` | the journal: none of them ends by itself |
//! | `Failed(UndoFailed)` | — | the mount's take-down was asked for by the runner |
//! | `Failed(Store)` | `remember_failed_try(Store, next)` | the journal |
//! | `Failed(Disk)` | `remember_failed_try(LocalIo, next)` | the journal |
//! | `Failed(AnswerUnreadable)` | `remember_failed_try(Failed, next)` | the journal |
//! | `Failed(RunnerPanicked)` | `remember_failed_try(Failed, next)` | the journal; towards OneDrive a cycle asked for |
//!
//! **Whose the failed tries are.** `remember_failed_try` in the table is two things, by
//! whether the step has a stamp of an unsent change (`LocalStep::of`). A step towards
//! OneDrive planned from an unsent row: the store's (`StepMemory::remember_failed_try`,
//! `MemoryWrite::FailedTry`), kept under the stamp of the unsent change the step sends.
//! Every other step — towards the folder, of the rows, and towards OneDrive from a row
//! that is not unsent —: the task's own (`LocalStepTriesChange::FailedTry`, applied to
//! its `LocalStepTries`), kept with the
//! versions the step was planned from, for the daemon's run; the store keeps nothing of
//! such a try. Either way the plan is told the next try
//! (`plan_input::describe_next_tries`) and answers the wait.
//!
//! **A step towards OneDrive from a row that is not unsent** has no stamp for the store to
//! keep any fact under: what the table writes into the step memory for a refusal — a
//! taken name, a refusal for good, a refusal for lack of space — would not be kept
//! (`FactStanding::VersionGone`) and the step would be sent again at once. For such a
//! step each of those is a failed try of the task's, with the refusal's reason; what the
//! table says beside the step memory (a cycle asked for) stands.
//!
//! **A write of the store that failed** (`write_step_settlements` answers the items, or
//! its job failed as a whole) leaves a step towards OneDrive with nothing kept: the task
//! counts a try of its own for it (`keep_unwritten_tries`), so that it waits.
//!
//! **Tested without the rest**, a test per line of the table:
// test: each_outcome_is_settled_as_its_line_of_the_table_says
// test: the_next_try_is_a_second_after_the_first_failure_and_doubles_to_an_hour
// test: a_refusal_for_good_keeps_the_local_version_that_was_sent_and_the_removal_for_a_removal
// test: a_taken_name_keeps_the_place_sent_and_the_holders_name
// test: a_step_that_did_not_run_for_a_cause_that_does_not_end_by_itself_is_not_planned_again_at_once
// test: the_settlements_of_several_steps_are_written_in_one_job
// test: a_failed_try_of_a_step_towards_onedrive_is_the_stores_and_any_other_the_tasks
// test: a_step_that_is_written_forgets_the_tasks_tries_of_it
// test: a_failed_try_of_a_step_towards_onedrive_from_a_row_that_is_not_unsent_is_the_tasks
// test: what_onedrive_refuses_of_a_row_that_is_not_unsent_is_a_failed_try_of_the_tasks
// test: a_settlement_the_store_could_not_be_written_is_a_try_the_task_keeps
// test: the_count_of_tries_is_the_stores_of_the_stamp_the_step_was_planned_from
// test: a_written_step_is_said_in_the_journal_without_the_path_of_its_event
// test: the_first_filling_logs_no_item_shown_and_every_other_event

use std::collections::{BTreeMap, BTreeSet};

use konedrive_tree::{ActivityKind, ActivityRow, FactStanding, FailedTryStanding, ItemVersions, LocalItemId, OneDriveStep, Reason, Refusal, RefusedVersion, SpaceRefusal, StepFact, SyncStep, TakenName, TreeError, TreeStore};

use super::local_step_tries::{LocalStep, LocalStepTries};
use crate::sync_parts::step_outcome::{RowsChangedFollowUp, StepEvent, StepFailure, StepObstacle, StepOutcome, StepRefusal};
use crate::sync_parts::sending_holds::SendingHold;
use crate::sync_parts::sending_holds::{BACKOFF_FIRST, BACKOFF_MAX};

/// One write of the step memory, as its operations are named. A fact of a local change
/// that is not sent — a failed try, a refusal, a refusal for lack of space, a taken name —
/// is written with the stamp of the versions the step was planned from
/// (`ItemVersions::unsent_since`, kept in `StepSettlement::unsent_since`), and the
/// store keeps it only for a row that still has that stamp. All of them are of a step
/// towards OneDrive: nothing is written to the store for a step towards the folder or of
/// the rows that failed ([`LocalStepTriesChange`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemoryWrite {
    /// `StepMemory::remember_failed_try(item, unsent_since, reason, next_try_at)`, of a
    /// step towards OneDrive. `FailedTryStanding::ChangeGone`: the row is another unsent
    /// change by now, nothing was counted, and the next plan decides.
    FailedTry { reason: Reason, next_try_at: Option<i64> },
    /// `StepMemory::remember_refusal(item, unsent_since, refusal)`; a refusal of a removal is
    /// `RefusedVersion::Removal`, which holds no stamp of its own.
    Refusal(Refusal),
    /// `StepMemory::remember_space_refusal(item, unsent_since, space_refusal)`.
    SpaceRefusal(SpaceRefusal),
    /// `StepMemory::remember_taken_name(item, unsent_since, taken_name)`.
    TakenName(TakenName),
    /// `StepMemory::forget_fact`.
    // TODO: no step answers this yet. Check with issue #238 whether the step that ends an exchange of names is to forget its temporary name (`TemporaryName`: "or its step forgets it"), and fix if needed.
    #[allow(dead_code)]
    Forget(StepFact),
}

/// What a step's end says of the tries the task keeps (`LocalStepTries`). A failed try
/// only for a step whose tries are the task's (`LocalStep::of`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalStepTriesChange {
    Nothing,
    /// `LocalStepTries::remember_failed_try(step, reason, next_try_at)`.
    FailedTry { reason: Reason, next_try_at: Option<i64> },
    /// The step was written: `LocalStepTries::forget_tries(step)`.
    Forgotten,
}

/// Whether the listing is asked for a cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CycleRequest {
    None,
    /// The changes since the delta link (`ListingPollHandle::refresh`).
    Changes,
}

/// What follows a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StepSettlement {
    pub(crate) item: LocalItemId,
    /// The stamp of the unsent change a step towards OneDrive was planned for
    /// (`ItemVersions::unsent_since`): what the store keeps each write of `memory` under.
    /// None for a step towards the folder or of the rows, which writes none.
    pub(crate) unsent_since: Option<u64>,
    pub(crate) memory: Vec<MemoryWrite>,
    /// What the task's own tries of the step become.
    pub(crate) local_step_tries: LocalStepTriesChange,
    pub(crate) cycle: CycleRequest,
    /// What holds all sending from now on, when the outcome says so. For a throttle
    /// OneDrive named no time for, `until` is `now`: the end is chosen by `SendingHolds`,
    /// which the task gives the outcome's own `retry_after`.
    pub(crate) hold: Option<SendingHold>,
    /// Whether a step towards OneDrive went through (`SendingHolds::pass`).
    pub(crate) went_through: bool,
    pub(crate) events: Vec<StepEvent>,
}

/// Whether an event of this kind says what OneDrive changed: what the cap per listing is
/// of.
fn is_onedrive_change(kind: ActivityKind) -> bool {
    matches!(kind, ActivityKind::Added | ActivityKind::Updated | ActivityKind::Removed | ActivityKind::Moved)
}

/// The versions of the item `step` is of.
fn find_item_versions(step: &SyncStep) -> &ItemVersions {
    match step {
        SyncStep::OneDrive(step) => step.item(),
        SyncStep::Folder(step) => step.item(),
        SyncStep::Row(step) => step.item(),
    }
}

/// What OneDrive refused for good of `step`: the removal for a removal, the local version
/// that was sent for any other.
fn find_refused_version(step: &SyncStep) -> RefusedVersion {
    match (step, find_item_versions(step).local.clone()) {
        (SyncStep::OneDrive(OneDriveStep::SendFileRemoval { .. } | OneDriveStep::SendDirectoryRemoval { .. }), _) | (_, None) => RefusedVersion::Removal,
        (_, Some(local)) => RefusedVersion::Shown(local),
    }
}

/// The events of `events` that go to the activity log, when `logged_since_listing_ended`
/// (the plan's task's count) were logged for what OneDrive changed since the last
/// listing ended. While the folder is filled for the first time (`is_first_filling`,
/// `FirstFilling::is_under_way`) an item shown is no event: nothing was added in OneDrive.
/// Pure.
// OPEN: the map's MISSING-4.c: today a cycle logs one batch, capped, and "a folder
// removed with everything in it is one removal" (`remote/listing/reconcile.rs:376-391`).
// Written as: each step's events are logged as they come, up to today's cap per listing;
// a removed directory is one event because its children's `RemoveItem` steps answer
// none. Pure, and can change without touching a signature.
pub(crate) fn decide_events_to_log(events: &[StepEvent], logged_since_listing_ended: usize, is_first_filling: bool) -> Vec<StepEvent> {
    let mut left = crate::status::activity::PER_KIND.saturating_sub(logged_since_listing_ended);
    let mut logged = Vec::new();
    for event in events {
        if is_first_filling && event.kind == ActivityKind::Added {
            continue;
        }
        if is_onedrive_change(event.kind) {
            if left == 0 {
                continue;
            }
            left -= 1;
        }
        logged.push(event.clone());
    }
    logged
}

/// When the next try of a step that failed `count` times is due, Unix seconds: a second
/// after the first failure, doubling to an hour (`upload/engine.rs:108`, `backoff_after`).
/// Pure.
pub fn decide_next_try(count: u32, now: i64) -> i64 {
    let wait = BACKOFF_FIRST.as_secs().saturating_mul(1u64 << count.saturating_sub(1).min(20));
    now.saturating_add(wait.min(BACKOFF_MAX.as_secs()) as i64)
}

/// What follows `step` ended as `outcome`: the table of the module's doc. `tries` is how
/// many tries of the step failed before this one ([`count_failed_tries`]). Pure.
pub(crate) fn decide_step_settlement(step: &SyncStep, outcome: &StepOutcome, tries: u32, now: i64) -> StepSettlement {
    let is_towards_onedrive = matches!(step, SyncStep::OneDrive(_));
    // A step with no stamp of an unsent change: the store keeps no fact of it.
    let is_try_kept_by_task = LocalStep::of(step).is_some();
    let mut settlement = StepSettlement {
        item: step.local_item_id(),
        unsent_since: match step {
            SyncStep::OneDrive(step) => step.item().unsent_since,
            SyncStep::Folder(_) | SyncStep::Row(_) => None,
        },
        memory: Vec::new(),
        local_step_tries: LocalStepTriesChange::Nothing,
        cycle: CycleRequest::None,
        hold: None,
        went_through: false,
        events: Vec::new(),
    };
    let next_try_at = Some(decide_next_try(tries.saturating_add(1), now));
    let count_failed_try = |settlement: &mut StepSettlement, reason: Reason| {
        if is_try_kept_by_task {
            settlement.local_step_tries = LocalStepTriesChange::FailedTry { reason, next_try_at };
        } else {
            settlement.memory.push(MemoryWrite::FailedTry { reason, next_try_at });
        }
    };
    match outcome {
        StepOutcome::Written { events } => {
            settlement.events = events.clone();
            settlement.went_through = is_towards_onedrive;
            // Whatever the task kept of the step: its own tries, or one the store could not be written.
            settlement.local_step_tries = LocalStepTriesChange::Forgotten;
        }
        StepOutcome::RowsChanged(RowsChangedFollowUp::Nothing | RowsChangedFollowUp::MadeItemRemoved(_)) => {}
        StepOutcome::RowsChanged(RowsChangedFollowUp::CycleWanted | RowsChangedFollowUp::MadeItemLeft(_)) => settlement.cycle = CycleRequest::Changes,
        StepOutcome::Refused(refusal) => match refusal {
            StepRefusal::NameTaken { .. } if is_try_kept_by_task => {
                count_failed_try(&mut settlement, Reason::NameHeld);
                settlement.cycle = CycleRequest::Changes;
            }
            StepRefusal::NameTaken { place, holder_name } => {
                settlement.memory.push(MemoryWrite::TakenName(TakenName { place: place.clone(), holder_name: holder_name.clone() }));
                settlement.cycle = CycleRequest::Changes;
            }
            StepRefusal::NameHeldByUploadSession => count_failed_try(&mut settlement, Reason::NameHeld),
            StepRefusal::ItemChanged => {
                count_failed_try(&mut settlement, Reason::ChangedAgain);
                settlement.cycle = CycleRequest::Changes;
            }
            StepRefusal::ItemGone => {
                count_failed_try(&mut settlement, Reason::NotFound);
                settlement.cycle = CycleRequest::Changes;
            }
            StepRefusal::DirectoryGone => {
                count_failed_try(&mut settlement, Reason::Parent);
                settlement.cycle = CycleRequest::Changes;
            }
            StepRefusal::ContentMismatch => {
                count_failed_try(&mut settlement, Reason::Hash);
                settlement.cycle = CycleRequest::Changes;
            }
            // A step that sends no content waits and is tried again: a refusal for lack of space is a content's.
            StepRefusal::NoSpace { .. } if !matches!(step, SyncStep::OneDrive(OneDriveStep::SendNewFile { .. } | OneDriveStep::SendContent { .. })) => count_failed_try(&mut settlement, Reason::Quota),
            StepRefusal::NoSpace { free_space: Some(_) } if is_try_kept_by_task => count_failed_try(&mut settlement, Reason::Quota),
            StepRefusal::NoSpace { free_space: Some(free_space) } => {
                let content = find_item_versions(step).local.as_ref().and_then(|local| local.content.clone());
                settlement.memory.extend(content.map(|content| MemoryWrite::SpaceRefusal(SpaceRefusal { content, free_space: *free_space })));
            }
            StepRefusal::NoSpace { free_space: None } => {}
            StepRefusal::Throttled { retry_after } => {
                let wait = retry_after.map_or(0, |wait| i64::try_from(wait.as_secs()).unwrap_or(i64::MAX));
                settlement.hold = Some(SendingHold::Throttled { until: now.saturating_add(wait) });
            }
            StepRefusal::SignedOut => settlement.hold = Some(SendingHold::SignedOut),
            StepRefusal::Forbidden if is_try_kept_by_task => count_failed_try(&mut settlement, Reason::Forbidden),
            StepRefusal::RefusedForGood { message } if is_try_kept_by_task => count_failed_try(&mut settlement, Reason::Refused(Some(message.clone()))),
            StepRefusal::Forbidden => settlement.memory.push(MemoryWrite::Refusal(Refusal { reason: Reason::Forbidden, version: find_refused_version(step) })),
            StepRefusal::RefusedForGood { message } => settlement.memory.push(MemoryWrite::Refusal(Refusal { reason: Reason::Refused(Some(message.clone())), version: find_refused_version(step) })),
            StepRefusal::Locked => count_failed_try(&mut settlement, Reason::Locked),
            StepRefusal::UploadSessionGone => count_failed_try(&mut settlement, Reason::SessionEnded),
            StepRefusal::Transient { .. } => count_failed_try(&mut settlement, Reason::Network),
        },
        StepOutcome::NotRun(StepObstacle::OpenForWriting | StepObstacle::SendingStopped) => {}
        StepOutcome::NotRun(StepObstacle::ContentChanged) => count_failed_try(&mut settlement, Reason::Changed),
        StepOutcome::NotRun(StepObstacle::BackingFileMissing | StepObstacle::PlaceHeldOnDisk { .. }) => count_failed_try(&mut settlement, Reason::LocalIo),
        StepOutcome::Failed(StepFailure::UndoFailed { .. }) => {}
        StepOutcome::Failed(StepFailure::Store { .. }) => count_failed_try(&mut settlement, Reason::Store),
        StepOutcome::Failed(StepFailure::Disk { .. }) => count_failed_try(&mut settlement, Reason::LocalIo),
        StepOutcome::Failed(StepFailure::AnswerUnreadable { .. }) => count_failed_try(&mut settlement, Reason::Failed),
        StepOutcome::Failed(StepFailure::RunnerPanicked { .. }) => {
            count_failed_try(&mut settlement, Reason::Failed);
            // Whether the request of a step towards OneDrive left is not known.
            if is_towards_onedrive {
                settlement.cycle = CycleRequest::Changes;
            }
        }
    }
    settlement
}

/// What came of a step, as the journal says it at `debug`: the case, and for a step that
/// was not written why — with no event of a written step, which names its path. Pure.
pub(crate) fn describe_outcome(outcome: &StepOutcome) -> String {
    match outcome {
        StepOutcome::Written { events } => format!("written, {} event(s)", events.len()),
        StepOutcome::RowsChanged(follow_up) => format!("the rows changed ({follow_up:?})"),
        StepOutcome::Refused(refusal) => format!("refused ({refusal:?})"),
        StepOutcome::NotRun(obstacle) => format!("not run ({obstacle:?})"),
        StepOutcome::Failed(failure) => format!("failed ({failure:?})"),
    }
}

/// Whether what came of a step changed anything a plan is made from: the step was
/// written, its commit found the rows changed, OneDrive refused it (a try counted, a fact
/// kept, a hold set, OneDrive full), a try was counted, a cycle was asked for. A step
/// that did not run for a cause that ends by itself, and one whose undo failed, changed
/// nothing: the plan made after it would be the plan before it. What the plan's task
/// asks before it makes a plan for finished steps alone (`decide_round`). Pure.
pub(crate) fn has_changed_anything(outcome: &StepOutcome, settlement: &StepSettlement) -> bool {
    matches!(outcome, StepOutcome::Written { .. } | StepOutcome::RowsChanged(_) | StepOutcome::Refused(_))
        || !settlement.memory.is_empty()
        || settlement.local_step_tries != LocalStepTriesChange::Nothing
        || settlement.cycle != CycleRequest::None
        || settlement.hold.is_some()
}

/// How many tries of `step` failed so far: what its next wait is computed from. For a step
/// whose tries the task keeps (`LocalStep::of`), the task's count. For a step towards
/// OneDrive planned from an unsent row, what the store keeps under that stamp
/// (`store_counts`, read for the settlement by [`read_failed_try_counts`]) — or the task's
/// own count when that is more: tries the store could not be written
/// ([`keep_unwritten_tries`]). Pure.
pub(crate) fn count_failed_tries(step: &SyncStep, tries: &LocalStepTries, store_counts: &BTreeMap<LocalItemId, u32>) -> u32 {
    let kept_by_task = tries.count_tries(&LocalStep::describe(step));
    match LocalStep::of(step) {
        Some(_) => kept_by_task,
        None => store_counts.get(&step.local_item_id()).copied().unwrap_or(0).max(kept_by_task),
    }
}

/// For each of `steps` whose tries the store keeps (a step towards OneDrive planned from an
/// unsent row), how many tries the store keeps of the unsent change it was planned from:
/// read when the step is settled, so that the count is the store's own whatever the last
/// plan was given. An item with no try kept, or whose row has another stamp by now, is
/// left out.
pub(crate) fn read_failed_try_counts(items: &konedrive_tree::ItemReader<'_>, steps: &[(LocalItemId, Option<u64>)]) -> Result<BTreeMap<LocalItemId, u32>, TreeError> {
    let mut counts = BTreeMap::new();
    for (item, unsent_since) in steps {
        let memory = items.find_item_memory(*item)?;
        if let Some(failed_tries) = memory.failed_tries.filter(|_| memory.unsent_since == *unsent_since) {
            counts.insert(*item, failed_tries.count);
        }
    }
    Ok(counts)
}

/// The store could not be written what `settled` say of the items of `unwritten`: every
/// step towards OneDrive among them that had something to keep there counts a try in the
/// task's own tries, with the reason of its failed try, or `Reason::Store` for a fact of
/// another kind, and its next try at the time the count gives. So a step whose try was
/// not written waits as one whose try was. No store, no clock.
pub(crate) fn keep_unwritten_tries(tries: &mut LocalStepTries, settled: &[(SyncStep, StepSettlement)], unwritten: &BTreeSet<LocalItemId>, now: i64) {
    for (step, settlement) in settled {
        if !matches!(step, SyncStep::OneDrive(_)) || settlement.memory.is_empty() || !unwritten.contains(&settlement.item) {
            continue;
        }
        let reason = settlement.memory.iter().find_map(|write| if let MemoryWrite::FailedTry { reason, .. } = write { Some(reason.clone()) } else { None }).unwrap_or(Reason::Store);
        let kept_as = LocalStep::describe(step);
        let next_try_at = decide_next_try(tries.count_tries(&kept_as).saturating_add(1), now);
        tries.remember_failed_try(&kept_as, reason, Some(next_try_at));
    }
}

/// Applies what the settlements of `ended` say of the task's own tries, each to the step
/// it is of: a failed try counted, the tries of a written step forgotten — also one kept
/// for a step towards OneDrive whose try the store could not be written
/// (`LocalStep::describe`). No store, no clock. Called by `SyncPlanTask::settle_steps`,
/// under the lock of the task's tries.
pub(crate) fn change_local_step_tries(tries: &mut LocalStepTries, ended: &[(SyncStep, StepSettlement)]) {
    for (step, settlement) in ended {
        let local_step = LocalStep::describe(step);
        match &settlement.local_step_tries {
            LocalStepTriesChange::Nothing => {}
            LocalStepTriesChange::FailedTry { reason, next_try_at } => {
                tries.remember_failed_try(&local_step, reason.clone(), *next_try_at);
            }
            LocalStepTriesChange::Forgotten => tries.forget_tries(&local_step),
        }
    }
}

/// Writes what `settlements` say of the step memory, and their events to the activity
/// log (`TreeStore::add_activity`, with the folder's path before each, capped by
/// [`decide_events_to_log`]), in ONE job of the store's thread; how many events were
/// of what OneDrive changed were logged. `now` is the time the events are logged with. A
/// refusal for good is logged here, once per reason, with the item's path as the rows
/// say it now. A fact the row does not say any more is not written
/// (`FactStanding::VersionGone`): the item changed since, and the next plan decides. A
/// write the store refuses — an item whose row went — is said in the journal, answered
/// (`SettlementsWritten::unwritten_items`) and is that item's alone: the other writes and
/// every event are written all the same.
/// Called by `SyncPlanTask::settle_steps`.
pub(crate) fn write_step_settlements(store: &mut TreeStore, settlements: &[StepSettlement], folder: &std::path::Path, logged_since_listing_ended: usize, is_first_filling: bool, now: i64) -> Result<SettlementsWritten, TreeError> {
    let mut events = Vec::new();
    let mut logged_count = 0;
    let mut unwritten_items = BTreeSet::new();
    for settlement in settlements {
        for write in &settlement.memory {
            // A write the store refuses is that item's alone: an item whose row went has nothing to remember.
            match write_memory(store, settlement, write, folder, now) {
                Ok(event) => events.extend(event),
                Err(error) => {
                    tracing::warn!("cannot keep {write:?} of item {}: {error}", settlement.item);
                    unwritten_items.insert(settlement.item);
                }
            }
        }
        for event in decide_events_to_log(&settlement.events, logged_since_listing_ended + logged_count, is_first_filling) {
            if is_onedrive_change(event.kind) {
                logged_count += 1;
            }
            let detail = event.former_path.as_ref().map(|former_path| format!("from {}", folder.join(former_path).display())).unwrap_or_default();
            events.push(ActivityRow { at: now, kind: event.kind, path: folder.join(&event.path).to_string_lossy().into_owned(), detail });
        }
    }
    if !events.is_empty() {
        store.add_activity(&events)?;
    }
    Ok(SettlementsWritten { logged_count, rows: events, unwritten_items })
}

/// What one job of [`write_step_settlements`] wrote into the activity log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SettlementsWritten {
    /// How many events of what OneDrive changed were logged: what the cap per listing
    /// counts.
    pub(crate) logged_count: usize,
    /// Every row written, in the order written: what the task announces
    /// (`SyncPlanTask::announce_settled_steps`).
    pub(crate) rows: Vec<ActivityRow>,
    /// The items a write of the step memory failed for: what the task keeps a try of
    /// itself ([`keep_unwritten_tries`]).
    pub(crate) unwritten_items: BTreeSet<LocalItemId>,
}

/// Writes one `write` of `settlement` into the step memory, and answers the event a
/// refusal for good is logged with, once per reason: none for a refusal the item is kept
/// with already, and none for one the row does not say any more.
fn write_memory(store: &mut TreeStore, settlement: &StepSettlement, write: &MemoryWrite, folder: &std::path::Path, now: i64) -> Result<Option<ActivityRow>, TreeError> {
    let (item, unsent_since) = (settlement.item, settlement.unsent_since);
    match write {
        MemoryWrite::FailedTry { reason, next_try_at } => {
            // A try that was not counted is of another unsent change by now: the next plan decides.
            let _: FailedTryStanding = store.step_memory().remember_failed_try(item, unsent_since, reason, *next_try_at)?;
        }
        MemoryWrite::Refusal(refusal) => {
            let is_kept_already = store.step_memory().find_item_memory(item)?.refusal.is_some_and(|kept| kept.reason == refusal.reason);
            let standing = store.step_memory().remember_refusal(item, unsent_since, refusal)?;
            if standing == FactStanding::StillSaid && !is_kept_already {
                // An item removed here has no local place: its path is the one agreed on.
                // The refusal is kept by now: its event is logged also when the path cannot be read, with the folder's path and the item's id.
                let (path, detail) = match store.read_items(|items| Ok(items.find_path(item)?.or(items.find_synced_path(item)?))) {
                    Ok(Some(path)) => (folder.join(path), refusal.reason.to_string()),
                    Ok(None) => (folder.to_path_buf(), format!("{} (item {item})", refusal.reason)),
                    Err(error) => {
                        tracing::warn!("cannot read the path of item {item}, which OneDrive refuses: {error}");
                        (folder.to_path_buf(), format!("{} (item {item})", refusal.reason))
                    }
                };
                return Ok(Some(ActivityRow { at: now, kind: ActivityKind::UploadFailed, path: path.to_string_lossy().into_owned(), detail }));
            }
        }
        MemoryWrite::SpaceRefusal(space_refusal) => {
            let _: FactStanding = store.step_memory().remember_space_refusal(item, unsent_since, space_refusal)?;
        }
        MemoryWrite::TakenName(taken_name) => {
            let _: FactStanding = store.step_memory().remember_taken_name(item, unsent_since, taken_name)?;
        }
        MemoryWrite::Forget(fact) => store.step_memory().forget_fact(item, *fact)?,
    }
    Ok(None)
}

#[cfg(test)]
mod tests;
