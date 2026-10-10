//! Remove: unlink and rmdir. The disk first, the row second (design §8) — the other way
//! round from today, which records first (`write_operations/remove.rs:26-36`).
//!
//! **Decides:** [`decide_remove`]. **Writes:** `LocalVersionChange::Removed`: a row with
//! no binding goes; a bound one keeps its binding and what was agreed on, which is the
//! unsent removal.
//!
//! So that the disk change can be undone, the entry is first set aside
//! (`PlaceChange::set_aside`), and removed for good only after the row is written.
//!
//! **On record before the disk:** `PlaceChangeRecord::SetAside { item, place, aside_name }`,
//! `aside_name` being `folder::disk::make_aside_entry_name(item)`. `RowAlone` changes no
//! name on disk and writes no record. The row takes the record
//! (`settle_place_change_record` with the `Removed`); the removal for good comes after,
//! with no record. A death between the set-aside and the row: the check at start puts the
//! entry back under its name, and nothing is removed, here or in OneDrive. So the entries
//! that were set aside must still be there when the check runs
//! (`PlaceChanges::remove_aside_entries` after it, never before).
//!
//! **Tested without the rest:**
// test: a_file_removed_is_set_aside_then_gone_from_its_row_then_gone_from_disk
// test: a_file_not_on_disk_is_removed_with_no_disk_change
// test: a_directory_the_folder_shows_something_in_is_not_removed
// test: a_directory_of_onedrive_with_no_directory_on_disk_is_removed_in_its_row
// test: a_file_is_not_removed_by_rmdir_and_a_directory_not_by_unlink
// test: a_row_refused_after_the_disk_puts_the_entry_back
// test: a_file_open_for_writing_is_removed_and_its_writers_close_writes_nothing
#![allow(dead_code, unused_variables)]

use std::ffi::OsStr;
use std::path::PathBuf;

use fuser::Errno;
use konedrive_tree::{Item, ItemKind, LocalItemId};

use super::ItemWriteOperations;

/// What the rows say of what is to be removed: read in one job of the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RemoveCase {
    pub(super) directory: Option<Item>,
    pub(super) item: Option<Item>,
    /// The path of the item in the backing directory.
    pub(super) path: Option<PathBuf>,
    /// What the request removes: unlink a file, rmdir a directory.
    pub(super) asked_kind: ItemKind,
    /// Whether the folder shows something in the directory to be removed.
    pub(super) is_empty: bool,
}

/// What a removal that goes ahead does on disk before the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemoveDecision {
    /// Something of the item may be in the backing directory: it is set aside, then
    /// removed (a directory only when it is empty on disk: what the mount does not show
    /// in it keeps it there, and is no error).
    SetAsideOnDisk { item: LocalItemId },
    /// Nothing of the item is on disk (a file that is not on disk): the row alone.
    RowAlone { item: LocalItemId },
}

/// Whether the removal goes ahead and how; the errno otherwise, in today's order
/// (`write_operations/remove_plan.rs:37-53`): `ENOENT`, `EISDIR`, `ENOTDIR`, `ENOTEMPTY`.
/// Pure.
pub(super) fn decide_remove(case: &RemoveCase) -> Result<RemoveDecision, Errno> {
    todo!()
}

impl ItemWriteOperations {
    /// What [`decide_remove`] decides from, read in one job of the store through the
    /// writer.
    fn read_remove_case(&self, directory: LocalItemId, name: &OsStr, asked_kind: ItemKind) -> Result<RemoveCase, Errno> {
        todo!()
    }

    /// Unlink and rmdir: removes `name` from `directory`.
    ///
    /// `read_remove_case`; [`decide_remove`]; the record written
    /// (`ItemStore::record_place_change(SetAside { item, place, aside_name })`; none for
    /// `RowAlone`); a place change begun; the entry set aside;
    /// `settle_place_change_record(record, LocalVersion(Removed { item }))` through
    /// `write_row_or_undo` (undo: `PlaceUndo::Restore`, or `RestoreMark` for a directory
    /// that holds what no row says and stays on disk, and then
    /// `forget_place_change_record`); the change ended; `PlaceChanges::discard`. A writer that still holds
    /// the file keeps its descriptor; its last close finds no shown row and writes
    /// nothing. What the download states keep of the item is forgotten.
    ///
    /// Called by the kernel's unlink and rmdir, on the write-operation thread.
    pub(in crate::fuse) fn remove(&self, directory: LocalItemId, name: &OsStr, asked_kind: ItemKind) -> Result<(), Errno> {
        todo!()
    }
}
