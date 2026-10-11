//! Free up space: the downloaded copy of a file removed, the row first.
//!
//! **State owned:** none.
//!
//! **Answers for:** design §9, last row — refused while the local content is not the
//! synced one or a program holds the file open for writing — and design §8's order: "the
//! row says `NotOnDisk` first, then the file goes". Today the file is removed with no row
//! touched (`files_interface.rs:230`). And the mending of a row that says on disk over a
//! file that is not there (the map's E1).
//!
//! **Tested without the rest**, a test per case of the decision, over the fixture:
// test: a_clean_copy_is_freed
// test: a_pinned_file_is_left
// test: a_file_changed_here_is_left_its_copy_is_the_only_one
// test: a_file_open_for_writing_is_left
// test: a_file_being_downloaded_or_sent_is_left
// test: a_file_not_on_disk_and_a_directory_have_nothing_to_free
// test: the_row_says_not_on_disk_before_the_copy_goes
// test: a_copy_changed_between_the_decision_and_the_commit_stays
// test: a_clean_row_that_says_on_disk_over_no_file_is_mended_to_not_on_disk
// test: a_changed_row_that_says_on_disk_over_no_file_is_left_for_the_check_at_start
#![allow(dead_code, unused_variables)]

use std::sync::Arc;

use konedrive_tree::{Item, LocalItemId, Store};

use super::free_up_decision::FreeUpDecision;
use super::item_download_states::ItemDownloadStates;
use super::item_handles::ItemHandles;
use super::place_changes::PlaceChanges;
use crate::folder::running_steps::RunningSteps;

/// What a free-up does with `item`. Pure; the first reason that holds, in the order of
/// [`FreeUpDecision`]'s cases. Replaces `free_up_decision::decide_free_up`, which takes
/// an entry's state of three sources: "changed here" is the row's
/// (`Item::file_sync_state` is `ModifiedHere` or `LocalOnly`). A file a step runs for is
/// left as one that is being downloaded is.
pub(super) fn decide_item_free_up(item: &Item, is_pinned: bool, is_downloading_or_sent: bool, is_open_for_writing: bool) -> FreeUpDecision {
    todo!()
}

/// What became of one file a free-up was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FreedCopy {
    /// The copy is gone: how many bytes it took on disk.
    Freed { bytes: u64 },
    /// It was left, and why.
    Left(FreeUpDecision),
    /// The row changed between the decision and the commit: left as it is.
    RowsChanged,
}

/// The free-up of one mount.
pub(super) struct ItemFreeUp {
    place_changes: Arc<PlaceChanges>,
    store: Store,
    item_handles: Arc<ItemHandles>,
    download_states: Arc<ItemDownloadStates>,
    running_steps: Arc<RunningSteps>,
}

impl ItemFreeUp {
    /// Made by the mount's build.
    pub(super) fn new(place_changes: Arc<PlaceChanges>, store: Store, item_handles: Arc<ItemHandles>, download_states: Arc<ItemDownloadStates>, running_steps: Arc<RunningSteps>) -> Self {
        Self { place_changes, store, item_handles, download_states, running_steps }
    }

    /// Frees the copy of the file `item`: one job of the store reads the row, its pin and
    /// its path; [`decide_item_free_up`]; `ItemStore::commit_freed_content` with the
    /// content it decided on; and only then the backing file removed, as a place change
    /// (`PlaceChange::set_aside`, its end, `PlaceChanges::discard`). A daemon that dies between the two
    /// leaves a file the row says is not on disk: the check at start reads it as the
    /// copy, landed before its row (design §10).
    ///
    /// Called by `ItemFilesInterface::free_up`, as a job of the write-operation thread,
    /// under the tree lock, once per file.
    pub(super) fn free_downloaded_copy(&self, item: LocalItemId) -> FreedCopy {
        todo!()
    }

    /// The row of `item` says its content is on disk and an open found no file there:
    /// when the row is a clean copy (its local content is the synced one) it is written
    /// not on disk (`ItemStore::commit_freed_content`), and the next open reads from
    /// OneDrive; a changed one is left as it is, for the check at start to read as a
    /// removal by hand. Whether it was mended.
    ///
    /// Called as a job of the write-operation thread by the open request that got
    /// `ENOENT` through `PlaceChanges::open_at_row_path` (which has ruled out a rename
    /// under way), before the open is tried once more.
    pub(super) fn mend_missing_copy(&self, item: LocalItemId) -> bool {
        todo!()
    }
}
