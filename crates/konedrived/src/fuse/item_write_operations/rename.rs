//! Rename. The disk first, the row second (design §8).
//!
//! **Decides:** [`decide_rename`], a pure function of the two places as the rows say
//! them. **Writes:** `LocalVersionChange::Moved` where nothing is at the new place,
//! `SavedOver` over an entry (a file saved over a file takes its binding over: design
//! §5). What is below a renamed directory needs no write: its rows name their parent by
//! id.
//!
//! **On record before the disk:** `PlaceChangeRecord::MoveEntry { item, from, to }` for a
//! move to a place nothing is at; `MoveEntryOver { item, from, to, replaced, aside_name }`
//! over an entry, `aside_name` being `folder::disk::make_aside_entry_name(replaced)`, the
//! name `PlaceChange::move_entry_over` sets the displaced entry aside to. The places are
//! the rows' (`ItemPlace`: the directory's local item id and the name), `from` the moved
//! item's local place as it was read. An item with nothing on disk changes no name on disk
//! and writes no record — unless it is moved over an entry that is on disk, which is then
//! set aside by itself: on record as `SetAside { item: replaced, place: to, aside_name }`. The row takes the record (`settle_place_change_record` with the
//! `Moved` or the `SavedOver`). A death between the disk and the row: the check at start
//! moves the entry back, and puts the displaced one back under its name; no row changes,
//! and the rename did not happen.
//!
//! **Tested without the rest:**
// test: each_case_of_a_rename_is_decided
// test: each_case_of_what_a_rename_does_on_disk_is_decided
// test: an_item_renamed_to_a_free_name_is_moved_on_disk_and_then_in_its_row
// test: a_directory_renamed_takes_what_is_in_it_with_one_row_written
// test: a_file_saved_over_another_takes_its_binding_and_the_other_row_goes
// test: a_file_not_on_disk_is_renamed_with_no_disk_change
// test: a_directory_is_not_renamed_into_what_is_below_it
// test: a_directory_over_a_directory_that_is_not_empty_is_refused
// test: a_file_over_a_directory_and_a_directory_over_a_file_are_refused
// test: a_rename_to_a_name_of_the_daemons_own_is_refused
// test: an_exchange_of_two_names_is_refused
// test: an_item_of_onedrive_moved_into_a_directory_that_is_left_here_is_taken
// test: a_rename_to_a_place_the_disk_holds_though_no_row_says_so_is_refused_and_replaces_nothing
// test: a_row_refused_after_the_disk_renames_back_and_restores_what_was_displaced
// test: a_file_open_for_writing_is_renamed_and_its_writer_keeps_its_file
// test: a_directory_with_a_file_open_for_writing_in_it_is_renamed_and_the_close_is_written
// test: a_file_renamed_over_a_file_that_is_not_on_disk_moves_with_nothing_set_aside
// test: a_file_not_on_disk_renamed_over_a_file_on_disk_sets_that_one_aside
// test: a_file_saved_by_a_rename_over_the_original_is_one_item_with_its_binding_and_new_content
// test: a_move_that_cannot_be_taken_back_asks_for_the_take_down_and_keeps_its_record
#![allow(dead_code)]

use std::ffi::OsStr;
use std::path::PathBuf;

use fuser::Errno;
use konedrive_tree::{ContentPresence, Item, ItemKind, ItemPlace, LocalItemId, LocalVersionChange, PlaceChangeRecord, PlaceChangeRowWrite};

use super::super::place_changes::{DirectoryMarks, PlaceUndo};
use super::ItemWriteOperations;
use crate::folder::disk::{is_hidden_name, make_aside_entry_name};

#[cfg(test)]
mod tests;

/// What the kernel's flags ask of a rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fuse) enum RenameMode {
    /// Over whatever is there.
    Replace,
    /// `RENAME_NOREPLACE`.
    NoReplace,
    /// `RENAME_EXCHANGE`.
    Exchange,
}

/// The two places of a rename, as the rows say them: read in one job of the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RenameCase {
    /// What the folder shows at the old place; `None` also when the node asked is no
    /// directory the folder shows.
    pub(super) moved: Option<Item>,
    /// The path of the moved item on disk; `None` for one with nothing there.
    pub(super) moved_path: Option<PathBuf>,
    pub(super) new_directory: Option<Item>,
    pub(super) new_directory_path: Option<PathBuf>,
    pub(super) new_directory_marks: DirectoryMarks,
    /// What the folder shows at the new place.
    pub(super) displaced: Option<Item>,
    /// Whether the folder shows something in the displaced directory.
    pub(super) is_displaced_empty: bool,
    /// Whether the new directory is the moved item or below it.
    pub(super) is_into_itself: bool,
    pub(super) mode: RenameMode,
    /// `folder::disk::is_hidden_name` of the new name.
    pub(super) is_new_name_hidden: bool,
    pub(super) is_new_name_ignored: bool,
}

