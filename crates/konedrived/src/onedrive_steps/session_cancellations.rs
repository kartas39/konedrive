//! What is left to clean in OneDrive after steps that did not end well: the upload
//! sessions nothing goes on with, cancelled; the items uploads left with other content
//! whose own item is gone, removed; the openings that name no file any more,
//! forgotten; and the requests whose answer was never committed, settled.
//!
//! **State owned:** the tries that failed, for this run of the folder's sync: for each
//! session and each item that could not be cleaned, how many tries failed and when the
//! next is due ([`CancellationTries`]). In memory, shared by every copy of
//! [`SessionCancellations`]; changed by its functions alone. What is left to clean is
//! not kept here: the step memory's tables `upload_session_cancellations`,
//! `mismatched_item_removals`, `upload_session_openings` and `request_records` are read
//! and cleared through its own operations.
//!
//! **Answers for:** an abandoned session's empty file not holding a name for days; and
//! a session that cannot be cancelled now neither being asked with every plan nor keeping
//! those behind it from being asked.
//! Today: `cancel_given_up` (`upload/mod.rs:153`), `cancel_session` (`:136`).
//!
//! **Tested without the rest**, against the fixture's fake OneDrive:
// test: a_session_to_cancel_is_cancelled_and_forgotten
// test: a_session_onedrive_no_longer_knows_is_forgotten
// test: a_cancel_that_fails_leaves_the_session_listed_and_waits_for_its_next_try
// test: a_session_that_waits_is_passed_over_and_the_one_behind_it_is_cancelled
// test: the_next_try_is_a_minute_after_the_first_failure_and_doubles_to_an_hour
// test: a_round_answers_how_many_got_through_how_many_failed_and_how_many_wait
// test: an_item_an_upload_left_is_removed_while_it_is_unchanged_since_that_upload_and_forgotten
// test: an_item_an_upload_left_that_another_changed_since_is_left_and_forgotten
// test: openings_that_name_no_file_go_after_the_time_they_are_kept
// test: a_record_that_stands_is_settled_and_one_that_cannot_be_read_waits
// test: a_record_whose_step_runs_is_passed_over

use std::sync::{Arc, Mutex};
use std::time::Duration;

use konedrive_graph::drive::{DriveError, WriteError};
use konedrive_graph::pool::Class;
use konedrive_tree::{LocalItemId, MismatchedItem, OneDriveItemReference, UploadSessionAddress, UPLOAD_SESSION_OPENING_KEPT};

use super::confirmed_item::to_guard_of_answer;
use super::lost_answers::{LostAnswers, SettledRecord};
use super::GraphAccess;
use crate::sync_parts::running_steps::RunningSteps;
use crate::sync_parts::step_outcome::StepRefusal;

/// What a failed try was of.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CleanedThing {
    UploadSession(UploadSessionAddress),
    MismatchedItem(OneDriveItemReference),
    RequestRecord(LocalItemId),
}

/// The tries that failed, each with how many failed and when its next is due, Unix
/// seconds. An entry goes when its thing was cleaned, and with the run.
#[derive(Default)]
struct CancellationTries {
    tries: Vec<(CleanedThing, u32, i64)>,
}

/// The first wait after a cancel or a removal that failed, and the longest, in seconds.
const FIRST_WAIT: i64 = 60;
const LONGEST_WAIT: i64 = 60 * 60;

/// When the next try of a cancel or a removal that failed `failed_count` times is due,
/// Unix seconds: a minute after the first failure, doubling to an hour; after a throttle
/// not before the time OneDrive named. Pure.
pub fn decide_next_cancellation_try(failed_count: u32, retry_after: Option<std::time::Duration>, now: i64) -> i64 {
    let wait = FIRST_WAIT.saturating_mul(1i64 << failed_count.saturating_sub(1).min(20)).min(LONGEST_WAIT);
    let named = retry_after.map_or(0, |named| i64::try_from(named.as_secs()).unwrap_or(i64::MAX));
    now.saturating_add(wait.max(named))
}

/// What one call of [`SessionCancellations::cancel_sessions`],
/// [`SessionCancellations::remove_mismatched_items`] or
/// [`SessionCancellations::settle_request_records`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CancellationRound {
    /// How many are cleaned and forgotten: cancelled or removed, found gone, or — an
    /// item — found changed by another and left; a record settled.
    pub done_count: usize,
    /// How many were asked and failed: each waits for its next try.
    pub failed_count: usize,
    /// How many were passed over because their next try is not due.
    pub waiting_count: usize,
    /// When the soonest next try of what failed or waits is due, Unix seconds; none when
    /// nothing does.
    pub next_try_at: Option<i64>,
    /// How many request records went with nothing learnt, because OneDrive refuses the
    /// read for certain (counted in `done_count` too): the caller asks the listing for a
    /// cycle, which brings what OneDrive has.
    pub left_to_listing_count: usize,
}

