//! Remove: unlink and rmdir. The disk first, the row second (design §8) — the other way
//! round from today, which records first (`write_operations/remove.rs:26-36`).
//!
//! **Decides:** `decide_remove`. **Writes:** `LocalVersionChange::Removed`: a row with
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
use konedrive_tree::{ContentPresence, Item, ItemKind, ItemsNeverShown, LocalItemId, LocalVersionChange, PlaceChangeRecord, PlaceChangeRowWrite};

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
    /// What OneDrive has in the directory to be removed, at any depth, that the folder
    /// never showed (`ItemReader::find_onedrive_items_never_shown_below`): items it
    /// cannot show (a name too long, a OneNote notebook…), or items a listing brought
    /// that no step has shown yet. Either way the directory is not removed here: `rmdir`
    /// answers `ENOTEMPTY`, as for one the folder shows something in — the shown files
    /// in it were removed as asked, each by its own unlink, and those removals are none
    /// of this. `Nothing` for a file.
    pub(super) onedrive_items_never_shown: ItemsNeverShown,
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
/// `ENOTEMPTY` also for a directory the folder shows nothing in while OneDrive has in it
/// what the folder never showed (`RemoveCase::onedrive_items_never_shown` is not
/// `Nothing`). For `CannotBeShown`, and for it alone, the caller then tells the user why
/// (`ItemWriteOperations::tell_directory_not_removable`): of items that are only not
/// shown yet, "cannot be shown here" would be false, and they appear by themselves. Pure.
pub(super) fn decide_remove(case: &RemoveCase) -> Result<RemoveDecision, Errno> {
    if case.directory.is_none() {
        return Err(Errno::ENOENT);
    }
    let item = case.item.as_ref().ok_or(Errno::ENOENT)?;
    match (item.kind, case.asked_kind) {
        (ItemKind::Directory, ItemKind::File) => Err(Errno::EISDIR),
        (ItemKind::File, ItemKind::Directory) => Err(Errno::ENOTDIR),
        (ItemKind::Directory, ItemKind::Directory) if !case.is_empty => Err(Errno::ENOTEMPTY),
        // OneDrive has in it what the folder never showed: not removed here either.
        (ItemKind::Directory, ItemKind::Directory) if case.onedrive_items_never_shown != ItemsNeverShown::Nothing => Err(Errno::ENOTEMPTY),
        (ItemKind::File, ItemKind::File) if item.content_presence == ContentPresence::NotOnDisk => Ok(RemoveDecision::RowAlone { item: item.local_item_id }),
        (ItemKind::File, ItemKind::File) | (ItemKind::Directory, ItemKind::Directory) => Ok(RemoveDecision::SetAsideOnDisk { item: item.local_item_id }),
    }
}

impl ItemWriteOperations {
    /// Tells the user that the directory `directory` cannot be removed here because it
    /// holds in OneDrive items that cannot be shown here, and that it is to be removed in
    /// OneDrive: `rmdir` itself carries only the errno. One event of the activity log
    /// (`ActivityKind::NotRemovedHere`, the directory's full path), recorded and announced
    /// through the mount's report (`Activity::record_blocking`: `ActivityLog.Added`,
    /// which the tray shows as a notification) — at most once per directory while the
    /// mount is up, so that a program that tries again says it once. The items themselves
    /// are in the bus's list of what is not shown (`Folder.Skipped()`,
    /// `upload_queue::read_items_not_shown`), one row each. Called by
    /// [`remove`](Self::remove), on the write-operation thread, after the decision and
    /// before the errno is answered; a failure of it is a warning and changes no answer.
    fn tell_directory_not_removable(&self, directory: LocalItemId) {
        // Once per directory in a run of the mount: a program that tries again is not told again. Asked here and kept only once the event was handed over (this thread alone tells, so no second telling comes between): a telling that failed is tried at the next `rmdir`.
        if crate::panic::lock(&self.directories_told_not_removable).contains(&directory) {
            return;
        }
        let path = match self.store.call_blocking(move |store| store.item_store().reader().find_path(directory)) {
            Ok(Some(path)) => path,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!("cannot say that directory {directory} cannot be removed here: {error}");
                return;
            }
        };
        let event = crate::status::activity::event(konedrive_tree::ActivityKind::NotRemovedHere, self.folder.join(path).display().to_string(), "");
        self.report.activity.record_blocking(vec![event]);
        crate::panic::lock(&self.directories_told_not_removable).insert(directory);
    }

    /// What [`decide_remove`] decides from, read in one job of the store through the
    /// writer.
    fn read_remove_case(&self, directory: LocalItemId, name: &OsStr, asked_kind: ItemKind) -> Result<RemoveCase, Errno> {
        let name = name.to_owned();
        self.run_store_job("what is to be removed could not be read", move |store| {
            let items = store.item_store();
            let reader = items.reader();
            let Some(shown) = reader.find_item(directory)?.filter(|row| row.kind == ItemKind::Directory && row.local.is_some()) else {
                return Ok(RemoveCase { directory: None, item: None, path: None, asked_kind, is_empty: true, onedrive_items_never_shown: ItemsNeverShown::Nothing });
            };
            let item = reader.find_item_at(directory, &name)?;
            let (path, is_empty) = match &item {
                Some(item) => (reader.find_path(item.local_item_id)?, item.kind == ItemKind::File || reader.list_children(item.local_item_id)?.is_empty()),
                None => (None, true),
            };
            // Only a directory the folder shows nothing in is asked what OneDrive has in it that was never shown.
            let onedrive_items_never_shown = match &item {
                Some(item) if item.kind == ItemKind::Directory && is_empty => reader.find_onedrive_items_never_shown_below(item.local_item_id)?,
                _ => ItemsNeverShown::Nothing,
            };
            Ok(RemoveCase { directory: Some(shown), item, path, asked_kind, is_empty, onedrive_items_never_shown })
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
    /// A `rmdir` refused because OneDrive has in the directory what the folder cannot
    /// show (`ItemsNeverShown::CannotBeShown`) tells the user why before it answers `ENOTEMPTY`
    /// ([`tell_directory_not_removable`](Self::tell_directory_not_removable)).
    ///
    /// Called by the kernel's unlink and rmdir, on the write-operation thread.
    pub(in crate::mount) fn remove(&self, directory: LocalItemId, name: &OsStr, asked_kind: ItemKind) -> Result<(), Errno> {
        self.access_mode_switch.check_read_write()?;
        let case = self.read_remove_case(directory, name, asked_kind)?;
        let decision = decide_remove(&case);
        // An errno says nothing of why: a directory that holds what cannot be shown here is said to the user.
        // Of an `rmdir` alone: an `unlink` of such a directory is refused as any directory's (`EISDIR`), and that says why.
        if let (Err(_), ItemKind::Directory, true, ItemsNeverShown::CannotBeShown, Some(directory)) = (&decision, case.asked_kind, case.is_empty, case.onedrive_items_never_shown, &case.item) {
            self.tell_directory_not_removable(directory.local_item_id);
        }
        let removed = match decision? {
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
