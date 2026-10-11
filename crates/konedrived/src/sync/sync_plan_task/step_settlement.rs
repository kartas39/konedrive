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
//! | `Failed(_)` | `remember_failed_try(Failed, next)` | the journal |
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
#![allow(dead_code, unused_variables)]

use konedrive_tree::{Reason, FullListingCause, LocalItemId, Refusal, SpaceRefusal, StepFact, SyncStep, TakenName, TreeError, TreeStore};

use super::local_step_tries::LocalStepTries;
use crate::folder::step_outcome::{StepEvent, StepOutcome};
use crate::upload::sending_holds::SendingHold;

/// One write of the step memory, as its operations are named. A fact of a local change
/// that is not sent — a failed try, a refusal, a refusal for lack of space, a taken name —
/// is written with the stamp of the versions the step was planned from
/// (`ItemVersions::unsent_since`, read off the step by `write_step_settlements`), and the
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
    pub memory: Vec<MemoryWrite>,
    /// What the task's own tries of the step become.
    pub local_step_tries: LocalStepTriesChange,
    pub cycle: CycleRequest,
    /// What holds all sending from now on, when the outcome says so.
    pub hold: Option<SendingHold>,
    /// Whether a step towards OneDrive went through (`SendingHolds::pass`).
    pub went_through: bool,
    pub events: Vec<StepEvent>,
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
    todo!()
}

/// When the next try of a step that failed `count` times is due, Unix seconds: a second
/// after the first failure, doubling to an hour (`upload/engine.rs:108`, `backoff_after`).
/// Pure.
pub fn decide_next_try(count: u32, now: i64) -> i64 {
    todo!()
}

/// What follows `step` ended as `outcome`: the table of the module's doc. `tries` is how
/// many tries of the step failed before this one: for a step towards OneDrive, what the
/// store answered of the unsent change in the plan's round (`ItemMemory::failed_tries`,
/// which is of the stamp the step was planned from, or not answered); for a step towards
/// the folder or of the rows, `LocalStepTries::count_tries` of the step. Pure.
pub fn decide_step_settlement(step: &SyncStep, outcome: &StepOutcome, tries: u32, now: i64) -> StepSettlement {
    todo!()
}

/// Applies what the settlements of `ended` say of the task's own tries, each to the step
/// it is of (`LocalStep::of`): a failed try counted, the tries of a written step
/// forgotten. No store, no clock. Called by `SyncPlanTask::settle_steps`, under the lock
/// of the task's tries.
pub fn change_local_step_tries(tries: &mut LocalStepTries, ended: &[(SyncStep, StepSettlement)]) {
    todo!()
}

/// Writes what `settlements` say of the step memory, and their events to the activity
/// log (`TreeStore::add_activity`, with the folder's path before each, capped by
/// [`decide_events_to_log`]), in ONE job of the store's thread; how many events were
/// logged. A fact the row does not say any more is not written
/// (`FactStanding::VersionGone`): the item changed since, and the next plan decides.
/// Called by `SyncPlanTask::settle_steps`.
pub fn write_step_settlements(store: &mut TreeStore, settlements: &[StepSettlement], folder: &std::path::Path, logged_since_listing_ended: usize) -> Result<usize, TreeError> {
    todo!()
}
