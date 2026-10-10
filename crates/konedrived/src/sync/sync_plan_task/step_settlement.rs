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
//! | `Written`, towards the folder or of the rows | `forget_fact(FailedTries)` | the events |
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
//! **Tested without the rest**, a test per line of the table:
// test: each_outcome_is_settled_as_its_line_of_the_table_says
// test: the_next_try_is_a_second_after_the_first_failure_and_doubles_to_an_hour
// test: a_refusal_for_good_keeps_the_local_version_that_was_sent_and_the_removal_for_a_removal
// test: a_taken_name_keeps_the_place_sent_and_the_holders_name
// test: a_step_that_did_not_run_for_a_cause_that_does_not_end_by_itself_is_not_planned_again_at_once
// test: the_settlements_of_several_steps_are_written_in_one_job
#![allow(dead_code, unused_variables)]

use konedrive_tree::{Reason, FullListingCause, LocalItemId, Refusal, SpaceRefusal, StepFact, SyncStep, TakenName, TreeError, TreeStore};

use crate::folder::step_outcome::{StepEvent, StepOutcome};
use crate::upload::sending_holds::SendingHold;

/// One write of the step memory, as its operations are named. A fact of a local change
/// that is not sent — a failed try, a refusal, a refusal for lack of space, a taken name —
/// is written with the stamp of the versions the step was planned from
/// (`ItemVersions::unsent_since`, read off the step by `write_step_settlements`), and the
/// store keeps it only for a row that still has that stamp: nothing is kept of a step
/// planned from a row with nothing unsent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryWrite {
    /// `StepMemory::remember_failed_try(item, unsent_since, reason, next_try_at)`.
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
/// many tries of the item failed before this one (`ItemMemory::failed_tries` of the
/// plan's round). Pure.
pub fn decide_step_settlement(step: &SyncStep, outcome: &StepOutcome, tries: u32, now: i64) -> StepSettlement {
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
