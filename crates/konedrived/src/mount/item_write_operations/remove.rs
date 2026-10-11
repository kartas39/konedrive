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
// test: each_case_of_a_removal_is_decided
// test: a_file_removed_is_set_aside_then_gone_from_its_row_then_gone_from_disk
// test: a_file_of_onedrive_removed_keeps_its_row_as_a_removal_to_send
// test: a_file_not_on_disk_is_removed_with_no_disk_change
// test: a_directory_the_folder_shows_something_in_is_not_removed
// test: a_directory_of_onedrive_with_no_directory_on_disk_is_removed_in_its_row
// test: a_file_is_not_removed_by_rmdir_and_a_directory_not_by_unlink
// test: a_row_refused_after_the_disk_puts_the_entry_back
// test: an_entry_that_cannot_be_put_back_asks_for_the_take_down_and_keeps_its_record
// test: a_directory_that_holds_what_no_row_says_stays_on_disk_unmarked_and_its_row_goes
// test: a_file_open_for_writing_is_removed_and_its_writers_close_writes_nothing
#![allow(dead_code)]

use std::ffi::OsStr;
use std::path::PathBuf;

use fuser::Errno;
use konedrive_tree::{ContentPresence, Item, ItemKind, LocalItemId, LocalVersionChange, PlaceChangeRecord, PlaceChangeRowWrite};

use super::ItemWriteOperations;
use crate::folder::disk::make_aside_entry_name;

#[cfg(test)]
mod tests;

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
    if case.directory.is_none() {
        return Err(Errno::ENOENT);
    }
    let item = case.item.as_ref().ok_or(Errno::ENOENT)?;
    match (item.kind, case.asked_kind) {
        (ItemKind::Directory, ItemKind::File) => Err(Errno::EISDIR),
        (ItemKind::File, ItemKind::Directory) => Err(Errno::ENOTDIR),
        (ItemKind::Directory, ItemKind::Directory) if !case.is_empty => Err(Errno::ENOTEMPTY),
        (ItemKind::File, ItemKind::File) if item.content_presence == ContentPresence::NotOnDisk => Ok(RemoveDecision::RowAlone { item: item.local_item_id }),
        (ItemKind::File, ItemKind::File) | (ItemKind::Directory, ItemKind::Directory) => Ok(RemoveDecision::SetAsideOnDisk { item: item.local_item_id }),
    }
}

impl ItemWriteOperations {
    /// What [`decide_remove`] decides from, read in one job of the store through the
    /// writer.
    fn read_remove_case(&self, directory: LocalItemId, name: &OsStr, asked_kind: ItemKind) -> Result<RemoveCase, Errno> {
        let name = name.to_owned();
        self.run_store_job("what is to be removed could not be read", move |store| {
            let items = store.item_store();
            let reader = items.reader();
            let Some(shown) = reader.find_item(directory)?.filter(|row| row.kind == ItemKind::Directory && row.local.is_some()) else {
                return Ok(RemoveCase { directory: None, item: None, path: None, asked_kind, is_empty: true });
            };
            let item = reader.find_item_at(directory, &name)?;
            let (path, is_empty) = match &item {
                Some(item) => (reader.find_path(item.local_item_id)?, item.kind == ItemKind::File || reader.list_children(item.local_item_id)?.is_empty()),
                None => (None, true),
            };
            Ok(RemoveCase { directory: Some(shown), item, path, asked_kind, is_empty })
        })
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
    pub(in crate::mount) fn remove(&self, directory: LocalItemId, name: &OsStr, asked_kind: ItemKind) -> Result<(), Errno> {
        self.access_mode_switch.check_read_write()?;
        let case = self.read_remove_case(directory, name, asked_kind)?;
        let removed = match decide_remove(&case)? {
            RemoveDecision::RowAlone { item } => {
                self.run_store_job("a removal could not be written", move |store| store.item_store().apply(&LocalVersionChange::Removed { item }))?;
                self.wake_plan();
                item
            }
            RemoveDecision::SetAsideOnDisk { item } => {
                let path = case.path.as_ref().ok_or(Errno::ENOENT)?;
                let place = case.item.as_ref().and_then(|row| row.local.as_ref()).and_then(|local| local.place.clone()).ok_or(Errno::ENOENT)?;
                let record = self.write_place_change_record(PlaceChangeRecord::SetAside { item, place, aside_name: make_aside_entry_name(item) })?;
                #[cfg(test)]
                self.reach(super::tests::OperationPoint::RecordWritten);
                let change = self.place_changes.begin();
                let undo = change.set_aside(path, item).map_err(|refusal| self.refuse_place_change(record, &refusal))?;
                #[cfg(test)]
                self.reach(super::tests::OperationPoint::DiskChanged);
                let ((), undo) = self.write_row_or_undo(&change, undo, record, move |store| store.item_store().settle_place_change_record(record, &PlaceChangeRowWrite::LocalVersion(LocalVersionChange::Removed { item })).map(drop))?;
                change.end();
                self.place_changes.discard(undo);
                item
            }
        };
        self.download_states.forget_item(removed);
        Ok(())
    }
}
