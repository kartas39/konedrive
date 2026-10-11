//! What is written and asked for after a step, by what came of it: the one place that
//! counts tries, keeps a refusal and asks the listing for a cycle.
//!
//! **State owned:** none. [`decide_step_settlement`] is a pure function of the step and
//! its outcome; [`write_step_settlements`] writes the answers of the steps that ended
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
//! | `Refused(NoSpace { free_space: Some })` | `remember_space_refusal` | — |
//! | `Refused(NoSpace { free_space: None })` | — | OneDrive is full (`OneDriveSpace` said so already) |
//! | `Refused(Throttled)` | — (no try counted) | all sending held until the time |
//! | `Refused(SignedOut)` | — (no try counted) | all sending held until the sign-in |
//! | `Refused(Forbidden \| RefusedForGood)` | `remember_refusal`, with `RefusedVersion::Shown` of the local version sent, or `Removal` for a removal | the event, once per reason |
//! | `Refused(Locked \| UploadSessionGone \| Transient)` | `remember_failed_try(reason, next)` | — |
//! | `NotRun(OpenForWriting \| SendingStopped)` | — | — (the close, or the end of the hold, wakes the plan) |
//! | `NotRun(ContentChanged \| BackingFileMissing \| PlaceHeldOnDisk)` | `remember_failed_try(reason, next)` | the journal: none of them ends by itself |
//! | `Failed(UndoFailed)` | — | the mount's take-down was asked for by the runner |
//! | `Failed(Store)` | `remember_failed_try(Store, next)` | the journal |
//! | `Failed(Disk)` | `remember_failed_try(LocalIo, next)` | the journal |
//! | `Failed(AnswerUnreadable \| RunnerPanicked)` | `remember_failed_try(Failed, next)` | the journal |
//!
//! **Whose the failed tries are.** `remember_failed_try` in the table is two things, by
//! the direction of the step. A step towards OneDrive: the store's
//! (`StepMemory::remember_failed_try`, [`MemoryWrite::FailedTry`]), kept under the stamp
//! of the unsent change the step sends. A step towards the folder or of the rows: the
//! task's own ([`LocalStepTriesChange::FailedTry`], applied to its
//! [`LocalStepTries`](super::local_step_tries::LocalStepTries)), kept with the versions
//! the step was planned from, for the daemon's run; the store keeps nothing of such a try.
//! Either way the plan is told the next try (`plan_input::describe_next_tries`) and
//! answers the wait.
//!
//! **Tested without the rest**, a test per line of the table:
// test: each_outcome_is_settled_as_its_line_of_the_table_says
// test: the_next_try_is_a_second_after_the_first_failure_and_doubles_to_an_hour
// test: a_refusal_for_good_keeps_the_local_version_that_was_sent_and_the_removal_for_a_removal
// test: a_taken_name_keeps_the_place_sent_and_the_holders_name
// test: a_step_that_did_not_run_for_a_cause_that_does_not_end_by_itself_is_not_planned_again_at_once
// test: the_settlements_of_several_steps_are_written_in_one_job
// test: a_failed_try_of_a_step_towards_onedrive_is_the_stores_and_any_other_the_tasks
// test: a_step_towards_the_folder_that_is_written_forgets_the_tasks_tries_of_it

use konedrive_tree::{ActivityKind, ActivityRow, FactStanding, FailedTryStanding, FullListingCause, ItemVersions, LocalItemId, OneDriveStep, Reason, Refusal, RefusedVersion, SpaceRefusal, StepFact, SyncStep, TakenName, TreeError, TreeStore};

use super::local_step_tries::{LocalStep, LocalStepTries};
use crate::folder::step_outcome::{RowsChangedFollowUp, StepEvent, StepFailure, StepObstacle, StepOutcome, StepRefusal};
use crate::upload::sending_holds::SendingHold;
use crate::upload::{BACKOFF_FIRST, BACKOFF_MAX};

/// One write of the step memory, as its operations are named. A fact of a local change
/// that is not sent — a failed try, a refusal, a refusal for lack of space, a taken name —
/// is written with the stamp of the versions the step was planned from
/// (`ItemVersions::unsent_since`, kept in `StepSettlement::unsent_since`), and the
/// store keeps it only for a row that still has that stamp. All of them are of a step
/// towards OneDrive: nothing is written to the store for a step towards the folder or of
/// the rows that failed ([`LocalStepTriesChange`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryWrite {
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
    Forget(StepFact),
}

/// What a step's end says of the tries the task keeps of the steps towards the folder and
/// of the rows (`LocalStepTries`). `Nothing` for every step towards OneDrive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalStepTriesChange {
    Nothing,
    /// `LocalStepTries::remember_failed_try(step, reason, next_try_at)`.
    FailedTry { reason: Reason, next_try_at: Option<i64> },
    /// The step was written: `LocalStepTries::forget_tries(step)`.
    Forgotten,
}

/// Whether the listing is asked for a cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleRequest {
    None,
    /// The changes since the delta link (`ListingPollHandle::refresh`).
    Changes,
    /// A full listing (`ItemStore::request_full_listing`, then the poller).
    Full(FullListingCause),
}

