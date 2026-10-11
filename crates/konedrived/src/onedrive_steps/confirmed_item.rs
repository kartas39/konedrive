//! Graph's answer to a step as the step's commit takes it, and what a request is guarded
//! with.
//!
//! **State owned:** none: two pure functions.
//!
//! **Answers for:** stage C's "before each commit": Graph's `driveItem` as a
//! `ConfirmedItem` — the id, `parentReference`, the name Graph stored (not the one sent),
//! the eTag, and for a file size, time, quickXorHash and cTag as
//! `ContentVersion::Hashed`. And the map's F10: the guard comes from the versions the step
//! was planned from.
//!
//! **Tested without the rest:**
// test: an_answer_is_confirmed_with_the_name_graph_stored
// test: a_files_answer_carries_its_content_as_hashed_and_a_directorys_none
// test: an_answer_with_no_id_no_parent_no_name_or_no_etag_is_unreadable
// test: an_answer_that_names_the_item_as_deleted_or_as_the_root_is_unreadable
// test: a_drive_id_spelt_in_another_case_is_the_folders_drive
// test: the_guard_is_the_etag_planned_from_else_the_ctag_of_the_synced_content
// test: an_item_with_neither_has_no_guard
#![allow(dead_code, unused_variables)]

use std::ffi::OsStr;

use konedrive_graph::drive::DriveItem;
use konedrive_tree::{ConfirmedItem, DriveId, ItemVersions};

use crate::sync_parts::step_outcome::{StepFailure, StepRefusal};

/// The `If-Match` value of a guarded request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestGuard(String);

impl RequestGuard {
    pub fn as_str(&self) -> &str {
        todo!()
    }
}

/// `name` as Graph's client takes it. A name that is not UTF-8 is never planned to be
/// sent (the upload rules refuse it: `SyncPlanInput::name_refusals`); met here it is
/// `StepRefusal::RefusedForGood`, and nothing is sent. Pure.
pub fn to_graph_name(name: &OsStr) -> Result<&str, StepRefusal> {
    todo!()
}

/// What a request about `item` is guarded with: the eTag of the OneDrive version it was
/// planned from, else the cTag of the content the two sides agreed on (an edit made on a
/// download whose eTag is not known: `upload/steps/shared.rs:198-221`, `Guard`). `None`
/// for an item with neither: its guarded step is answered `StepFailure::AnswerUnreadable`,
/// which counts a try and asks for nothing: the step waits, and is planned with a guard
/// once a listing that names the item wrote its eTag. Pure.
pub fn decide_guard(item: &ItemVersions) -> Option<RequestGuard> {
    todo!()
}

/// `answer` as a commit takes it, in the drive `drive_id` unless the answer names
/// another. Pure. Replaces `upload/steps/shared.rs:159` (`answer_row`): the commit
/// resolves both references to rows itself. A file's content is
/// `ContentVersion::Hashed` of the answer's size, `fileSystemInfo` time in whole
/// seconds, quickXorHash and cTag. Unreadable (`StepFailure::AnswerUnreadable`): an
/// answer with no id, no parent's id, no name or no eTag, and one that names the item as
/// deleted or as the root — OneDrive's failure, tried again as any other.
pub fn to_confirmed_item(answer: &DriveItem, drive_id: &DriveId) -> Result<ConfirmedItem, StepFailure> {
    todo!()
}