/// What a rename that goes ahead does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RenameDecision {
    /// The same place: nothing to do.
    Nothing,
    /// To a place nothing is at: `PlaceChange::move_entry` (which replaces nothing), then
    /// `LocalVersionChange::Moved`.
    Move { item: LocalItemId, is_name_ignored: bool },
    /// Over an entry: `PlaceChange::move_entry_over` with `new_directory_marks` (the
    /// displaced item may have nothing on disk, and the directory above it neither),
    /// which sets the displaced entry aside until the row is written, then
    /// `LocalVersionChange::SavedOver`.
    SaveOver { replacing: LocalItemId, replaced: LocalItemId },
}

/// Whether the rename goes ahead and as what; the errno otherwise, in today's order
/// (`write_operations/rename_plan.rs:137-171`): `EINVAL` for an exchange; `ENOENT` for no
/// directory at the new place; `EPERM` for a new name the mount hides; `ENOENT` for
/// nothing at the old place; the same place is nothing to do; `EINVAL` for a directory
/// into itself or below; over an entry `EEXIST` when the mode keeps it, `EISDIR` for a
/// file over a directory, `ENOTDIR` for a directory over a file, `ENOTEMPTY` for a
/// directory the folder shows something in. Pure. Two things differ from today:
///
/// - `RENAME_EXCHANGE` is refused (`EINVAL`), as today, though the store could exchange.
/// - An item OneDrive has, moved into a directory that is left here, is TAKEN: today it
///   is `EXDEV` (`rename_plan.rs:168-170`), which makes `mv` copy and remove. The plan
///   then plans it as any item OneDrive has (design §6); its move cannot be sent into a
///   directory OneDrive does not have, so it waits for the user (`Reason::Parent`, by the plan's `wait_chain`) and
///   stays in OneDrive where it was until the directory is sent or the item moved out.
pub(super) fn decide_rename(case: &RenameCase) -> Result<RenameDecision, Errno> {
    if case.mode == RenameMode::Exchange {
        return Err(Errno::EINVAL);
    }
    if case.new_directory.is_none() {
        return Err(Errno::ENOENT);
    }
    if case.is_new_name_hidden {
        return Err(Errno::EPERM);
    }
    let moved = case.moved.as_ref().ok_or(Errno::ENOENT)?;
    let Some(displaced) = &case.displaced else {
        return match case.is_into_itself {
            true => Err(Errno::EINVAL),
            false => Ok(RenameDecision::Move { item: moved.local_item_id, is_name_ignored: case.is_new_name_ignored }),
        };
    };
    if displaced.local_item_id == moved.local_item_id {
        return Ok(RenameDecision::Nothing);
    }
    if case.is_into_itself {
        return Err(Errno::EINVAL);
    }
    match (moved.kind, displaced.kind) {
        _ if case.mode == RenameMode::NoReplace => Err(Errno::EEXIST),
        (ItemKind::File, ItemKind::Directory) => Err(Errno::EISDIR),
        (ItemKind::Directory, ItemKind::File) => Err(Errno::ENOTDIR),
        (ItemKind::Directory, ItemKind::Directory) if !case.is_displaced_empty => Err(Errno::ENOTEMPTY),
        (ItemKind::File, ItemKind::File) | (ItemKind::Directory, ItemKind::Directory) => Ok(RenameDecision::SaveOver { replacing: moved.local_item_id, replaced: displaced.local_item_id }),
    }
}

/// What a rename that goes ahead does to the names of the backing directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RenameDiskChange {
    /// Nothing of the moved item is on disk, and nothing of a displaced one: the row
    /// alone, with no record.
    Nothing,
    /// `PlaceChange::move_entry` from the path the moved item has on disk, on record as
    /// `MoveEntry`.
    MoveEntry { from: PathBuf },
    /// `PlaceChange::move_entry_over` from that path, on record as `MoveEntryOver`.
    MoveEntryOver { from: PathBuf, replaced: LocalItemId },
    /// Nothing of the moved item is on disk, and the displaced one may be:
    /// `PlaceChange::set_aside` of that one, on record as `SetAside`.
    SetAside { replaced: LocalItemId },
}

