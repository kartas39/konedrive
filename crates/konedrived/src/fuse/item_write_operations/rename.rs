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
//! and writes no record. The row takes the record (`settle_place_change_record` with the
//! `Moved` or the `SavedOver`). A death between the disk and the row: the check at start
//! moves the entry back, and puts the displaced one back under its name; no row changes,
//! and the rename did not happen.
//!
//! **Tested without the rest:**
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
#![allow(dead_code, unused_variables)]

use std::ffi::OsStr;
use std::path::PathBuf;

use fuser::Errno;
use konedrive_tree::{Item, LocalItemId};

use super::super::place_changes::DirectoryMarks;
use super::ItemWriteOperations;

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
    /// Over an entry: `PlaceChange::move_entry_over`, which sets the displaced entry aside
    /// until the row is written, then `LocalVersionChange::SavedOver`.
    SaveOver { replacing: LocalItemId, replaced: LocalItemId },
}

/// Whether the rename goes ahead and as what; the errno otherwise, in today's order
/// (`write_operations/rename_plan.rs:137-171`). Pure. Two things differ from today:
///
/// - `RENAME_EXCHANGE` is refused (`EINVAL`), as today, though the store could exchange.
/// - An item OneDrive has, moved into a directory that is left here, is TAKEN: today it
///   is `EXDEV` (`rename_plan.rs:168-170`), which makes `mv` copy and remove. The plan
///   then plans it as any item OneDrive has (design §6); its move cannot be sent into a
///   directory OneDrive does not have, so it waits for the user (`Reason::Parent`, by the plan's `wait_chain`) and
///   stays in OneDrive where it was until the directory is sent or the item moved out.
pub(super) fn decide_rename(case: &RenameCase) -> Result<RenameDecision, Errno> {
    todo!()
}

impl ItemWriteOperations {
    /// What [`decide_rename`] decides from, read in one job of the store through the
    /// writer.
    fn read_rename_case(&self, directory: LocalItemId, name: &OsStr, new_directory: LocalItemId, new_name: &OsStr, mode: RenameMode) -> Result<RenameCase, Errno> {
        todo!()
    }

    /// Renames `name` in `directory` to `new_name` in `new_directory`.
    ///
    /// `read_rename_case`; [`decide_rename`]; the record written
    /// (`ItemStore::record_place_change(MoveEntry { .. })` or `MoveEntryOver { .. }`; none
    /// for an item with nothing on disk); a place change begun; the disk through it
    /// (an item with nothing on disk changes no disk); the row through
    /// `write_row_or_undo` — `settle_place_change_record(record, LocalVersion(Moved { item,
    /// place, is_name_ignored }))` or `LocalVersion(SavedOver { replacing, replaced })`
    /// (undo: `MoveBack` / `MoveBackAndRestore`, and then
    /// `forget_place_change_record`); then a file that
    /// took a binding over marked as the copy of that item. From the begin of the place
    /// change to its end no reader opens by a path of either place (the map's E3).
    ///
    /// Called by the kernel's rename, on the write-operation thread.
    pub(in crate::fuse) fn rename(&self, directory: LocalItemId, name: &OsStr, new_directory: LocalItemId, new_name: &OsStr, mode: RenameMode) -> Result<(), Errno> {
        todo!()
    }
}
