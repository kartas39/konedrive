//! Make: mkdir, mknod and create. The disk first, the row second (design §8).
//!
//! **Decides:** [`decide_make`], a pure function of what the rows say at the place.
//! **Writes:** `LocalVersionChange::Made`, with `replaced` when a file is made where a
//! file was removed here whose removal is not sent (design §5: the made file takes the
//! binding over, and OneDrive sees one item whose content changed); for a created file
//! `ItemStore::apply_made_open_for_writing`, the making and the first writer's open in
//! one transaction.
//!
//! **On record before the disk:** `PlaceChangeRecord::MakeEntry { kind, place, owner:
//! MadeEntryOwner::NewItem { replaced } }`, with the `replaced` the decision answered. The
//! row takes it: `settle_place_change_record` with the `Made`, and for a created file the
//! record's id given to `apply_made_open_for_writing` as its last argument, which takes it
//! in its transaction.
//! A death between the entry and the row: the check at start makes the row from the record
//! (the entry is there, no row has its place), taking the binding of `replaced` over.
//!
//! **Tested without the rest:**
// test: each_case_of_a_make_is_decided
// test: a_directory_made_is_on_disk_and_then_in_its_row
// test: a_directory_made_below_a_directory_that_is_not_on_disk_makes_it_marked
// test: a_file_made_is_empty_on_disk_changed_here_and_on_disk_in_its_row
// test: a_file_made_where_a_file_was_removed_and_not_sent_takes_its_binding_over
// test: a_file_made_where_a_removal_is_being_sent_takes_nothing_over
// test: a_name_the_upload_rules_ignore_is_made_with_the_flag
// test: a_name_of_the_daemons_own_and_the_trash_are_refused
// test: a_name_that_is_taken_is_refused_before_the_disk_is_touched
// test: a_name_the_disk_holds_though_no_row_says_so_is_refused_and_nothing_is_replaced
// test: a_file_created_is_made_and_open_for_writing_in_one_transaction
// test: a_row_refused_after_the_disk_removes_what_was_made
// test: a_made_entry_that_cannot_be_removed_again_asks_for_the_take_down_and_keeps_its_record
// test: a_created_file_is_answered_and_has_its_writer_whatever_is_read_after_its_row
#![allow(dead_code)]

use std::ffi::OsStr;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use fuser::Errno;
use konedrive_tree::{ContentVersion, Item, ItemKind, ItemPlace, LocalItemId, LocalVersionChange, MadeEntryOwner, PlaceChangeRecord, PlaceChangeRowWrite, TreeStore};

use super::super::backing_directory::EntryKind;
use super::super::errno::to_errno;
use super::super::item_handles::Fh;
use super::super::place_changes::DirectoryMarks;
use super::{read_size_and_time, ItemWriteOperations};
use crate::folder::disk::is_hidden_name;

#[cfg(test)]
mod tests;

/// What is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::mount) enum MadeKind {
    /// mkdir.
    Directory,
    /// mknod of a regular file.
    File,
    /// create: a file that is open for writing from its making.
    FileOpenedForWriting,
}

impl MadeKind {
    fn to_item_kind(self) -> ItemKind {
        match self {
            MadeKind::Directory => ItemKind::Directory,
            MadeKind::File | MadeKind::FileOpenedForWriting => ItemKind::File,
        }
    }

    fn to_entry_kind(self) -> EntryKind {
        match self {
            MadeKind::Directory => EntryKind::Directory,
            MadeKind::File | MadeKind::FileOpenedForWriting => EntryKind::File,
        }
    }
}

/// What was made.
#[derive(Debug, Clone)]
pub(in crate::mount) struct MadeItem {
    /// The row of the made item, as it was written: what the request's answer makes the
    /// attributes from (`item_attributes::to_item_attributes`, with the facts
    /// `ItemLookup::read_backing_file_facts` reads; the write operations hold neither).
    pub(in crate::mount) item: Item,
    /// The writer's handle of a created file; `Fh::NOT_OPENED` otherwise.
    pub(in crate::mount) fh: Fh,
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
    /// `folder::disk::is_hidden_name` of the name: an entry no row says, a temporary name.
    pub(super) is_hidden_name: bool,
    /// A file manager's trash directory, to be made in the folder itself.
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
    if case.directory.is_none() {
        return Err(Errno::ENOENT);
    }
    if case.is_hidden_name || case.is_trash {
        return Err(Errno::EPERM);
    }
    if case.holder.is_some() {
        return Err(Errno::EEXIST);
    }
    let replaced = match (case.kind, &case.removed_file) {
        (MadeKind::File | MadeKind::FileOpenedForWriting, Some(removed)) if !case.is_name_ignored && !case.has_removed_file_a_running_step => Some(removed.local_item_id),
        _ => None,
    };
    Ok(MakeDecision { is_name_ignored: case.is_name_ignored, replaced })
}

impl ItemWriteOperations {
    /// What [`decide_make`] decides from, read in one job of the store through the
    /// writer, with what the upload rules and the running steps say.
    fn read_make_case(&self, directory: LocalItemId, name: &OsStr, kind: MadeKind) -> Result<MakeCase, Errno> {
        let name_read = name.to_owned();
        let (directory_row, directory_path, directory_marks, holder, removed_file) = self.run_store_job("the place of a make could not be read", move |store| {
            let items = store.item_store();
            let reader = items.reader();
            let Some(shown) = reader.find_item(directory)?.filter(|row| row.kind == ItemKind::Directory && row.local.is_some()) else { return Ok((None, None, Vec::new(), None, None)) };
            let (path, marks) = reader.find_path_with_marks(directory)?.unzip();
            Ok((Some(shown), path, marks.unwrap_or_default(), reader.find_item_at(directory, &name_read)?, reader.find_removed_file_at_synced_place(directory, &name_read)?))
        })?;
        Ok(MakeCase {
            directory: directory_row,
            directory_path,
            directory_marks,
            holder,
            has_removed_file_a_running_step: removed_file.as_ref().is_some_and(|removed| self.running_steps.has_running_step(removed.local_item_id)),
            removed_file,
            kind,
            is_hidden_name: is_hidden_name(name),
            is_trash: kind == MadeKind::Directory && directory == LocalItemId::ROOT && self.upload_rules.is_trash(&name.to_string_lossy()),
            is_name_ignored: self.upload_rules.is_name_ignored(name),
        })
    }