/// What `decision` does on disk, from what the case says is there: the moved item by its
/// path on disk, a displaced file by its content's presence; a displaced directory may be
/// on disk whatever its row says. Pure.
pub(super) fn decide_rename_disk_change(case: &RenameCase, decision: RenameDecision) -> RenameDiskChange {
    let is_displaced_on_disk = case.displaced.as_ref().is_some_and(|displaced| displaced.kind == ItemKind::Directory || displaced.content_presence == ContentPresence::OnDisk);
    match (decision, &case.moved_path) {
        (RenameDecision::Nothing, _) | (RenameDecision::Move { .. }, None) => RenameDiskChange::Nothing,
        (RenameDecision::Move { .. }, Some(from)) => RenameDiskChange::MoveEntry { from: from.clone() },
        (RenameDecision::SaveOver { replaced, .. }, Some(from)) => RenameDiskChange::MoveEntryOver { from: from.clone(), replaced },
        (RenameDecision::SaveOver { replaced, .. }, None) if is_displaced_on_disk => RenameDiskChange::SetAside { replaced },
        (RenameDecision::SaveOver { .. }, None) => RenameDiskChange::Nothing,
    }
}

impl ItemWriteOperations {
    /// What [`decide_rename`] decides from, read in one job of the store through the
    /// writer; whether a moved directory is on disk is then asked of the backing
    /// directory, which stands still under the tree lock.
    fn read_rename_case(&self, directory: LocalItemId, name: &OsStr, new_directory: LocalItemId, new_name: &OsStr, mode: RenameMode) -> Result<RenameCase, Errno> {
        let (name, new_name_read) = (name.to_owned(), new_name.to_owned());
        let (is_new_name_hidden, is_new_name_ignored) = (is_hidden_name(new_name), self.upload_rules.is_name_ignored(new_name));
        let mut case = self.run_store_job("the places of a rename could not be read", move |store| {
            let items = store.item_store();
            let reader = items.reader();
            let is_shown_directory = |row: &Item| row.kind == ItemKind::Directory && row.local.is_some();
            let moved = match reader.find_item(directory)?.filter(is_shown_directory) {
                Some(_) => reader.find_item_at(directory, &name)?,
                None => None,
            };
            let moved_path = match &moved {
                Some(moved) => reader.find_path(moved.local_item_id)?,
                None => None,
            };
            let new_directory_row = reader.find_item(new_directory)?.filter(is_shown_directory);
            let (new_directory_path, new_directory_marks, displaced) = match &new_directory_row {
                Some(_) => {
                    let (path, marks) = reader.find_path_with_marks(new_directory)?.unzip();
                    (path, marks.unwrap_or_default(), reader.find_item_at(new_directory, &new_name_read)?)
                }
                None => (None, Vec::new(), None),
            };
            let is_displaced_empty = match &displaced {
                Some(displaced) if displaced.kind == ItemKind::Directory => reader.list_children(displaced.local_item_id)?.is_empty(),
                _ => true,
            };
            let is_into_itself = match (&moved, &moved_path, &new_directory_path) {
                (Some(moved), Some(moved_path), Some(new_directory_path)) => moved.kind == ItemKind::Directory && new_directory_path.starts_with(moved_path),
                _ => false,
            };
            Ok(RenameCase { moved, moved_path, new_directory: new_directory_row, new_directory_path, new_directory_marks, displaced, is_displaced_empty, is_into_itself, mode, is_new_name_hidden, is_new_name_ignored })
        })?;
        let is_moved_on_disk = match (&case.moved, &case.moved_path) {
            (Some(moved), Some(_)) if moved.kind == ItemKind::File => moved.content_presence == ContentPresence::OnDisk,
            (Some(_), Some(path)) => self.backing_directory.read_metadata(path).is_some(),
            _ => false,
        };
        if !is_moved_on_disk {
            case.moved_path = None;
        }
        Ok(case)
    }

