//! The runner of the sync plan's steps towards the folder: one function per case of
//! `FolderStep`, each a change of the backing directory and its commit, in the order of
//! design §8, with its undo.
//!
//! **State owned:** none; [`FolderSteps`] holds the parts a step goes through. Every step
//! but a download's fetching runs as one job of the write-operation thread, under the
//! folder's tree lock, as a write operation of the mount does (design §4): no program
//! changes the folder between a step's change of the disk and its row. So steps towards
//! the folder need no limit of their own: the thread runs one job at a time.
//!
//! **Every step, in this order:**
//!
//! 1. ONE job of the store reads the item's row, its path and what the step needs of
//!    other rows, through the writer; the versions the step was planned from must still
//!    be the row's, and no program may hold the file open for writing — asked before the
//!    disk is touched, so that what is left to race with the commit is a listing's page
//!    alone, which does not take the tree lock;
//! 2. the item's former place is taken from that row, for the announcement;
//! 3. the record of what is about to be done to the disk's names
//!    (`ItemStore::record_place_change`, a transaction of its own; none for a step that
//!    changes no name on disk: `ShowItem`, an item with nothing on disk); then
//!    `PlaceChanges::begin`, then the backing directory changed through the place
//!    change, which keeps the undo and replaces nothing the rows do not say;
//! 4. its commit of `step_commit`, with whether the item's new name is ignored
//!    (`UploadRules::is_name_ignored`) where the commit asks, and with the record's id
//!    as its last argument (`record: Option<PlaceChangeRecordId>`, `None` for a step that
//!    wrote none): a commit that writes takes the record in its own transaction; one that
//!    answers `RowsChanged` leaves it;
//! 5. on `RowsChanged` the undo, then the record taken by itself
//!    (`ItemStore::forget_place_change_record`), and `StepOutcome::RowsChanged(Nothing)`;
//!    an undo that fails asks for the mount's take-down, and the record stands for the
//!    check at start; the place change ended; and only then, on `Written`, what was set
//!    aside removed for good (`PlaceChanges::discard`, outside the change: no opener
//!    waits for an unlink);
//! 6. the change announced through the bridge by item and former name; what the download
//!    states and the range fetcher keep of an item that left or changed content
//!    forgotten; what happened answered for the activity log.
//!
//! | Case | Record | Disk | Commit | Order |
//! |---|---|---|---|---|
//! | `ShowItem` | none | nothing | `commit_show_items` | the commit alone, several in one job |
//! | `MoveItem` | `MoveEntry` | move to OneDrive's place | `commit_move_item` | record, disk, commit |
//! | `TakeMoveBack` | `MoveEntry` | move to OneDrive's place | `commit_take_move_back` | record, disk, commit |
//! | `MoveToTemporaryName` | `MoveEntry` | move to the temporary name | `commit_move_to_temporary_name` | record, disk, commit |
//! | `RenameToConflictCopy` | `MoveEntry` | move to the copy's name | `commit_rename_to_conflict_copy_on_record` | record, disk, commit |
//! | `RemoveItem` | `SetAside` | the entry set aside, then removed | `commit_remove_item` | record, set aside, commit, remove |
//! | `TakeOneDriveContent` | `SetAside` | the copy set aside, then removed | `commit_take_onedrive_content` | record, set aside, commit, remove |
//! | `DownloadContent` | `LinkFile` | the file landed | `commit_download_content` | synced, record, named, directory synced, commit |
//!
//! The record is the same for a step and for a program's operation, and does not say whose
//! it is. So the check at start never writes a step's row from it: a move or a set-aside
//! that died before its commit is undone on disk, the rows are as they were, and the plan
//! plans the step again.
//!
//! `TakeOneDriveContent` is NOT "commit, then disk", as a free-up is: its copy is of the
//! OLD content. A death after the commit and before the disk would leave that copy under
//! a row that says not on disk with the NEW content, which the check at start reads as
//! content the user put there, and the old content would be sent over OneDrive's new one.
//! Set aside first and on record, a death before the commit leaves the old row and the
//! record: the check at start puts the old copy back under its name, and the plan takes
//! OneDrive's content again. A death after the commit leaves the new row, which says not
//! on disk, and no record: the copy that was set aside is not put back
//! (`AsideRow::Otherwise`) and goes with the cleaning of what was set aside.
//!
//! **Tested without the rest**, over the fixture (`item_fixture`), with no mount and no
//! OneDrive; and by the races' test of design §12, which puts a failure and a death at
//! every point between a disk change and its commit:
// test: each_case_changes_the_disk_and_the_rows_as_its_line_of_the_table_says
// test: a_step_planned_from_other_versions_than_the_rows_touches_nothing
// test: a_step_for_a_file_a_program_opened_for_writing_since_touches_nothing
// test: a_commit_that_finds_the_rows_changed_undoes_the_disk
// test: an_undo_that_fails_asks_for_the_mounts_take_down
// test: an_item_with_nothing_on_disk_is_moved_in_its_row_alone
// test: a_moved_directory_gets_its_directories_above_made_and_marked
// test: a_place_held_on_disk_by_what_no_row_says_stops_the_step_and_is_said
// test: a_removed_file_is_put_back_when_its_commit_is_refused
// test: a_directory_that_leaves_is_removed_on_disk_only_when_it_is_empty_there
// test: a_death_after_the_copy_of_a_file_that_takes_onedrives_content_was_set_aside_is_not_read_as_an_edit
// test: a_conflict_copy_is_on_record_with_its_commit
// test: the_items_of_several_show_steps_are_committed_in_one_job_each_by_its_own_versions
// test: every_step_announces_the_former_name_and_the_new_one
// test: a_death_between_the_disk_and_the_commit_is_undone_by_the_check_at_start_from_the_steps_record
// test: a_commit_that_writes_takes_the_steps_record_and_one_that_finds_the_rows_changed_leaves_it
#![allow(dead_code, unused_variables)]

