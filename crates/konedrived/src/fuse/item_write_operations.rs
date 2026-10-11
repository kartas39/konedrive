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
// test: a_place_held_on_disk_is_eexist_and_any_other_refusal_the_disks_own_errno
// test: a_failed_row_is_undone_on_disk_and_the_program_gets_the_errno
// test: a_death_at_each_point_of_each_operation_is_settled_by_the_check_at_start
// test: an_undo_that_fails_asks_for_the_mounts_take_down
// test: a_change_refused_with_an_entry_that_could_not_be_put_back_keeps_its_record_and_asks_for_the_take_down
// test: every_operation_wakes_the_plan_after_its_row_and_never_before
// test: a_read_only_folder_refuses_every_operation_before_anything_is_read
// test: a_reader_that_opens_by_path_during_an_operation_gets_the_file_of_its_row
#![allow(dead_code)]

use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::Errno;
use konedrive_tree::{Item, LocalItemId, PlaceChangeRecord, PlaceChangeRecordId, RowRefusalReason, Store, TreeError, TreeStore};

use super::access_mode_switch::AccessModeSwitch;
use super::backing_directory::BackingDirectory;
use super::item_download_states::ItemDownloadStates;
use super::item_handles::ItemHandles;
use super::mount_take_down::{MountTakeDown, TakeDownCause};
use super::place_changes::{PlaceChange, PlaceChanges, PlaceRefusal, PlaceUndo};
use super::read_connections::ReadConnections;
use super::upload_rules::UploadRules;
use crate::folder::running_steps::RunningSteps;
use crate::folder::sync_plan_wake::{SyncPlanWake, WakeCause};

/// A file's content: open for writing, truncate, the time set, the last close.
mod content_write;
/// Make: mkdir, mknod, create.
mod make;
/// Remove: unlink, rmdir.
mod remove;
/// Rename.
mod rename;
#[cfg(test)]
mod tests;

pub(super) use content_write::FileToFill;
pub(super) use make::{MadeItem, MadeKind};
pub(super) use rename::RenameMode;

/// The errno a program gets when the store refuses a change of a local version
/// (`TreeError::Refused`) or fails. Pure. A refusal names its reason where a program goes
/// on by the errno — `mkdir -p`, `rm -r`, `mv` and a save by rename, when another program
/// or a step changed the rows between an operation's read and its row: the item is gone
/// (`ESTALE`), the place is taken (`EEXIST`), the directory is not empty (`ENOTEMPTY`),
/// there is no such directory (`ENOENT`). Any other rule (a place deeper than the walks
/// of the rows go, a writer that holds the file) and a failure of SQLite or the disk are
/// `EIO`, as today every failed record is (`write_operations.rs:110-115`).
pub(super) fn decide_store_errno(error: &TreeError) -> Errno {
    match error {
        TreeError::Refused(refusal) => match refusal.reason {
            RowRefusalReason::ItemGone => Errno::ESTALE,
            RowRefusalReason::PlaceTaken => Errno::EEXIST,
            RowRefusalReason::DirectoryNotEmpty => Errno::ENOTEMPTY,
            RowRefusalReason::NoDirectory => Errno::ENOENT,
            RowRefusalReason::AnotherRule => Errno::EIO,
        },
        TreeError::Sql(_) | TreeError::Io(_) | TreeError::Schema(_) => Errno::EIO,
    }
}

/// The errno a program gets when the backing directory refuses a change: `EEXIST` for a
/// place the disk holds though the rows say it is free, `EIO` when the change left an
/// entry set aside that could not be put back, else the disk's own. Pure.
pub(super) fn decide_place_errno(refusal: &PlaceRefusal) -> Errno {
    match refusal {
        PlaceRefusal::PlaceHeld { .. } => Errno::EEXIST,
        PlaceRefusal::Disk { errno, .. } => Errno::from_i32(*errno),
        PlaceRefusal::NotPutBack { .. } => Errno::EIO,
    }
}