/// What became of one thing asked.
enum CleanedOutcome {
    Done,
    Failed { retry_after: Option<Duration> },
}

/// What cancels and removes this folder's leftovers in OneDrive.
#[derive(Clone)]
pub struct SessionCancellations {
    graph: GraphAccess,
    /// What settles a request record that stands.
    lost_answers: LostAnswers,
    /// Asked whether the step of a record's item runs: its record is the step's then.
    running_steps: Arc<RunningSteps>,
    tries: Arc<Mutex<CancellationTries>>,
}

impl SessionCancellations {
    pub fn new(graph: GraphAccess, lost_answers: LostAnswers, running_steps: Arc<RunningSteps>) -> Self {
        Self { graph, lost_answers, running_steps, tries: Arc::default() }
    }

    /// Whether the next try of `thing` is not due at `now`.
    fn is_waiting(&self, thing: &CleanedThing, now: i64) -> bool {
        crate::panic::lock(&self.tries).tries.iter().any(|(tried, _, next_try_at)| tried == thing && *next_try_at > now)
    }

    /// How many things of the kind `is_of_kind` tells have a failed try kept.
    fn count_tried(&self, is_of_kind: fn(&CleanedThing) -> bool) -> usize {
        crate::panic::lock(&self.tries).tries.iter().filter(|(tried, _, _)| is_of_kind(tried)).count()
    }

    /// `thing` was cleaned: its tries go.
    fn forget_tries(&self, thing: &CleanedThing) {
        crate::panic::lock(&self.tries).tries.retain(|(tried, _, _)| tried != thing);
    }

    /// A try of `thing` failed at `now`: counted, with its next try.
    fn count_failed_try(&self, thing: CleanedThing, retry_after: Option<Duration>, now: i64) {
        let mut kept = crate::panic::lock(&self.tries);
        let failed_count = kept.tries.iter().find(|(tried, _, _)| *tried == thing).map_or(0, |(_, failed_count, _)| *failed_count).saturating_add(1);
        let next_try_at = decide_next_cancellation_try(failed_count, retry_after, now);
        kept.tries.retain(|(tried, _, _)| *tried != thing);
        kept.tries.push((thing, failed_count, next_try_at));
    }

    /// The soonest next try kept of a thing of the kind `is_of_kind` tells.
    fn find_next_try(&self, is_of_kind: fn(&CleanedThing) -> bool) -> Option<i64> {
        crate::panic::lock(&self.tries).tries.iter().filter(|(tried, _, _)| is_of_kind(tried)).map(|(_, _, next_try_at)| *next_try_at).min()
    }

    /// Counts what came of `thing` into `round` and into the tries.
    fn settle_outcome(&self, round: &mut CancellationRound, thing: CleanedThing, outcome: CleanedOutcome) {
        match outcome {
            CleanedOutcome::Done => {
                self.forget_tries(&thing);
                round.done_count += 1;
            }
            CleanedOutcome::Failed { retry_after } => {
                self.count_failed_try(thing, retry_after, (self.graph.clock)());
                round.failed_count += 1;
            }
        }
    }

    /// The time OneDrive named in `error`, when it is a throttle.
    fn find_retry_after(error: &WriteError) -> Option<Duration> {
        match error {
            WriteError::Throttled { retry_after } => *retry_after,
            _ => None,
        }
    }

    /// Cancels the session at `address` and forgets it when it is gone; what came of it.
    async fn cancel_one_session(&self, address: &UploadSessionAddress) -> CleanedOutcome {
        let cancelled = {
            let _slot = self.graph.drive.pool().acquire(Class::Metadata).await;
            self.graph.drive.cancel_upload(address.as_str()).await
        };
        if let Err(error) = cancelled {
            tracing::info!("an upload session nothing goes on with is not cancelled yet: {error}");
            return CleanedOutcome::Failed { retry_after: Self::find_retry_after(&error) };
        }
        let forgotten = address.clone();
        match self.graph.store.call(move |store| store.step_memory().forget_upload_session_cancellation(&forgotten)).await {
            Ok(()) => CleanedOutcome::Done,
            Err(error) => {
                tracing::warn!("a cancelled upload session is not forgotten: {error}");
                CleanedOutcome::Failed { retry_after: None }
            }
        }
    }

    /// Cancels the session at `address` (`cancel_upload`, `G/upload.rs:270`) and, when it
    /// was cancelled or found gone, forgets it
    /// (`StepMemory::forget_upload_session_cancellation`). Whether it is gone now. Asked
    /// whatever its next try: called by `NameHolders` for a session whose empty file
    /// holds the name a step needs now. A failure is counted as a failed try all the
    /// same.
    pub async fn cancel_session(&self, address: &UploadSessionAddress) -> bool {
        let thing = CleanedThing::UploadSession(address.clone());
        let outcome = self.cancel_one_session(address).await;
        let is_gone = matches!(outcome, CleanedOutcome::Done);
        self.settle_outcome(&mut CancellationRound::default(), thing, outcome);
        is_gone
    }

