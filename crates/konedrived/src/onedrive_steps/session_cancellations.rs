//! What is left to clean in OneDrive after uploads that did not end well: the upload
//! sessions nothing goes on with, cancelled; the items uploads left with other content
//! whose own item is gone, removed; and the openings that name no file any more,
//! forgotten.
//!
//! **State owned:** the tries that failed, for this run of the folder's sync: for each
//! session and each item that could not be cleaned, how many tries failed and when the
//! next is due ([`CancellationTries`]). In memory, shared by every copy of
//! [`SessionCancellations`]; changed by its two functions alone. What is left to clean is
//! not kept here: the step memory's tables `upload_session_cancellations`,
//! `mismatched_item_removals` and `upload_session_openings` are read and cleared through
//! its own operations.
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
#![allow(dead_code, unused_variables)]

use std::sync::{Arc, Mutex};

use konedrive_tree::{OneDriveItemReference, UploadSessionAddress};

use super::GraphAccess;

/// What a failed try was of.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CleanedThing {
    UploadSession(UploadSessionAddress),
    MismatchedItem(OneDriveItemReference),
}

/// The tries that failed, each with when its next is due, Unix seconds. An entry goes
/// when its thing was cleaned, and with the run.
#[derive(Default)]
struct CancellationTries {
    tries: Vec<(CleanedThing, u32, i64)>,
}

/// When the next try of a cancel or a removal that failed `failed_count` times is due,
/// Unix seconds: a minute after the first failure, doubling to an hour; after a throttle
/// not before the time OneDrive named. Pure.
pub fn decide_next_cancellation_try(failed_count: u32, retry_after: Option<std::time::Duration>, now: i64) -> i64 {
    todo!()
}

/// What one call of [`SessionCancellations::cancel_sessions`] or
/// [`SessionCancellations::remove_mismatched_items`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CancellationRound {
    /// How many are cleaned and forgotten: cancelled or removed, found gone, or — an
    /// item — found changed by another and left.
    pub done_count: usize,
    /// How many were asked and failed: each waits for its next try.
    pub failed_count: usize,
    /// How many were passed over because their next try is not due.
    pub waiting_count: usize,
    /// When the soonest next try of what failed or waits is due, Unix seconds; none when
    /// nothing does.
    pub next_try_at: Option<i64>,
}

/// What cancels and removes this folder's leftovers in OneDrive.
#[derive(Clone)]
pub struct SessionCancellations {
    graph: GraphAccess,
    tries: Arc<Mutex<CancellationTries>>,
}

impl SessionCancellations {
    pub fn new(graph: GraphAccess) -> Self {
        Self { graph, tries: Arc::default() }
    }

    /// Cancels the session at `address` (`cancel_upload`, `G/upload.rs:270`) and, when it
    /// was cancelled or found gone, forgets it
    /// (`StepMemory::forget_upload_session_cancellation`). Whether it is gone now. Asked
    /// whatever its next try: called by `NameHolders` for a session whose empty file
    /// holds the name a step needs now. A failure is counted as a failed try all the
    /// same.
    pub async fn cancel_session(&self, address: &UploadSessionAddress) -> bool {
        todo!()
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
        todo!()
    }

    /// Removes up to `limit` items of `ItemReader::list_mismatched_item_removals` in
    /// OneDrive, each in the drive it is listed with (`MismatchedItem::item`), and each
    /// only while it is unchanged since the upload that left it
    /// (`MismatchedItem::is_unchanged_since_upload`, read by `item`, `G/mod.rs:217`;
    /// `delete_item`, `G/write.rs:140`, guarded by the eTag just read), and forgets each
    /// that was removed, found gone, or found changed by another
    /// (`StepMemory::forget_mismatched_item`). The list never holds an item the folder
    /// has. One that fails waits, and those that wait are passed over, as
    /// [`cancel_sessions`](Self::cancel_sessions) does; called as it is.
    pub async fn remove_mismatched_items(&self, limit: usize) -> CancellationRound {
        todo!()
    }

    /// Forgets the openings that name no file and whose last try is older than
    /// `UPLOAD_SESSION_OPENING_KEPT` (`StepMemory::forget_upload_session_openings_before`).
    /// Called by the plan's task at its start and once a day.
    pub async fn forget_old_openings(&self) {
        todo!()
    }
}
