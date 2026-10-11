//! What a step made in OneDrive that its commit did not write, and what an upload left
//! there whose file was removed here before its answer came.
//!
//! **State owned:** none.
//!
//! **Answers for:** stage C's notes, "when a commit answers `RowsChanged`", and design
//! §6's cell "made and removed here before it was sent: if its sending is running, the
//! step finds the row gone at its commit and removes in OneDrive what it made"; and the
//! same after a restart (the map's F2), when no step runs any more and the item comes
//! back from OneDrive as new.
//!
//! **Tested without the rest**, against the fixture's fake OneDrive:
// test: a_new_item_whose_row_is_gone_is_removed_in_onedrive
// test: a_new_item_whose_row_is_still_there_asks_for_a_cycle
// test: a_made_item_that_cannot_be_removed_is_left_and_said
// test: a_listed_file_at_the_place_of_an_opening_that_names_no_file_is_this_folders_left_upload
// test: a_listed_file_made_outside_every_window_or_changed_since_is_anothers
#![allow(dead_code, unused_variables)]

use konedrive_tree::{ConfirmedItem, Item, ItemVersions, OpeningWindow};

use super::GraphAccess;
use crate::folder::step_outcome::RowsChangedFollowUp;

/// What is done about an item a step made in OneDrive whose commit wrote nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MadeItemFate {
    /// The item's row is gone: what was made is removed in OneDrive.
    Remove,
    /// The row is there, changed: a listing brings the made item as one of OneDrive's,
    /// and the plan compares the two (two new items of one name).
    LeaveForListing,
}

/// The fate of what `MakeDirectory` or `SendNewFile` made, from the item's row as it is
/// now. Pure.
pub fn decide_made_item_fate(row: Option<&Item>) -> MadeItemFate {
    todo!()
}

/// Whose a file is that a listing brought as new in OneDrive, at a place this folder was
/// about to open an upload session at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListedFileOrigin {
    /// Another's: shown as any new item.
    Another,
    /// This folder's own upload, whose file was removed here before the upload was
    /// confirmed: OneDrive made it within a window of an opening that names no file any
    /// more.
    OwnLeftUpload,
}

/// Whose `listed` is: `OwnLeftUpload` when it is a file no local version was ever agreed
/// on for, made in OneDrive within one of `opening_windows` (the clock's slack as for an
/// empty file), and no row the folder shows asks for that place. Pure.
// TODO: the owner's (the review's S26, the map's F2): what is done with `OwnLeftUpload` — the file is removed in OneDrive, since the user removed it here before it was confirmed, or shown again, not downloaded, as today. Written as decided here and ACTED ON by nobody: the plan's task does not call `find_left_upload` until that is said.
pub fn decide_listed_file_origin(listed: &Item, created_at: i64, opening_windows: &[OpeningWindow], has_local_claimant: bool, now: i64) -> ListedFileOrigin {
    todo!()
}

/// What follows up on made items.
#[derive(Clone)]
pub struct MadeItems {
    graph: GraphAccess,
}

impl MadeItems {
    pub fn new(graph: GraphAccess) -> Self {
        Self { graph }
    }

    /// After `RowsChanged` on `MakeDirectory` or `SendNewFile`: reads the row,
    /// [`decide_made_item_fate`], and removes what was made (`delete_item`,
    /// `G/write.rs:140`, guarded by the eTag of the answer; `delete_folder`, `:153`, for
    /// a directory: unguarded, and it takes only what this step made — a directory is
    /// adopted as the step's own only when it is empty, `lost_answers`) or asks for a
    /// cycle. Called with NO lock held: the tree lock of the
    /// commit was let go. Answers `MadeItemRemoved` for an item removed or found gone,
    /// `MadeItemLeft` for a removal OneDrive did not carry out (a 412: another changed it
    /// since; any failure), `CycleWanted` for a row that is still there. Today:
    /// `landed_away` (`upload/steps/shared.rs:663`) and `never_uploaded` (`:632`).
    pub async fn follow_up_new_item(&self, item: &ItemVersions, made: &ConfirmedItem) -> RowsChangedFollowUp {
        todo!()
    }

    /// Whether the file `shown` is about to show — new in OneDrive — is this folder's own
    /// left upload: reads the openings at its place (`ItemReader::list_opening_windows`),
    /// and only when there is one, the item's creation time from Graph (`item`,
    /// `G/mod.rs:217`), and decides ([`decide_listed_file_origin`]).
    pub async fn find_left_upload(&self, shown: &ItemVersions) -> ListedFileOrigin {
        todo!()
    }
}