    /// Cancels up to `limit` sessions of `ItemReader::list_upload_session_cancellations`,
    /// the one abandoned first first, passing over those whose next try is not due (it
    /// lists `limit` more than wait, so that a session that cannot be cancelled keeps
    /// none behind it from being asked). One that fails waits
    /// ([`decide_next_cancellation_try`]); it is not asked a second time in the call.
    /// Called by the plan's task, one at a time, at each round in which sending is not
    /// held, while the list is not empty: a removal of a file by a program puts a session
    /// there with no step at all. The task goes on while `done_count` is not 0, and
    /// otherwise leaves what is left to a later plan.
    pub async fn cancel_sessions(&self, limit: usize) -> CancellationRound {
        let is_session = |thing: &CleanedThing| matches!(thing, CleanedThing::UploadSession(_));
        let mut round = CancellationRound::default();
        let listed_count = limit.saturating_add(self.count_tried(is_session));
        let addresses = match self.graph.store.read_items(move |items| items.list_upload_session_cancellations(listed_count)).await {
            Ok(addresses) => addresses,
            Err(error) => {
                tracing::warn!("cannot read the upload sessions left to cancel: {error}");
                return round;
            }
        };
        for address in addresses {
            let thing = CleanedThing::UploadSession(address.clone());
            if self.is_waiting(&thing, (self.graph.clock)()) {
                round.waiting_count += 1;
                continue;
            }
            if round.done_count + round.failed_count >= limit {
                break;
            }
            let outcome = self.cancel_one_session(&address).await;
            self.settle_outcome(&mut round, thing, outcome);
        }
        round.next_try_at = self.find_next_try(is_session);
        round
    }

    /// Removes `left` in OneDrive while it is still what the upload left, and forgets it
    /// when it is removed, gone or another's by now; what came of it.
    async fn remove_one_mismatched_item(&self, left: &MismatchedItem) -> CleanedOutcome {
        // The Graph client asks in the account's own drive: an item of another is not this client's to reach.
        if left.item.drive_id != self.graph.drive_id {
            tracing::warn!("an item an upload left is in another drive than the folder's: it is not removed");
            return CleanedOutcome::Failed { retry_after: None };
        }
        let id = left.item.item_id.as_str();
        // Whether OneDrive does not have the item any more; false for one that is another's by now and left where it is.
        let removed: Result<bool, WriteError> = {
            let _slot = self.graph.drive.pool().acquire(Class::Metadata).await;
            match self.graph.drive.item(id).await {
                Err(DriveError::NotFound) => Ok(true),
                Err(error) => Err(WriteError::from(error)),
                // Another changed it since the upload: theirs now.
                Ok(found) if !left.is_unchanged_since_upload(found.c_tag.as_deref(), found.e_tag.as_deref()) => Ok(false),
                Ok(found) => match self.graph.drive.delete_item(id, to_guard_of_answer(&found).as_str()).await {
                    Ok(()) | Err(WriteError::NotFound) => Ok(true),
                    // A 412: changed between the read and the removal.
                    Err(WriteError::Changed) => Ok(false),
                    Err(error) => Err(error),
                },
            }
        };
        let is_gone = match removed {
            Ok(is_gone) => is_gone,
            Err(error) => {
                tracing::info!("an item an upload left with other content is not removed yet: {error}");
                return CleanedOutcome::Failed { retry_after: Self::find_retry_after(&error) };
            }
        };
        let forgotten = left.item.clone();
        // Removed or gone: the row a listing made of it loses its OneDrive version in the same transaction.
        let forget = move |store: &mut konedrive_tree::TreeStore| if is_gone { store.step_memory().forget_removed_mismatched_item(&forgotten) } else { store.step_memory().forget_mismatched_item(&forgotten) };
        match self.graph.store.call(forget).await {
            Ok(()) => CleanedOutcome::Done,
            Err(error) => {
                tracing::warn!("an item an upload left is not forgotten: {error}");
                CleanedOutcome::Failed { retry_after: None }
            }
        }
    }

