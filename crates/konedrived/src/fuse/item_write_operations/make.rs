//! Make: mkdir, mknod and create. The disk first, the row second (design §8).
//!
//! **Decides:** [`decide_make`], a pure function of what the rows say at the place.
//! **Writes:** `LocalVersionChange::Made`, with `replaced` when a file is made where a
//! file was removed here whose removal is not sent (design §5: the made file takes the
//! binding over, and OneDrive sees one item whose content changed); for a created file
//! `ItemStore::apply_made_open_for_writing`, the making and the first writer's open in
//! one transaction.
//!
//! **Tested without the rest:**
// test: a_directory_made_is_on_disk_and_then_in_its_row
// test: a_file_made_is_empty_on_disk_changed_here_and_on_disk_in_its_row
// test: a_file_made_where_a_file_was_removed_and_not_sent_takes_its_binding_over
// test: a_file_made_where_a_removal_is_being_sent_takes_nothing_over
// test: a_name_the_upload_rules_ignore_is_made_with_the_flag
// test: a_name_of_the_daemons_own_and_the_trash_are_refused
// test: a_name_that_is_taken_is_refused_before_the_disk_is_touched
// test: a_name_the_disk_holds_though_no_row_says_so_is_refused_and_nothing_is_replaced
// test: a_file_created_is_made_and_open_for_writing_in_one_transaction
// test: a_row_refused_after_the_disk_removes_what_was_made
#![allow(dead_code, unused_variables)]

use std::ffi::OsStr;
use std::path::PathBuf;

use fuser::{Errno, FileAttr};
use konedrive_tree::{Item, LocalItemId};

use super::super::open_handles::Fh;
use super::super::place_changes::DirectoryMarks;
use super::ItemWriteOperations;

/// What is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fuse) enum MadeKind {
    /// mkdir.
    Directory,
    /// mknod of a regular file.
    File,
    /// create: a file that is open for writing from its making.
    FileOpenedForWriting,
}

/// What was made.
#[derive(Debug, Clone)]
pub(in crate::fuse) struct MadeItem {
    pub(in crate::fuse) item: LocalItemId,
    pub(in crate::fuse) attributes: FileAttr,
    /// The writer's handle of a created file; `Fh::NOT_OPENED` otherwise.
    pub(in crate::fuse) fh: Fh,
}

/// What the rows and the rules say at the place something is to be made at: read in one
/// job of the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MakeCase {
    /// The directory, when the folder shows one as the node asked.
    pub(super) directory: Option<Item>,
    /// The path of the directory in the backing directory, and what the directories of it
    /// are marked with where they are made.
    pub(super) directory_path: Option<PathBuf>,
    pub(super) directory_marks: DirectoryMarks,
    /// What the folder shows under the name there.
    pub(super) holder: Option<Item>,
    /// The file removed from that place here whose removal is not sent
    /// (`ItemReader::find_removed_file_at_synced_place`).
    pub(super) removed_file: Option<Item>,
    /// Whether a step runs for that file (`RunningSteps::has_running_step`): its removal
    /// may be at OneDrive already, and nothing is taken over.
    pub(super) has_removed_file_a_running_step: bool,
    pub(super) kind: MadeKind,
    /// `folder::disk::is_hidden_name` of the name: an entry no row says, a row the check
    /// set aside, a temporary name.
    pub(super) is_hidden_name: bool,
    pub(super) is_trash: bool,
    pub(super) is_name_ignored: bool,
}

/// What a make that goes ahead writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MakeDecision {
    pub(super) is_name_ignored: bool,
    /// The item whose binding the made file takes over.
    pub(super) replaced: Option<LocalItemId>,
}

/// Whether the make goes ahead, and what it writes; the errno otherwise, in today's order
/// (`write_operations/make_check.rs:46-58`): `ENOENT` for no directory, `EPERM` for a name
/// the mount hides or the trash, `EEXIST` for a taken name. Pure. A directory never
/// replaces anything; a file replaces a removed file only when its own name is not
/// ignored and no step runs for that file.
pub(super) fn decide_make(case: &MakeCase) -> Result<MakeDecision, Errno> {
    todo!()
}

impl ItemWriteOperations {
    /// What [`decide_make`] decides from, read in one job of the store through the
    /// writer, with what the upload rules and the running steps say.
    fn read_make_case(&self, directory: LocalItemId, name: &OsStr, kind: MadeKind) -> Result<MakeCase, Errno> {
        todo!()
    }

    /// Mkdir, mknod, create: makes `name` in `directory`.
    ///
    /// `read_make_case`; [`decide_make`]; a place change begun; the entry made
    /// (`PlaceChange::make_entry`, with the directories above); a file that replaces a
    /// removed one marked as the copy of that item; then the row through
    /// `write_row_or_undo` (undo: `PlaceUndo::RemoveMade`): `ItemStore::apply(Made { kind,
    /// place, content, is_name_ignored, replaced })`, or for a created file
    /// `apply_made_open_for_writing`, after which the writer is added to `ItemHandles`.
    /// The new file's content is `ContentVersion::ChangedHere` of size 0, the backing
    /// file's time, change count 1.
    ///
    /// Called by the kernel's mkdir, mknod and create, on the write-operation thread.
    pub(in crate::fuse) fn make(&self, directory: LocalItemId, name: &OsStr, kind: MadeKind) -> Result<MadeItem, Errno> {
        todo!()
    }
}
