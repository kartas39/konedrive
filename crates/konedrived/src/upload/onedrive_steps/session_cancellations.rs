//! What is left to clean in OneDrive after uploads that did not end well: the upload
//! sessions nothing goes on with, cancelled; the items uploads left with other content
//! whose own item is gone, removed; and the openings that name no file any more,
//! forgotten.
//!
//! **State owned:** none: the step memory's tables `upload_session_cancellations`,
//! `mismatched_item_removals` and `upload_session_openings` are read and cleared through
//! its own operations.
//!
//! **Answers for:** an abandoned session's empty file not holding a name for days.
//! Today: `cancel_given_up` (`upload/mod.rs:153`), `cancel_session` (`:136`).
//!
//! **Tested without the rest**, against the fixture's fake OneDrive:
// test: a_session_to_cancel_is_cancelled_and_forgotten
// test: a_session_onedrive_no_longer_knows_is_forgotten
// test: a_cancel_that_fails_leaves_the_session_listed_for_the_next_time
// test: an_item_an_upload_left_is_removed_while_it_is_unchanged_since_that_upload_and_forgotten
// test: an_item_an_upload_left_that_another_changed_since_is_left_and_forgotten
// test: openings_that_name_no_file_go_after_the_time_they_are_kept
#![allow(dead_code, unused_variables)]

use konedrive_tree::UploadSessionAddress;

use super::GraphAccess;

/// What cancels and removes this folder's leftovers in OneDrive.
#[derive(Clone)]
pub struct SessionCancellations {
    graph: GraphAccess,
}

impl SessionCancellations {
    pub fn new(graph: GraphAccess) -> Self {
        todo!()
    }

    /// Cancels the session at `address` (`cancel_upload`, `G/upload.rs:270`) and, when it
    /// was cancelled or found gone, forgets it
    /// (`StepMemory::forget_upload_session_cancellation`). Whether it is gone now.
    pub async fn cancel_session(&self, address: &UploadSessionAddress) -> bool {
        todo!()
    }

    /// Cancels up to `limit` sessions of `ItemReader::list_upload_session_cancellations`;
    /// how many are left that it could not cancel. Called by the plan's task at each
    /// round in which sending is not held, when the list is not empty: a removal of a
    /// file by a program puts a session there with no step at all.
    pub async fn cancel_sessions(&self, limit: usize) -> usize {
        todo!()
    }

    /// Removes up to `limit` items of `ItemReader::list_mismatched_item_removals` in
    /// OneDrive, each in the drive it is listed with (`MismatchedItem::item`), and each
    /// only while it is unchanged since the upload that left it
    /// (`MismatchedItem::is_unchanged_since_upload`, read by `item`, `G/mod.rs:217`;
    /// `delete_item`, `G/write.rs:140`), and forgets each
    /// (`StepMemory::forget_mismatched_item`). The list never holds an item the folder has.
    /// Called as
    /// [`cancel_sessions`](Self::cancel_sessions) is.
    pub async fn remove_mismatched_items(&self, limit: usize) -> usize {
        todo!()
    }

    /// Forgets the openings that name no file and whose last try is older than
    /// `UPLOAD_SESSION_OPENING_KEPT` (`StepMemory::forget_upload_session_openings_before`).
    /// Called by the plan's task at its start and once a day.
    pub async fn forget_old_openings(&self) {
        todo!()
    }
}
