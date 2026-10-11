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

use std::ffi::OsStr;

use konedrive_graph::drive::DriveItem;
use konedrive_tree::{ConfirmedItem, ContentVersion, DriveId, ItemVersions, OneDriveItemId, OneDriveItemReference};

use crate::sync_parts::step_outcome::{StepFailure, StepRefusal};

/// The `If-Match` value of a guarded request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestGuard(String);

impl RequestGuard {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `name` as Graph's client takes it. A name that is not UTF-8 is never planned to be
/// sent (the upload rules refuse it: `SyncPlanInput::name_refusals`); met here it is
/// `StepRefusal::RefusedForGood`, and nothing is sent. Pure.
pub fn to_graph_name(name: &OsStr) -> Result<&str, StepRefusal> {
    name.to_str().ok_or_else(|| StepRefusal::RefusedForGood { message: format!("the name {name:?} is not UTF-8") })
}

/// What a request about `item` is guarded with: the eTag of the OneDrive version it was
/// planned from, else the cTag of the content the two sides agreed on (an edit made on a
/// download whose eTag is not known: `upload/steps/shared.rs:198-221`, `Guard`). `None`
/// for an item with neither: its guarded step is answered `StepFailure::AnswerUnreadable`,
/// which counts a try and asks for nothing: the step waits, and is planned with a guard
/// once a listing that names the item wrote its eTag. Pure.
pub fn decide_guard(item: &ItemVersions) -> Option<RequestGuard> {
    let synced_ctag = || match &item.synced_content {
        Some(ContentVersion::Hashed { ctag, .. }) => ctag.clone(),
        Some(ContentVersion::ChangedHere { .. }) | None => None,
    };
    item.onedrive_etag.clone().or_else(synced_ctag).map(RequestGuard)
}

/// What a request that follows a read of `answer` is guarded with: the eTag just read,
/// else its cTag; an answer with neither gives an empty guard, and what OneDrive makes of
/// it (today's `Guard::of_item`). Pure.
pub fn to_guard_of_answer(answer: &DriveItem) -> RequestGuard {
    RequestGuard(answer.e_tag.clone().or_else(|| answer.c_tag.clone()).unwrap_or_default())
}

/// `answer` as a commit takes it, in the drive `drive_id` unless the answer names
/// another. Pure. Replaces `upload/steps/shared.rs:159` (`answer_row`): the commit
/// resolves both references to rows itself. A file's content is
/// `ContentVersion::Hashed` of the answer's size, `fileSystemInfo` time in whole
/// seconds, quickXorHash and cTag. Unreadable (`StepFailure::AnswerUnreadable`): an
/// answer with no id, no parent's id, no name or no eTag, and one that names the item as
/// deleted or as the root — OneDrive's failure, tried again as any other.
pub fn to_confirmed_item(answer: &DriveItem, drive_id: &DriveId) -> Result<ConfirmedItem, StepFailure> {
    let unreadable = |what: &str| StepFailure::AnswerUnreadable { detail: format!("OneDrive answered an item {what}") };
    if answer.id.is_empty() {
        return Err(unreadable("with no id"));
    }
    if answer.deleted.is_some() {
        return Err(unreadable("that is deleted"));
    }
    if answer.root.is_some() {
        return Err(unreadable("that is the root"));
    }
    let parent = answer.parent_reference.as_ref();
    let Some(parent_id) = parent.and_then(|parent| parent.id.clone()).filter(|id| !id.is_empty()) else { return Err(unreadable("with no parent")) };
    let Some(name) = answer.name.clone().filter(|name| !name.is_empty()) else { return Err(unreadable("with no name")) };
    let Some(etag) = answer.e_tag.clone().filter(|etag| !etag.is_empty()) else { return Err(unreadable("with no eTag")) };
    // Graph spells one drive's id in more than one case: such an answer is of the folder's drive.
    let answered_drive = parent.and_then(|parent| parent.drive_id.as_deref()).filter(|answered| !answered.eq_ignore_ascii_case(drive_id.as_str()));
    let drive_id = match answered_drive {
        Some(answered) => DriveId::new(answered).ok_or_else(|| unreadable("in a drive that cannot be read"))?,
        None => drive_id.clone(),
    };
    let content = answer.file.is_some().then(|| ContentVersion::Hashed {
        size: answer.size.unwrap_or(0),
        mtime: answer.mtime(),
        mtime_nsec: 0,
        hash: answer.quick_xor_hash().map(str::to_owned),
        ctag: answer.c_tag.clone(),
    });
    Ok(ConfirmedItem {
        item: OneDriveItemReference { drive_id: drive_id.clone(), item_id: OneDriveItemId::new(answer.id.clone()) },
        parent: OneDriveItemReference { drive_id, item_id: OneDriveItemId::new(parent_id) },
        name: name.into(),
        content,
        etag,
    })
}
