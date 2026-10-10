//! The changes a program makes to the folder, each made in the backing directory and
//! written into the item's row in the order of design §8, on the write-operation thread
//! under the folder's tree lock.
//!
//! **State owned:** none; [`ItemWriteOperations`] holds the parts an operation goes
//! through and nothing else. Nothing is "recorded for upload": the difference from the
//! synced version is the record (design §9).
//!
//! **Every operation that changes a name**, in this order:
//!
//! 1. ONE job of the store's thread reads everything the operation decides from — its
//!    case (`read_make_case`, `read_rename_case`, `read_remove_case`), with the paths —
//!    through the writer, so that it sees the thread's own last write;
//! 2. the decision, a pure function of the case, in the operation's module;
//! 3. the record: what is about to be done to the disk's names is written to the store
//!    (`ItemStore::record_place_change`, one job of the store, a transaction of its own),
//!    before the disk is touched (design §8). An operation that changes no name on disk —
//!    an item with nothing there — writes none;
//! 4. `PlaceChanges::begin`: from here a reader that opens by path waits or tries again;
//! 5. the disk, through the place change, which keeps the undo; a place the rows say is
//!    free and the disk holds is `EEXIST`, and nothing is replaced;
//! 6. the row ([`write_row_or_undo`](ItemWriteOperations::write_row_or_undo)), which takes
//!    the record in the row's own transaction
//!    (`ItemStore::settle_place_change_record`); refused or failed: the undo, and then the
//!    record taken by itself (`ItemStore::forget_place_change_record`); an undo that
//!    fails: `MountTakeDown::ask`, and the record stands for the check at start;
//! 7. the place change ended; then what was set aside removed for good
//!    (`PlaceChanges::discard`, outside the change, with no record: it went with the
//!    row); the plan woken (`WakeCause::LocalVersionWritten`).
//!
//! A daemon that dies between 3 and 6 leaves the record, and the check at start finishes
//! or undoes exactly that change from it (`backing_directory_check`'s
//! `standing_place_changes`): a made entry gets its row, a moved or set-aside entry goes
//! back where its row says it is.
//!
//! **A file's content** goes the other way (design §8): the row first — the first
//! writer's open writes `OpenedForWriting`, and then bytes go to disk with no row touched
//! per write; the last close writes the size and the times read from the backing file,
//! and only then the writer leaves the handles.
//!
//! **Tested without the rest**, over the fixture (`item_fixture`), with no mount (the
//! tests of each operation are listed in its module):
// test: a_refusal_of_the_store_is_the_errno_of_its_rule
// test: a_failed_row_is_undone_on_disk_and_the_program_gets_the_errno
// test: an_undo_that_fails_asks_for_the_mounts_take_down
// test: every_operation_wakes_the_plan_after_its_row_and_never_before
// test: a_read_only_folder_refuses_every_operation_before_anything_is_read
// test: a_reader_that_opens_by_path_during_an_operation_gets_the_file_of_its_row
#![allow(dead_code, unused_variables)]

use std::sync::Arc;

use fuser::Errno;
use konedrive_tree::{Store, TreeError, TreeStore};

use super::access_mode_switch::AccessModeSwitch;
use super::item_download_states::ItemDownloadStates;
use super::item_handles::ItemHandles;
use super::mount_take_down::MountTakeDown;
use super::place_changes::{PlaceChange, PlaceChanges, PlaceRefusal, PlaceUndo};
use super::upload_rules::UploadRules;
use crate::folder::running_steps::RunningSteps;
use crate::folder::sync_plan_wake::SyncPlanWake;

/// A file's content: open for writing, truncate, the time set, the last close.
mod content_write;
/// Make: mkdir, mknod, create.
mod make;
/// Remove: unlink, rmdir.
mod remove;
/// Rename.
mod rename;

pub(super) use content_write::FileToFill;
pub(super) use make::{MadeItem, MadeKind};
pub(super) use rename::RenameMode;

/// The errno a program gets when the store refuses a change of a local version
/// (`TreeError::Refused`) or fails. Pure. Today every failed record is `EIO`
/// (`write_operations.rs:110-115`); a refusal that a decision foresaw never reaches here,
/// so what does is a race with a step's commit: `EIO` for a failure, `ESTALE` for a row
/// that is gone, `EEXIST` for a place taken meanwhile.
pub(super) fn decide_store_errno(error: &TreeError) -> Errno {
    todo!()
}

/// The errno a program gets when the backing directory refuses a change: `EEXIST` for a
/// place the disk holds though the rows say it is free, else the disk's own. Pure.
pub(super) fn decide_place_errno(refusal: &PlaceRefusal) -> Errno {
    todo!()
}

/// The write operations of one mount.
pub(super) struct ItemWriteOperations {
    place_changes: Arc<PlaceChanges>,
    /// The writer: every read and write of an operation is a job of the store's thread.
    store: Store,
    item_handles: Arc<ItemHandles>,
    download_states: Arc<ItemDownloadStates>,
    upload_rules: Arc<UploadRules>,
    access_mode_switch: Arc<AccessModeSwitch>,
    /// Whether a step runs for an item a made file would replace.
    running_steps: Arc<RunningSteps>,
    sync_plan_wake: Arc<SyncPlanWake>,
    mount_take_down: Arc<MountTakeDown>,
}

impl ItemWriteOperations {
    /// Made by the mount's build.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        place_changes: Arc<PlaceChanges>,
        store: Store,
        item_handles: Arc<ItemHandles>,
        download_states: Arc<ItemDownloadStates>,
        upload_rules: Arc<UploadRules>,
        access_mode_switch: Arc<AccessModeSwitch>,
        running_steps: Arc<RunningSteps>,
        sync_plan_wake: Arc<SyncPlanWake>,
        mount_take_down: Arc<MountTakeDown>,
    ) -> Self {
        todo!()
    }

    /// Runs `write` as one job of the store's thread and wakes the plan; when the store
    /// refuses or fails, takes `undo` back through `change` and answers the errno of
    /// [`decide_store_errno`]; when the undo fails too, asks for the mount's take-down
    /// (which switches the folder to read-only). `write` is the row AND the going of the
    /// operation's record, in one transaction: `ItemStore::settle_place_change_record(record,
    /// &PlaceChangeRowWrite::LocalVersion(change))`, never `apply` beside a record. After an
    /// undo that went through, the record is taken by itself
    /// (`ItemStore::forget_place_change_record`); after one that failed it is left standing.
    /// On success answers `undo` back with what
    /// was written: the caller ends its change and then discards
    /// (`PlaceChanges::discard`). Called by make, rename and remove after their change of the backing
    /// directory, with their place change still open. Relies on the tree lock the
    /// thread's job holds: nothing else writes a local version in between.
    fn write_row_or_undo<T: Send + 'static>(&self, change: &PlaceChange<'_>, undo: PlaceUndo, write: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static) -> Result<(T, PlaceUndo), Errno> {
        todo!()
    }
}