    /// Removes up to `limit` items of `ItemReader::list_mismatched_item_removals` in
    /// OneDrive, each in the drive it is listed with (`MismatchedItem::item`), and each
    /// only while it is unchanged since the upload that left it
    /// (`MismatchedItem::is_unchanged_since_upload`, read by `item`, `G/mod.rs:217`;
    /// `delete_item`, `G/write.rs:140`, guarded by the eTag just read), and forgets each
    /// that was removed or found gone (`StepMemory::forget_removed_mismatched_item`: the
    /// row a listing made of it loses its OneDrive version with it) or found changed by
    /// another (`StepMemory::forget_mismatched_item`). The list never holds an item the folder
    /// has. One that fails waits, and those that wait are passed over, as
    /// [`cancel_sessions`](Self::cancel_sessions) does; called as it is.
    pub async fn remove_mismatched_items(&self, limit: usize) -> CancellationRound {
        let is_item = |thing: &CleanedThing| matches!(thing, CleanedThing::MismatchedItem(_));
        let mut round = CancellationRound::default();
        let listed_count = limit.saturating_add(self.count_tried(is_item));
        let items_left = match self.graph.store.read_items(move |items| items.list_mismatched_item_removals(listed_count)).await {
            Ok(items_left) => items_left,
            Err(error) => {
                tracing::warn!("cannot read the items uploads left to remove: {error}");
                return round;
            }
        };
        for left in items_left {
            let thing = CleanedThing::MismatchedItem(left.item.clone());
            if self.is_waiting(&thing, (self.graph.clock)()) {
                round.waiting_count += 1;
                continue;
            }
            if round.done_count + round.failed_count >= limit {
                break;
            }
            let outcome = self.remove_one_mismatched_item(&left).await;
            self.settle_outcome(&mut round, thing, outcome);
        }
        round.next_try_at = self.find_next_try(is_item);
        round
    }

    /// Settles up to `limit` requests of `ItemReader::list_request_records` whose answer
    /// was never committed, the one sent first first (`LostAnswers::settle_lost_answer`:
    /// what became of each is read from OneDrive, and its answer committed when it went
    /// through). A record whose item has a running step is that step's and is passed
    /// over, as one whose next try is not due is; one that cannot be read now waits
    /// ([`decide_next_cancellation_try`], not before a time OneDrive named). While a
    /// record stands, its item and its destination are busy to the plan: the caller wakes
    /// the plan when `done_count` is not 0 (`WakeCause::RequestRecordSettled`), asks the
    /// listing for a cycle when `left_to_listing_count` is not 0, and its
    /// timer takes `next_try_at` (`sync_plan_task::decide_next_wake`). Called by the plan's task as
    /// [`cancel_sessions`](Self::cancel_sessions) is, which is after the first listing of
    /// a start ended.
    pub async fn settle_request_records(&self, limit: usize) -> CancellationRound {
        let is_record = |thing: &CleanedThing| matches!(thing, CleanedThing::RequestRecord(_));
        let mut round = CancellationRound::default();
        // Those of the running steps are listed too, and passed over.
        let listed_count = limit.saturating_add(self.count_tried(is_record)).saturating_add(self.running_steps.list_items().len());
        let records = match self.graph.store.read_items(move |items| items.list_request_records(listed_count)).await {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!("cannot read the requests whose answer was not committed: {error}");
                return round;
            }
        };
        for record in records {
            let thing = CleanedThing::RequestRecord(record.item);
            if self.running_steps.has_running_step(record.item) || self.is_waiting(&thing, (self.graph.clock)()) {
                round.waiting_count += 1;
                continue;
            }
            if round.done_count + round.failed_count >= limit {
                break;
            }
            let outcome = match self.lost_answers.settle_lost_answer(&record).await {
                Ok(SettledRecord::Settled) => CleanedOutcome::Done,
                Ok(SettledRecord::LeftToListing) => {
                    round.left_to_listing_count += 1;
                    CleanedOutcome::Done
                }
                Err(refusal) => {
                    tracing::info!("what became of the request of item {} is not known yet: {refusal:?}", record.item);
                    let retry_after = if let StepRefusal::Throttled { retry_after } = refusal { retry_after } else { None };
                    CleanedOutcome::Failed { retry_after }
                }
            };
            self.settle_outcome(&mut round, thing, outcome);
        }
        round.next_try_at = self.find_next_try(is_record);
        round
    }

    /// Forgets the openings that name no file and whose last try is older than
    /// `UPLOAD_SESSION_OPENING_KEPT` (`StepMemory::forget_upload_session_openings_before`).
    /// Called by the plan's task at its start and once a day.
    pub async fn forget_old_openings(&self) {
        let before = (self.graph.clock)().saturating_sub(UPLOAD_SESSION_OPENING_KEPT);
        match self.graph.store.call(move |store| store.step_memory().forget_upload_session_openings_before(before)).await {
            Ok(0) => {}
            Ok(forgotten_count) => tracing::debug!("{forgotten_count} openings of upload sessions that name no file are forgotten"),
            Err(error) => tracing::warn!("cannot forget the old openings of upload sessions: {error}"),
        }
    }
}
