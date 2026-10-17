//! Graph's answer to a step as the step's commit takes it, and what a request is guarded
//! with.
//!
//! **State owned:** none: two pure functions.
//!
//! **Answers for:** Graph's `driveItem` as a step's commit takes it, a
//! `ConfirmedItem` — the id, `parentReference`, the name Graph stored (not the one sent),
//! the eTag, and for a file size, time, quickXorHash and cTag as
//! `ContentVersion::Hashed`. And the guard comes from the versions the step was planned
//! from (`docs/design/writes.md` §4.2).
//!
//! **Tested without the rest:**
// test: an_answer_is_confirmed_with_the_name_graph_stored
// test: a_files_answer_carries_its_content_as_hashed_and_a_directorys_none
// test: an_answer_with_no_id_no_parent_no_name_or_no_etag_is_unreadable
// test: an_answer_that_names_the_item_as_deleted_or_as_the_root_is_unreadable
// test: a_drive_id_spelt_in_another_case_is_the_folders_drive
// test: the_guard_is_the_etag_planned_from_else_the_ctag_of_the_synced_content
// test: an_item_with_neither_has_no_guard
// test: the_guard_of_a_content_is_the_ctag_of_onedrives_content_else_the_synced_ones_else_the_etag
// test: a_guard_matches_an_item_by_the_tag_it_names
// test: the_guard_of_an_answer_is_its_etag_else_its_ctag
// test: a_name_that_is_not_utf8_is_refused_for_good_and_any_other_is_given_as_it_is

use std::ffi::OsStr;

use konedrive_graph::drive::DriveItem;
use konedrive_tree::{ConfirmedItem, ContentVersion, DriveId, ItemVersions, OneDriveItemId, OneDriveItemReference};

use crate::sync_parts::step_outcome::{StepFailure, StepRefusal};

/// The `If-Match` value of a guarded request, and which of the item's two tags it is.
/// OneDrive takes either (measured on OneDrive personal, 2026-10-14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestGuard {
    /// The item's eTag, which any change of the item raises: of its content, and of its
    /// name, place or time too.
    ETag(String),
    /// The cTag of the item's content, which only a change of the content raises.
    CTag(String),
}

impl RequestGuard {
    pub fn as_str(&self) -> &str {
        match self {
            Self::ETag(tag) | Self::CTag(tag) => tag,
        }
    }

    /// Whether `found` still has the tag this guard names. Pure.
    pub fn matches(&self, found: &DriveItem) -> bool {
        match self {
            Self::ETag(tag) => found.e_tag.as_deref() == Some(tag.as_str()),
            Self::CTag(tag) => found.c_tag.as_deref() == Some(tag.as_str()),
        }
    }
}

/// `name` as Graph's client takes it. A name that is not UTF-8 is never planned to be
/// sent (the upload rules refuse it: `SyncPlanInput::name_refusals`); met here it is
/// `StepRefusal::RefusedForGood`, and nothing is sent. Pure.
pub(crate) fn to_graph_name(name: &OsStr) -> Result<&str, StepRefusal> {
    name.to_str().ok_or_else(|| StepRefusal::RefusedForGood { message: format!("the name {name:?} is not UTF-8") })
}

/// What a request about `item` is guarded with: the eTag of the OneDrive version it was
/// planned from, else the cTag of the content the two sides agreed on (an edit made on a
/// download whose eTag is not known). `None`
/// for an item with neither: its guarded step is answered `StepFailure::AnswerUnreadable`,
/// which counts a try and asks for nothing: the step waits, and is planned with a guard
/// once a listing that names the item wrote its eTag. Pure.
pub(crate) fn decide_guard(item: &ItemVersions) -> Option<RequestGuard> {
    let synced_ctag = || find_ctag(item.synced_content.as_ref()).map(RequestGuard::CTag);
    item.onedrive_etag.clone().map(RequestGuard::ETag).or_else(synced_ctag)
}

/// What a request that sends a content into `item`'s item of OneDrive is guarded with:
/// the cTag of the content OneDrive has as the row knows it — its OneDrive version's,
/// else the synced content's. Only a change of the content breaks it: a rename or a move
/// made in OneDrive meanwhile does not hold the upload back, and neither does the eTag
/// OneDrive raises when an upload session is opened with the file's time (measured on
/// OneDrive personal, 2026-10-14). With no cTag known, the guard of any other request
/// ([`decide_guard`]). Pure.
pub(crate) fn decide_content_guard(item: &ItemVersions) -> Option<RequestGuard> {
    let onedrive_content = item.onedrive.as_ref().and_then(|onedrive| onedrive.content.as_ref());
    find_ctag(onedrive_content).or_else(|| find_ctag(item.synced_content.as_ref())).map(RequestGuard::CTag).or_else(|| decide_guard(item))
}

/// The cTag of `content`, when it is one OneDrive answered with. Pure.
fn find_ctag(content: Option<&ContentVersion>) -> Option<String> {
    match content {
        Some(ContentVersion::Hashed { ctag, .. }) => ctag.clone(),
        Some(ContentVersion::ChangedHere { .. }) | None => None,
    }
}

/// What a request that follows a read of `answer` is guarded with: the eTag just read,
/// else its cTag; an answer with neither gives an empty guard, and what OneDrive makes of
/// it. Pure.
pub(crate) fn to_guard_of_answer(answer: &DriveItem) -> RequestGuard {
    match (&answer.e_tag, &answer.c_tag) {
        (Some(etag), _) => RequestGuard::ETag(etag.clone()),
        (None, Some(ctag)) => RequestGuard::CTag(ctag.clone()),
        (None, None) => RequestGuard::ETag(String::new()),
    }
}

/// `answer` as a commit takes it, in the drive `drive_id` unless the answer names
/// another. Pure. The commit resolves both references to rows itself. A file's content is
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

#[cfg(test)]
pub(in crate::onedrive_steps) mod tests;