use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;

use konedrive_tree::{FolderStep, Item, ItemPlace, ItemVersions, Store};

use super::item_download_states::ItemDownloadStates;
use super::item_downloader::ItemDownloader;
use super::item_handles::ItemHandles;
use super::item_inotify_bridge::ItemInotifyBridge;
use super::item_range_fetcher::ItemRangeFetcher;
use super::mount_take_down::MountTakeDown;
use super::place_changes::{PlaceChange, PlaceChanges, PlaceUndo};
use super::upload_rules::UploadRules;
use super::write_operation_thread::WriteOperationThread;
use crate::folder::step_outcome::StepOutcome;

/// Why a step does not touch the folder, asked before the disk is changed; `None` when it
/// may go on. Pure. `row` is the item's row as it is now.
pub(super) fn decide_folder_step_obstacle(item: &ItemVersions, row: Option<&Item>, is_open_for_writing: bool) -> Option<StepOutcome> {
    todo!()
}

/// The runner of the steps towards the folder, for one mount.
pub struct FolderSteps {
    place_changes: Arc<PlaceChanges>,
    /// The writer: a step's reads and its commit are jobs of the store's thread.
    store: Store,
    write_operation_thread: Arc<WriteOperationThread>,
    upload_rules: Arc<UploadRules>,
    item_handles: Arc<ItemHandles>,
    inotify_bridge: Arc<ItemInotifyBridge>,
    downloader: Arc<ItemDownloader>,
    download_states: Arc<ItemDownloadStates>,
    range_fetcher: Arc<ItemRangeFetcher>,
    mount_take_down: Arc<MountTakeDown>,
    /// The folder's path, which is the mount: a conflict's record holds full paths.
    folder: PathBuf,
}