/// The path a take-down names when `undo` could not be taken back: where the disk has
/// what no row says.
fn find_undone_path(undo: &PlaceUndo) -> PathBuf {
    match undo {
        PlaceUndo::Nothing => PathBuf::new(),
        PlaceUndo::RemoveMade { path, .. } | PlaceUndo::UnmarkTaken { path } | PlaceUndo::Restore { path, .. } | PlaceUndo::RestoreMark { path, .. } => path.clone(),
        PlaceUndo::MoveBack { to, .. } | PlaceUndo::MoveBackAndRestore { to, .. } => to.clone(),
    }
}

/// The size and the modification time of `file`, as a row keeps them.
fn read_size_and_time(file: &File) -> io::Result<(u64, i64, u32)> {
    let metadata = file.metadata()?;
    Ok((metadata.size(), metadata.mtime(), metadata.mtime_nsec() as u32))
}

/// The time a row keeps as `mtime` and `mtime_nsec`, as the disk takes it.
fn to_system_time(mtime: i64, mtime_nsec: u32) -> SystemTime {
    match u64::try_from(mtime) {
        Ok(seconds) => UNIX_EPOCH + Duration::new(seconds, mtime_nsec),
        Err(_) => UNIX_EPOCH - Duration::from_secs(mtime.unsigned_abs()) + Duration::from_nanos(mtime_nsec.into()),
    }
}

/// The write operations of one mount.
pub(super) struct ItemWriteOperations {
    place_changes: Arc<PlaceChanges>,
    /// For what the write-operation thread does to a backing file by the path of its row,
    /// under the tree lock, where no place change runs beside it: the open of a file that
    /// is on disk, its write bit, its marks, and whether a directory is on disk. No name
    /// is changed through it: that is `place_changes`' alone.
    backing_directory: Arc<BackingDirectory>,
    /// The writer: every read and write of an operation is a job of the store's thread.
    store: Store,
    /// For the one read made on a thread of the mount, before a job is queued
    /// (`find_file_to_fill`): through that thread's own connection, so that it waits for
    /// neither the store's thread nor its one reading thread.
    read_connections: Arc<ReadConnections>,
    item_handles: Arc<ItemHandles>,
    download_states: Arc<ItemDownloadStates>,
    upload_rules: Arc<UploadRules>,
    access_mode_switch: Arc<AccessModeSwitch>,
    /// Whether a step runs for an item a made file would replace.
    running_steps: Arc<RunningSteps>,
    sync_plan_wake: Arc<SyncPlanWake>,
    mount_take_down: Arc<MountTakeDown>,
    /// What a test has run at the points of an operation between its record and its row.
    #[cfg(test)]
    operation_point_hook: tests::OperationPointHook,
}