    /// Renames `name` in `directory` to `new_name` in `new_directory`.
    ///
    /// `read_rename_case`; [`decide_rename`]; [`decide_rename_disk_change`]; the record
    /// written (`ItemStore::record_place_change(MoveEntry { .. })`, `MoveEntryOver { .. }`
    /// or `SetAside { .. }`; none where no name changes on disk); a place change begun;
    /// the disk through it; the row through
    /// `write_row_or_undo` — `settle_place_change_record(record, LocalVersion(Moved { item,
    /// place, is_name_ignored }))` or `LocalVersion(SavedOver { replacing, replaced })`
    /// (undo: `MoveBack` / `MoveBackAndRestore` / `Restore`, and then
    /// `forget_place_change_record`); the change ended; what was set aside removed for
    /// good; then a file that
    /// took a binding over marked as the copy of that item. From the begin of the place
    /// change to its end no reader opens by a path of either place (the map's E3).
    ///
    /// Called by the kernel's rename, on the write-operation thread.
    pub(in crate::fuse) fn rename(&self, directory: LocalItemId, name: &OsStr, new_directory: LocalItemId, new_name: &OsStr, mode: RenameMode) -> Result<(), Errno> {
        self.access_mode_switch.check_read_write()?;
        let case = self.read_rename_case(directory, name, new_directory, new_name, mode)?;
        let decision = decide_rename(&case)?;
        let to_place = ItemPlace { parent_local_item_id: new_directory, name: new_name.to_owned() };
        let (item, row_change) = match decision {
            RenameDecision::Nothing => return Ok(()),
            RenameDecision::Move { item, is_name_ignored } => (item, LocalVersionChange::Moved { item, place: to_place.clone(), is_name_ignored }),
            RenameDecision::SaveOver { replacing, replaced } => (replacing, LocalVersionChange::SavedOver { replacing, replaced }),
        };
        let to_path = case.new_directory_path.as_ref().ok_or(Errno::ENOENT)?.join(new_name);
        let from_place = case.moved.as_ref().and_then(|moved| moved.local.as_ref()).and_then(|local| local.place.clone()).ok_or(Errno::ENOENT)?;
        let disk_change = decide_rename_disk_change(&case, decision);
        let record = match &disk_change {
            RenameDiskChange::MoveEntry { .. } => PlaceChangeRecord::MoveEntry { item, from: from_place, to: to_place },
            RenameDiskChange::MoveEntryOver { replaced, .. } => PlaceChangeRecord::MoveEntryOver { item, from: from_place, to: to_place, replaced: *replaced, aside_name: make_aside_entry_name(*replaced) },
            RenameDiskChange::SetAside { replaced } => PlaceChangeRecord::SetAside { item: *replaced, place: to_place, aside_name: make_aside_entry_name(*replaced) },
            RenameDiskChange::Nothing => {
                self.run_store_job("a rename could not be written", move |store| store.item_store().apply(&row_change))?;
                self.wake_plan();
                return Ok(());
            }
        };
        let record = self.write_place_change_record(record)?;
        #[cfg(test)]
        self.reach(super::tests::OperationPoint::RecordWritten);
        let change = self.place_changes.begin();
        let moved = match &disk_change {
            RenameDiskChange::MoveEntry { from } => change.move_entry(from, &to_path, &case.new_directory_marks),
            RenameDiskChange::MoveEntryOver { from, replaced } => change.move_entry_over(from, &to_path, *replaced, &case.new_directory_marks),
            RenameDiskChange::SetAside { replaced } => change.set_aside(&to_path, *replaced),
            RenameDiskChange::Nothing => Ok(PlaceUndo::Nothing),
        };
        let undo = moved.map_err(|refusal| self.refuse_place_change(record, &refusal))?;
        #[cfg(test)]
        self.reach(super::tests::OperationPoint::DiskChanged);
        let (_, undo) = self.write_row_or_undo(&change, undo, record, move |store| store.item_store().settle_place_change_record(record, &PlaceChangeRowWrite::LocalVersion(row_change)))?;
        change.end();
        self.place_changes.discard(undo);
        self.mark_saved_over_file(&case, &to_path);
        Ok(())
    }

    /// The file of `case` that was saved over a file of OneDrive and took its binding
    /// over — a file with no binding of its own, on disk — is marked at `to_path` as the
    /// copy of that item. After the row: a mark that cannot be written is logged, and the
    /// check at start writes it.
    fn mark_saved_over_file(&self, case: &RenameCase, to_path: &std::path::Path) {
        let (Some(moved), Some(_), Some(displaced)) = (&case.moved, &case.moved_path, &case.displaced) else { return };
        let (ItemKind::File, None, Some(binding)) = (moved.kind, &moved.onedrive_item, &displaced.onedrive_item) else { return };
        if let Err(error) = self.backing_directory.mark_downloaded_copy(to_path, binding.item_id.as_str()) {
            tracing::warn!("{} could not be marked as the copy of the file it was saved over: {error}", to_path.display());
        }
    }
}