impl FolderSteps {
    /// Made by the mount's build, after the downloader and the bridge.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        place_changes: Arc<PlaceChanges>,
        store: Store,
        write_operation_thread: Arc<WriteOperationThread>,
        upload_rules: Arc<UploadRules>,
        item_handles: Arc<ItemHandles>,
        inotify_bridge: Arc<ItemInotifyBridge>,
        downloader: Arc<ItemDownloader>,
        download_states: Arc<ItemDownloadStates>,
        range_fetcher: Arc<ItemRangeFetcher>,
        mount_take_down: Arc<MountTakeDown>,
        folder: PathBuf,
    ) -> Self {
        todo!()
    }

    /// Runs `step` and answers what came of it: total over the cases. A `ShowItem` goes
    /// through [`run_show_items`](Self::run_show_items) with the one item, a
    /// `DownloadContent` through [`run_download_content`](Self::run_download_content);
    /// every other case is one job of the write-operation thread. Called by the plan's
    /// task (`sync::sync_plan_task`), which holds the folder's lease while it runs (and
    /// calls the two named functions itself for the show steps of a plan and for a
    /// download). A job dropped because the thread stopped, and any step after a
    /// take-down was asked for, is `StepOutcome::NotRun(SendingStopped)`.
    pub async fn run(self: &Arc<Self>, step: FolderStep) -> StepOutcome {
        todo!()
    }

    /// `ShowItem`, for every such step of one plan: the items of OneDrive the folder does
    /// not show appear at OneDrive's places, their content not on disk. Nothing on disk:
    /// a directory of OneDrive needs none, and a file's copy is not there. One job of the
    /// write-operation thread, in it one job of the store
    /// (`ItemStore::commit_show_items`, each item with whether its name is ignored, a
    /// directory before what is in it); each answered by itself, in the order given;
    /// `Appeared` announced for those written. Design §6, "new in OneDrive". A first
    /// listing of N items is a few jobs, not N.
    pub async fn run_show_items(self: &Arc<Self>, items: Vec<ItemVersions>) -> Vec<StepOutcome> {
        todo!()
    }

    /// `DownloadContent`: the content of a pinned file that is not on disk is downloaded.
    /// Not a job of the write-operation thread while it fetches: the download runs on the
    /// runtime in the pool's download slot, which the downloader takes
    /// (`ItemDownloader::download_and_land` with the step's versions; a download of the
    /// same content that runs already is joined), and only its landing is such a job
    /// (`ItemDownloadLanding::land`, which calls `commit_download_content`). The source's
    /// refusal is answered as `StepRefusal::{SignedOut, Throttled, ItemGone, Transient}`.
    /// When this future is dropped (the step's task aborted), a guard cancels the
    /// download (`ItemDownloader::cancel`: aborted when the plan alone asked for it). The plan's task runs it as its own class of step
    /// (`StepClass::Download`), held by the pause and the sign-out, never holding back
    /// the other steps towards the folder. Replaces `pinned_file_downloader.rs`.
    pub async fn run_download_content(self: &Arc<Self>, item: ItemVersions) -> StepOutcome {
        todo!()
    }

    /// `MoveItem`: the item is moved here to OneDrive's place. `move_on_disk`, then
    /// `commit_move_item(item, is_name_ignored, record)`. Announces the former name vanishing and
    /// the new one appearing. Replaces the cycle step's rename (`cycle_step.rs:210`) and
    /// the upload worker's `follow_cloud` (`upload/steps/shared.rs:547`).
    fn run_move_item(&self, item: &ItemVersions) -> StepOutcome {
        todo!()
    }

    /// `TakeMoveBack`: a move made here is taken back to OneDrive's place. As
    /// [`run_move_item`](Self::run_move_item), with `commit_take_move_back`. Design §6, "a
    /// cycle of moves".
    fn run_take_move_back(&self, item: &ItemVersions) -> StepOutcome {
        todo!()
    }

    /// `MoveToTemporaryName`: the item is moved here to `place`, a temporary name. As
    /// [`run_move_item`](Self::run_move_item) to `place`, with
    /// `commit_move_to_temporary_name(item, place, is_name_ignored, record)`: a temporary name
    /// (`.konedrive-swap-<id>`) is the row's place, with its file under that name on
    /// disk; the mount hides it (`folder::disk::is_hidden_name`), and the check at start
    /// reads it as that row's — it is no entry set aside.
    fn run_move_to_temporary_name(&self, item: &ItemVersions, place: &ItemPlace) -> StepOutcome {
        todo!()
    }

    /// `RenameToConflictCopy`: the user's own item is renamed here to `name` in the
    /// directory it is in. `move_on_disk`, then
    /// `commit_rename_to_conflict_copy_on_record(item, name, is_name_ignored, conflict, record)`
    /// with the two full paths. Replaces `upload/steps/shared.rs:389` (`copy`), `:454`
    /// (`move_to_copy_name`) and `conflict_copies.rs:106` (`move_aside`). The name was
    /// chosen free on both sides by the plan; one that is held on disk by what no row
    /// says is `StepObstacle::PlaceHeldOnDisk`.
    fn run_rename_to_conflict_copy(&self, item: &ItemVersions, name: &OsStr) -> StepOutcome {
        todo!()
    }

    /// `RemoveItem`: the folder shows the item no more. On record (`SetAside { item, place,
    /// aside_name }`), then `PlaceChange::set_aside` of what it has on disk, then
    /// `commit_remove_item`, which takes the record; undo: restore (the entry, or the mark
    /// of a directory that holds what no row says and stays), and the record taken by
    /// itself; after the change ended, `PlaceChanges::discard`.
    /// Announces the name vanishing; the download states and the ranges of the item are
    /// forgotten.
    fn run_remove_item(&self, item: &ItemVersions) -> StepOutcome {
        todo!()
    }

    /// `TakeOneDriveContent`: the file shows OneDrive's content from now on. On record
    /// (`SetAside { item, place, aside_name }`), the copy of the old content set aside
    /// (`PlaceChange::set_aside`), then `commit_take_onedrive_content`, which takes the
    /// record; undo: restore, and the record taken by itself; after the change ended,
    /// `PlaceChanges::discard` (see the module's doc for why not the commit first). Announces the content changed; the ranges the
    /// content source remembers of the item (`ItemRangeFetcher::forget_ranges`; today the
    /// cycle step, for every cycle: `cycle_step.rs:126`) and its download states are
    /// forgotten.
    fn run_take_onedrive_content(&self, item: &ItemVersions) -> StepOutcome {
        todo!()
    }

    /// Moves what `item` has on disk from the path of its local version to the path of
    /// `place` (`PlaceChange::move_entry`, with the directories above made and marked by
    /// what `ItemReader::find_path_with_marks` of the directory answered). The caller wrote
    /// the record before it began `change` (`MoveEntry { item, from: the item's local
    /// place, to: place }`; none for an item with nothing on disk), and gives the record's
    /// id to its commit.
    /// Answers how to take it back; `PlaceUndo::Nothing` for an item with nothing on
    /// disk; `StepObstacle::PlaceHeldOnDisk` as the outcome for a held place. Called by
    /// the four cases that move.
    fn move_on_disk(&self, change: &PlaceChange<'_>, item: &ItemVersions, place: &ItemPlace) -> Result<PlaceUndo, StepOutcome> {
        todo!()
    }

    /// Takes `undo` back through `change`, and then the step's record by itself
    /// (`ItemStore::forget_place_change_record`); when the undo fails, asks for the mount's
    /// take-down, leaves the record standing and answers `StepFailure::UndoFailed`. Called
    /// after a commit answered `RowsChanged` or failed.
    fn undo_on_disk(&self, change: &PlaceChange<'_>, undo: PlaceUndo) -> Result<(), StepOutcome> {
        todo!()
    }
}