impl ItemWriteOperations {
    /// Made by the mount's build.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        place_changes: Arc<PlaceChanges>,
        backing_directory: Arc<BackingDirectory>,
        store: Store,
        read_connections: Arc<ReadConnections>,
        item_handles: Arc<ItemHandles>,
        download_states: Arc<ItemDownloadStates>,
        upload_rules: Arc<UploadRules>,
        access_mode_switch: Arc<AccessModeSwitch>,
        running_steps: Arc<RunningSteps>,
        sync_plan_wake: Arc<SyncPlanWake>,
        mount_take_down: Arc<MountTakeDown>,
    ) -> Self {
        Self {
            place_changes,
            backing_directory,
            store,
            read_connections,
            item_handles,
            download_states,
            upload_rules,
            access_mode_switch,
            running_steps,
            sync_plan_wake,
            mount_take_down,
            #[cfg(test)]
            operation_point_hook: tests::OperationPointHook::default(),
        }
    }

    /// Runs `job` as one job of the store's thread and waits for it; a refusal or a
    /// failure is logged as `what` and answered as the errno of [`decide_store_errno`].
    fn run_store_job<T: Send + 'static>(&self, what: &str, job: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static) -> Result<T, Errno> {
        self.store.call_blocking(job).map_err(|error| {
            tracing::warn!("{what}: {error}");
            decide_store_errno(&error)
        })
    }

    /// Writes `record` to the store, in a job and a transaction of its own: what an
    /// operation is about to do to the disk's names, before it begins its place change.
    fn write_place_change_record(&self, record: PlaceChangeRecord) -> Result<PlaceChangeRecordId, Errno> {
        self.run_store_job("the record of a change of the backing directory could not be written", move |store| store.item_store().record_place_change(&record))
    }

    /// Takes `record` by itself: its change left the disk as it was. A failure is logged:
    /// the record then stands, and the check at start finds nothing left of its change.
    fn forget_place_change_record(&self, record: PlaceChangeRecordId) {
        let _ = self.run_store_job("the record of a change of the backing directory that was not made could not be taken", move |store| store.item_store().forget_place_change_record(record));
    }

    /// The backing directory refused the change of `record`. Nothing of it is on disk:
    /// the record is taken, and the errno is that of [`decide_place_errno`]. An entry it
    /// had set aside could not be put back (`PlaceRefusal::NotPutBack`): the record
    /// stands, for the check at start to put the entry back from, the mount's take-down
    /// is asked for, and the errno is `EIO` — as after an undo that failed.
    fn refuse_place_change(&self, record: PlaceChangeRecordId, refusal: &PlaceRefusal) -> Errno {
        match refusal {
            PlaceRefusal::PlaceHeld { .. } | PlaceRefusal::Disk { .. } => self.forget_place_change_record(record),
            PlaceRefusal::NotPutBack { path, errno } => {
                tracing::error!("{} was set aside in the backing directory for a change that failed (errno {errno}) and could not be put back; the mount is taken down", path.display());
                self.mount_take_down.ask(TakeDownCause::UndoFailed { path: path.clone() });
            }
        }
        decide_place_errno(refusal)
    }

    /// The plan is made again: a local version was written.
    fn wake_plan(&self) {
        self.sync_plan_wake.wake(WakeCause::LocalVersionWritten);
    }

    /// Runs `write` as one job of the store's thread and wakes the plan; when the store
    /// refuses or fails, takes `undo` back through `change` and answers the errno of
    /// [`decide_store_errno`]; when the undo fails too, asks for the mount's take-down
    /// (which switches the folder to read-only). `write` is the row AND the going of the
    /// operation's record, in one transaction: `ItemStore::settle_place_change_record(record,
    /// &PlaceChangeRowWrite::LocalVersion(change))`, never `apply` beside a record. After an
    /// undo that went through, `record` is taken by itself
    /// (`ItemStore::forget_place_change_record`); after one that failed it is left standing.
    /// On success answers `undo` back with what
    /// was written: the caller ends its change and then discards
    /// (`PlaceChanges::discard`). Called by make, rename and remove after their change of the backing
    /// directory, with their place change still open. Relies on the tree lock the
    /// thread's job holds: nothing else writes a local version in between.
    fn write_row_or_undo<T: Send + 'static>(&self, change: &PlaceChange<'_>, undo: PlaceUndo, record: PlaceChangeRecordId, write: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static) -> Result<(T, PlaceUndo), Errno> {
        let errno = match self.run_store_job("the row of a change of the backing directory could not be written, and the change is taken back", write) {
            Ok(written) => {
                self.wake_plan();
                #[cfg(test)]
                self.reach(tests::OperationPoint::RowWritten);
                return Ok((written, undo));
            }
            Err(errno) => errno,
        };
        Err(self.undo_place_change(change, undo, record, errno))
    }

    /// Takes `undo` back through `change`, whose row is not written, and then `record` by
    /// itself: the errno the program gets is `errno`, what the change failed with. An undo
    /// that fails asks for the mount's take-down, leaves the record for the check at
    /// start, and is `EIO`.
    fn undo_place_change(&self, change: &PlaceChange<'_>, undo: PlaceUndo, record: PlaceChangeRecordId, errno: Errno) -> Errno {
        let path = find_undone_path(&undo);
        match change.undo(undo) {
            Ok(()) => {
                self.forget_place_change_record(record);
                errno
            }
            Err(error) => {
                tracing::error!("{} could not be put back in the backing directory as its row says: {error}; the mount is taken down", path.display());
                self.mount_take_down.ask(TakeDownCause::UndoFailed { path });
                Errno::EIO
            }
        }
    }

}
