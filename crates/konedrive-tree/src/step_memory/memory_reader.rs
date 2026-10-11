//! What the steps remember, read through the reader: the step memory's readers over a
//! `&TreeStore`, so that the plan and the bus read it on the read-only connection and in
//! the transaction they read the rows in.
//!
//! **State owned:** none. Each calls the `pub(crate)` function of `step_memory` that
//! `StepMemory`'s reader of the same name calls. `StepMemory` (which borrows the store for
//! writing) keeps its writes; its own readers go when their callers are re-pointed.
//!
//! **Answers for:** the review's S13: `Changes()`, polled by the window, does not queue
//! behind a listing's page.
//!
//! **Tested without the rest:**
// test: each_memory_reader_answers_what_the_step_memorys_reader_of_the_same_name_answers
// test: the_memory_of_an_item_is_of_the_same_moment_as_its_row
#![allow(dead_code, unused_variables)]

use crate::item_store::ItemReader;
use crate::step_memory;
use crate::model::{ItemPlace, LocalItemId, OneDriveItemReference};
use konedrive_reason::Reason;

use crate::{EmptyFileSession, ItemMemory, ItemRefusalReason, ItemTakenName, MismatchedItem, MismatchedItemUpload, OpeningWindow, RequestRecord, TimedWait, TreeError, UploadSessionAddress};

impl ItemReader<'_> {
    /// As `StepMemory::find_item_memory`: everything remembered of `item` that its row
    /// still says.
    pub fn find_item_memory(&self, item: LocalItemId) -> Result<ItemMemory, TreeError> {
        step_memory::find_item_memory(self.conn(), item)
    }

    /// As `StepMemory::list_timed_waits`.
    pub fn list_timed_waits(&self, limit: usize) -> Result<Vec<TimedWait>, TreeError> {
        step_memory::list_timed_waits(self.conn(), limit)
    }

    /// As `StepMemory::list_refusal_reasons`: the items OneDrive refuses for good, of
    /// `reason` or of any.
    pub fn list_refusal_reasons(&self, reason: Option<&Reason>, after: Option<LocalItemId>, limit: usize) -> Result<Vec<ItemRefusalReason>, TreeError> {
        step_memory::list_refusal_reasons(self.conn(), reason, after, limit)
    }

    /// As `StepMemory::count_refusal_reasons`: `BlockedCount`'s part of the step memory.
    pub fn count_refusal_reasons(&self, reason: Option<&Reason>) -> Result<u64, TreeError> {
        step_memory::count_refusal_reasons(self.conn(), reason)
    }

    /// As `StepMemory::list_space_refusal_reasons`.
    pub fn list_space_refusal_reasons(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<ItemRefusalReason>, TreeError> {
        step_memory::list_space_refusal_reasons(self.conn(), after, limit)
    }

    /// As `StepMemory::count_space_refusals`: `TooBigCount`.
    pub fn count_space_refusals(&self) -> Result<u64, TreeError> {
        step_memory::count_space_refusals(self.conn())
    }

    /// As `StepMemory::list_items_with_confirmed_removal`: the plan's
    /// `items_with_confirmed_removal`, those of items the plan was not given among them.
    pub fn list_items_with_confirmed_removal(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<LocalItemId>, TreeError> {
        step_memory::list_items_with_confirmed_removal(self.conn(), after, limit)
    }

    /// As `StepMemory::list_taken_names`.
    pub fn list_taken_names(&self, directory: LocalItemId) -> Result<Vec<ItemTakenName>, TreeError> {
        step_memory::list_taken_names(self.conn(), directory)
    }

    /// As `StepMemory::list_empty_file_sessions`.
    pub fn list_empty_file_sessions(&self, place: &ItemPlace) -> Result<Vec<EmptyFileSession>, TreeError> {
        step_memory::list_empty_file_sessions(self.conn(), place)
    }

    /// As `StepMemory::list_opening_windows`.
    pub fn list_opening_windows(&self, place: &ItemPlace) -> Result<Vec<OpeningWindow>, TreeError> {
        step_memory::list_opening_windows(self.conn(), place)
    }

    /// As `StepMemory::list_upload_session_cancellations`.
    pub fn list_upload_session_cancellations(&self, limit: usize) -> Result<Vec<UploadSessionAddress>, TreeError> {
        step_memory::list_upload_session_cancellations(self.conn(), limit)
    }

    /// As `StepMemory::list_mismatched_item_removals`: the items uploads left with other
    /// content whose removal is still to be asked, each with its drive; never an item the
    /// folder has.
    pub fn list_mismatched_item_removals(&self, limit: usize) -> Result<Vec<MismatchedItem>, TreeError> {
        step_memory::list_mismatched_item_removals(self.conn(), limit)
    }

    /// As `StepMemory::find_mismatched_item`: whether the item `onedrive_item` of OneDrive
    /// is one an upload of this folder left with other content, and of which file. What the
    /// plan's input is built with for an item a listing brought.
    pub fn find_mismatched_item(&self, onedrive_item: &OneDriveItemReference) -> Result<Option<MismatchedItemUpload>, TreeError> {
        step_memory::find_mismatched_item(self.conn(), onedrive_item)
    }

    /// Up to `limit` requests that changed OneDrive and are not settled yet, the one sent
    /// first first: what the plan's input takes the busy items and the destinations from,
    /// beside the running steps, and what is settled by reading OneDrive after a start.
    pub fn list_request_records(&self, limit: usize) -> Result<Vec<RequestRecord>, TreeError> {
        step_memory::list_request_records(self.conn(), limit)
    }
}