/// What follows a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepSettlement {
    pub item: LocalItemId,
    /// The stamp of the unsent change a step towards OneDrive was planned for
    /// (`ItemVersions::unsent_since`): what the store keeps each write of `memory` under.
    /// None for a step towards the folder or of the rows, which writes none.
    pub unsent_since: Option<u64>,
    pub memory: Vec<MemoryWrite>,
    /// What the task's own tries of the step become.
    pub local_step_tries: LocalStepTriesChange,
    pub cycle: CycleRequest,
    /// What holds all sending from now on, when the outcome says so. For a throttle
    /// OneDrive named no time for, `until` is `now`: the end is chosen by `SendingHolds`,
    /// which the task gives the outcome's own `retry_after`.
    pub hold: Option<SendingHold>,
    /// Whether a step towards OneDrive went through (`SendingHolds::pass`).
    pub went_through: bool,
    pub events: Vec<StepEvent>,
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
/// listing ended. Pure.
// OPEN: the map's MISSING-4.c: today a cycle logs one batch, capped, and "a folder
// removed with everything in it is one removal" (`remote/listing/reconcile.rs:376-391`).
// Written as: each step's events are logged as they come, up to today's cap per listing;
// a removed directory is one event because its children's `RemoveItem` steps answer
// none. Pure, and can change without touching a signature.
pub fn decide_events_to_log(events: &[StepEvent], logged_since_listing_ended: usize) -> Vec<StepEvent> {
    let mut left = crate::status::activity::PER_KIND.saturating_sub(logged_since_listing_ended);
    let mut logged = Vec::new();
    for event in events {
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
/// many tries of the step failed before this one: for a step towards OneDrive, what the
/// store answered of the unsent change in the plan's round (`ItemMemory::failed_tries`,
/// which is of the stamp the step was planned from, or not answered); for a step towards
/// the folder or of the rows, `LocalStepTries::count_tries` of the step. Pure.
pub fn decide_step_settlement(step: &SyncStep, outcome: &StepOutcome, tries: u32, now: i64) -> StepSettlement {
    let is_towards_onedrive = matches!(step, SyncStep::OneDrive(_));
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
        if is_towards_onedrive {
            settlement.memory.push(MemoryWrite::FailedTry { reason, next_try_at });
        } else {
            settlement.local_step_tries = LocalStepTriesChange::FailedTry { reason, next_try_at };
        }
    };
    match outcome {
        StepOutcome::Written { events } => {
            settlement.events = events.clone();
            if is_towards_onedrive {
                settlement.went_through = true;
            } else {
                settlement.local_step_tries = LocalStepTriesChange::Forgotten;
            }
        }
        StepOutcome::RowsChanged(RowsChangedFollowUp::Nothing | RowsChangedFollowUp::MadeItemRemoved(_)) => {}
        StepOutcome::RowsChanged(RowsChangedFollowUp::CycleWanted | RowsChangedFollowUp::MadeItemLeft(_)) => settlement.cycle = CycleRequest::Changes,
        StepOutcome::Refused(refusal) => match refusal {
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
        StepOutcome::Failed(StepFailure::AnswerUnreadable { .. } | StepFailure::RunnerPanicked { .. }) => count_failed_try(&mut settlement, Reason::Failed),
    }
    settlement
}

/// Whether what came of a step changed anything a plan is made from: the step was
/// written, its commit found the rows changed, OneDrive refused it (a try counted, a fact
/// kept, a hold set, OneDrive full), a try was counted, a cycle was asked for. A step
/// that did not run for a cause that ends by itself, and one whose undo failed, changed
/// nothing: the plan made after it would be the plan before it. What the plan's task
/// asks before it makes a plan for finished steps alone (`decide_round`). Pure.
pub fn has_changed_anything(outcome: &StepOutcome, settlement: &StepSettlement) -> bool {
    matches!(outcome, StepOutcome::Written { .. } | StepOutcome::RowsChanged(_) | StepOutcome::Refused(_))
        || !settlement.memory.is_empty()
        || settlement.local_step_tries != LocalStepTriesChange::Nothing
        || settlement.cycle != CycleRequest::None
        || settlement.hold.is_some()
}

/// Applies what the settlements of `ended` say of the task's own tries, each to the step
/// it is of (`LocalStep::of`): a failed try counted, the tries of a written step
/// forgotten. No store, no clock. Called by `SyncPlanTask::settle_steps`, under the lock
/// of the task's tries.
pub fn change_local_step_tries(tries: &mut LocalStepTries, ended: &[(SyncStep, StepSettlement)]) {
    for (step, settlement) in ended {
        let Some(local_step) = LocalStep::of(step) else { continue };
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
/// write the store refuses — an item whose row went — is said in the journal and is that
/// item's alone: the other writes and every event are written all the same.
/// Called by `SyncPlanTask::settle_steps`.
pub fn write_step_settlements(store: &mut TreeStore, settlements: &[StepSettlement], folder: &std::path::Path, logged_since_listing_ended: usize, now: i64) -> Result<usize, TreeError> {
    let mut events = Vec::new();
    let mut logged_count = 0;
    for settlement in settlements {
        for write in &settlement.memory {
            // A write the store refuses is that item's alone: an item whose row went has nothing to remember.
            match write_memory(store, settlement, write, folder, now) {
                Ok(event) => events.extend(event),
                Err(error) => tracing::warn!("cannot keep {write:?} of item {}: {error}", settlement.item),
            }
        }
        for event in decide_events_to_log(&settlement.events, logged_since_listing_ended + logged_count) {
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
    Ok(logged_count)
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
