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

use super::ItemReader;
use crate::model::{ItemPlace, LocalItemId, OneDriveItemReference};
use konedrive_reason::Reason;

use crate::{EmptyFileSession, ItemMemory, ItemRefusalReason, ItemTakenName, MismatchedItem, MismatchedItemUpload, OpeningWindow, TimedWait, TreeError, UploadSessionAddress};

impl ItemReader<'_> {
    /// As `StepMemory::find_item_memory`: everything remembered of `item` that its row
    /// still says.
    pub fn find_item_memory(&self, item: LocalItemId) -> Result<ItemMemory, TreeError> {
        todo!()
    }

    /// As `StepMemory::list_timed_waits`.
    pub fn list_timed_waits(&self, limit: usize) -> Result<Vec<TimedWait>, TreeError> {
        todo!()
    }

    /// As `StepMemory::list_refusal_reasons`: the items OneDrive refuses for good, of
    /// `reason` or of any.
    pub fn list_refusal_reasons(&self, reason: Option<&Reason>, after: Option<LocalItemId>, limit: usize) -> Result<Vec<ItemRefusalReason>, TreeError> {
        todo!()
    }

    /// As `StepMemory::count_refusal_reasons`: `BlockedCount`'s part of the step memory.
    pub fn count_refusal_reasons(&self, reason: Option<&Reason>) -> Result<u64, TreeError> {
        todo!()
    }

    /// As `StepMemory::list_space_refusal_reasons`.
    pub fn list_space_refusal_reasons(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<ItemRefusalReason>, TreeError> {
        todo!()
    }

    /// As `StepMemory::count_space_refusals`: `TooBigCount`.
    pub fn count_space_refusals(&self) -> Result<u64, TreeError> {
        todo!()
    }

    /// As `StepMemory::list_items_with_confirmed_removal`: the plan's
    /// `items_with_confirmed_removal`, those of items the plan was not given among them.
    pub fn list_items_with_confirmed_removal(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<LocalItemId>, TreeError> {
        todo!()
    }

    /// As `StepMemory::list_taken_names`.
    pub fn list_taken_names(&self, directory: LocalItemId) -> Result<Vec<ItemTakenName>, TreeError> {
        todo!()
    }

    /// As `StepMemory::list_empty_file_sessions`.
    pub fn list_empty_file_sessions(&self, place: &ItemPlace) -> Result<Vec<EmptyFileSession>, TreeError> {
        todo!()
    }

    /// As `StepMemory::list_opening_windows`.
    pub fn list_opening_windows(&self, place: &ItemPlace) -> Result<Vec<OpeningWindow>, TreeError> {
        todo!()
    }

    /// As `StepMemory::list_upload_session_cancellations`.
    pub fn list_upload_session_cancellations(&self, limit: usize) -> Result<Vec<UploadSessionAddress>, TreeError> {
        todo!()
    }

    /// As `StepMemory::list_mismatched_item_removals`: the items uploads left with other
    /// content whose removal is still to be asked, each with its drive; never an item the
    /// folder has.
    pub fn list_mismatched_item_removals(&self, limit: usize) -> Result<Vec<MismatchedItem>, TreeError> {
        todo!()
    }

    /// As `StepMemory::find_mismatched_item`: whether the item `onedrive_item` of OneDrive
    /// is one an upload of this folder left with other content, and of which file. What the
    /// plan's input is built with for an item a listing brought.
    pub fn find_mismatched_item(&self, onedrive_item: &OneDriveItemReference) -> Result<Option<MismatchedItemUpload>, TreeError> {
        todo!()
    }
}