    /// Mkdir, mknod, create: makes `name` in `directory`.
    ///
    /// `read_make_case`; [`decide_make`]; the record written
    /// (`ItemStore::record_place_change(MakeEntry { kind, place, owner: NewItem { replaced }
    /// })`); a place change begun; the entry made (`PlaceChange::make_entry`, with the
    /// directories above); a file that replaces a removed one marked as the copy of that
    /// item; then the row through `write_row_or_undo` (undo: `PlaceUndo::RemoveMade`, and
    /// then `forget_place_change_record`): `ItemStore::settle_place_change_record(record,
    /// LocalVersion(Made { kind, place, content, is_name_ignored, replaced }))`, or for a
    /// created file `apply_made_open_for_writing` with the record, after which the writer
    /// is added to `ItemHandles`. The row answered ([`MadeItem::item`]) is read in the job
    /// that wrote it; the writer is added whatever that read said, and when it said
    /// nothing the writer is closed again and the program gets `EIO`.
    /// The new file's content is `ContentVersion::ChangedHere` of size 0, the backing
    /// file's time, change count 1. A created file that is closed with nothing written
    /// has that content again (`ClosedUnchanged`): its writer is not marked as having
    /// written, since an empty new file is unsent as it is.
    ///
    /// Called by the kernel's mkdir, mknod and create, on the write-operation thread.
    pub(in crate::mount) fn make(&self, directory: LocalItemId, name: &OsStr, kind: MadeKind) -> Result<MadeItem, Errno> {
        self.access_mode_switch.check_read_write()?;
        let case = self.read_make_case(directory, name, kind)?;
        let decision = decide_make(&case)?;
        let path = case.directory_path.as_ref().ok_or(Errno::ENOENT)?.join(name);
        let place = ItemPlace { parent_local_item_id: directory, name: name.to_owned() };
        let item_kind = kind.to_item_kind();
        let record = self.write_place_change_record(PlaceChangeRecord::MakeEntry { kind: item_kind, place: place.clone(), owner: MadeEntryOwner::NewItem { replaced: decision.replaced } })?;
        #[cfg(test)]
        self.reach(super::tests::OperationPoint::RecordWritten);
        let change = self.place_changes.begin();
        let (file, undo) = change.make_entry(&path, kind.to_entry_kind(), &case.directory_marks).map_err(|refusal| self.refuse_place_change(record, &refusal))?;
        // What the row is written from, read from the made file; and the mark of the item
        // whose binding it takes over.
        let replaced_binding = case.removed_file.as_ref().filter(|removed| decision.replaced == Some(removed.local_item_id)).and_then(|removed| removed.onedrive_item.as_ref());
        let read = (|| -> io::Result<Option<ContentVersion>> {
            let Some(file) = &file else { return Ok(None) };
            if let Some(binding) = replaced_binding {
                self.backing_directory.mark_downloaded_copy(&path, binding.item_id.as_str())?;
            }
            let (size, mtime, mtime_nsec) = read_size_and_time(file)?;
            Ok(Some(ContentVersion::ChangedHere { size, mtime, mtime_nsec, change_count: 1 }))
        })();
        let content = match read {
            Ok(content) => content,
            Err(error) => return Err(self.undo_place_change(&change, undo, record, to_errno(&error))),
        };
        #[cfg(test)]
        self.reach(super::tests::OperationPoint::DiskChanged);
        let (is_name_ignored, replaced) = (decision.is_name_ignored, decision.replaced);
        // The row, and what the answer is made from, out of the one job: read with the
        // writer that wrote it, before anything else can change it. A read that fails
        // after the row is written is no failure of the write, and undoes nothing.
        let write = move |store: &mut TreeStore| {
            let made = match (kind, content) {
                (MadeKind::FileOpenedForWriting, Some(content)) => store.item_store().apply_made_open_for_writing(&place, &content, is_name_ignored, replaced, Some(record)),
                (_, content) => store.item_store().settle_place_change_record(record, &PlaceChangeRowWrite::LocalVersion(LocalVersionChange::Made { kind: item_kind, place, content, is_name_ignored, replaced })),
            }?;
            let row = store.item_store().reader().find_item(made).inspect_err(|error| tracing::warn!("the row of item {made}, which was made, could not be read back: {error}")).ok().flatten();
            Ok((made, row))
        };
        let ((made, row), _) = self.write_row_or_undo(&change, undo, record, write)?;
        change.end();
        // The row of a created file says a writer holds it: the writer is added whatever
        // was read of the row.
        let fh = match (kind, file) {
            (MadeKind::FileOpenedForWriting, Some(file)) => self.item_handles.add_writer(made, Arc::new(file)).0,
            _ => Fh::NOT_OPENED,
        };
        let Some(item) = row else {
            // Nothing to answer the program with: the handle it will never get is closed,
            // so that no row keeps the content of before a write with no writer.
            if fh != Fh::NOT_OPENED {
                self.close_writer(fh);
            }
            return Err(Errno::EIO);
        };
        Ok(MadeItem { item, fh })
    }
}
