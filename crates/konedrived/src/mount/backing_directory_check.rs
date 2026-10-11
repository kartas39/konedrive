//! The check of the backing directory at start ([`BackingDirectoryCheck`]): before the mount
//! goes over the folder, the backing directory is compared with the rows of `items`, once,
//! and the rows are made to say what the disk has. It is the one place where the disk is
//! read to learn what the folder contains (`docs/tasks/item-versions-design.md`, §10).
//!
//! **The check does not guess what a person did while no daemon ran.** It looks at one
//! place at a time: what the rows say of a place, and what the disk has at that place. A
//! file moved by hand is a new file at its new place, and the item it was is shown again
//! at its old one as OneDrive has it; a file removed by hand is shown again. Neither is a
//! move or a removal in OneDrive: the check has no finding that moves a row, and none that
//! removes an item OneDrive has.
//!
//! [`BackingDirectoryCheck`] holds what a run is given and no state of its own. A run is
//! four steps, each a part that owns what it gathers:
//!
//! - [`standing_place_changes`]: a change of the daemon's own that a death cut short and
//!   left on record is finished or undone first, by the one decision of
//!   [`place_change_settlement`];
//! - [`item_rows`]: all the rows, read whole once, by id and by place;
//! - [`backing_directory_contents`]: all the backing directory holds, read once, a level
//!   of directories at a time on several threads, with the marks of each entry;
//! - [`place_walk`]: every place, a directory before what is in it, decided by the one
//!   decision of [`disk_finding`], which has no disk and no store in it, and written
//!   through [`row_writer`], which writes through [`ItemStore`].
//!
//! The marks on disk are read for three things and no more: the whole-copy state and the
//! cTag of the entry at a row's own place (a clean copy or a changed one; a copy that
//! landed before its row, also where a record says a download was to land there); the id
//! on an entry no row has, when no row at all has that id and no other entry carries it
//! (a store that was rebuilt); and the mark on an entry that becomes a new item, which is
//! taken off. Nothing is matched by a mark across places: of the entries that carry an id
//! only their number is kept, and an id that several carry — a second name of a file, a
//! copy made with its marks — binds none of them. What the check writes of marks is
//! [`mark_change`]'s decision: the id of its item on the own entry of an item OneDrive
//! has, and a mark taken off a new item. Of a file that has several names no mark is
//! written or taken off, and its cTag mark is not asked: the marks are the file's, and
//! another name of it may be another item's.
//!
//! A part of the backing directory that cannot be read is left as it is: no row in it or
//! below it is changed, and they are held ([`DiskFindings::list_held_items`]) and said.
//! The folder is not held for it, and no other row is.
//!
//! It knows nothing of the upload queue, of `onedrive_items`, of the node table or of the mount.

use std::path::PathBuf;

use konedrive_tree::{DriveId, ItemStore, LocalItemId, TreeError};

use super::backing_directory::BackingDirectory;
use super::upload_rules::UploadRules;

mod backing_directory_contents;
mod disk_finding;
mod item_rows;
mod mark_change;
mod place_change_settlement;
mod place_walk;
mod row_writer;
mod standing_place_changes;

pub(in crate::mount) use disk_finding::{DiskFinding, UnresolvedReason};

use backing_directory_contents::BackingDirectoryContents;
use item_rows::ItemRows;
use place_walk::PlaceWalk;
use row_writer::RowWriter;
use standing_place_changes::StandingPlaceChanges;

/// A path the check left as it found it, with what is below it, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::mount) struct UnresolvedPath {
    /// Relative to the backing directory.
    pub(in crate::mount) path: PathBuf,
    pub(in crate::mount) reason: UnresolvedReason,
}

/// What a check answers: how many of each finding it wrote to the rows, the paths it left
/// with why, the rows it left as they were found, and whether any part of the backing
/// directory was not read. A path that is not among those it left, and its row, agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::mount) struct DiskFindings {
    counts: [usize; DiskFinding::ALL.len()],
    unresolved: Vec<UnresolvedPath>,
    held_items: Vec<LocalItemId>,
    has_unread_part: bool,
}

impl DiskFindings {
    /// How many times `finding` was found and written.
    pub(in crate::mount) fn count(&self, finding: DiskFinding) -> usize {
        self.counts[finding as usize]
    }

    /// The paths that were left as they were found, by path: nothing is left silently.
    pub(in crate::mount) fn unresolved(&self) -> &[UnresolvedPath] {
        &self.unresolved
    }

    /// The rows that were left as they were found, though the disk may not have what they
    /// say: the row of a directory that was not read, for all the folder shows in it and
    /// below it; the row at a name that could not be looked at; a row a place change that
    /// stands on record names; and a row whose write was refused, or that waits for one.
    /// Nothing of these, and of what the rows say is below them, is to be sent before a
    /// check decides them; every other row is as the disk has it.
    pub(in crate::mount) fn list_held_items(&self) -> &[LocalItemId] {
        &self.held_items
    }

    /// Whether any part of the backing directory was left as it is: a directory that could
    /// not be read or is too deep, a name that is no file and no directory or whose marks
    /// could not be read, a place change that stands on record. It holds nothing by
    /// itself; it says why rows may be among [`list_held_items`](Self::list_held_items).
    pub(in crate::mount) fn has_unread_part(&self) -> bool {
        self.has_unread_part
    }

    /// Nothing written yet: the paths that were not read are left already.
    fn begin(unread: Vec<UnresolvedPath>, has_unread_part: bool) -> Self {
        DiskFindings { counts: [0; DiskFinding::ALL.len()], unresolved: unread, held_items: Vec::new(), has_unread_part }
    }

    /// `path` is left as it was found.
    fn leave(&mut self, path: PathBuf, reason: UnresolvedReason) {
        self.unresolved.push(UnresolvedPath { path, reason });
    }

    /// The row of `item` is left as it was found.
    fn hold(&mut self, item: LocalItemId) {
        self.held_items.push(item);
    }

    /// The answer: the paths that were left by path, each said once for a reason, and the
    /// rows that were left, each once.
    fn finish(mut self) -> Self {
        self.unresolved.sort_by(|a, b| a.path.cmp(&b.path));
        self.unresolved.dedup();
        self.held_items.sort();
        self.held_items.dedup();
        self
    }

    fn add(&mut self, finding: DiskFinding) {
        self.counts[finding as usize] += 1;
    }
}

/// The check of one folder's backing directory against its rows.
pub(in crate::mount) struct BackingDirectoryCheck<'a> {
    backing_directory: &'a BackingDirectory,
    /// What says of a made item's name whether it is ignored.
    upload_rules: &'a UploadRules,
    /// The drive of the folder: an entry marked with an item id no row has is bound in the
    /// drive of the nearest directory of OneDrive above it, or in this one. No row is looked
    /// for by it.
    drive_id: &'a DriveId,
}

impl<'a> BackingDirectoryCheck<'a> {
    pub(in crate::mount) fn new(backing_directory: &'a BackingDirectory, upload_rules: &'a UploadRules, drive_id: &'a DriveId) -> Self {
        BackingDirectoryCheck { backing_directory, upload_rules, drive_id }
    }

    /// Finishes or undoes the place changes left on record, then compares the backing
    /// directory with the rows and writes what it finds: afterwards the rows say what the
    /// disk has, but for the paths the answer names. An error is the store's, from
    /// settling a record or from reading the rows. Nothing else may read or write the rows
    /// or the backing directory meanwhile: no mount lies over the folder. The entries that
    /// were set aside are not removed here, and must not be removed before this ran: one
    /// of them may be what a record puts back.
    pub(in crate::mount) fn run(&self, item_store: &mut ItemStore<'_>) -> Result<DiskFindings, TreeError> {
        let standing_place_changes = StandingPlaceChanges::settle(self.backing_directory, self.upload_rules, item_store)?;
        let rows = ItemRows::new(item_store.list_items()?);
        let contents = BackingDirectoryContents::read(self.backing_directory);
        let writer = RowWriter::new(item_store, self.upload_rules);
        Ok(PlaceWalk::new(self.backing_directory, self.drive_id, &rows, &contents, &standing_place_changes, writer).walk())
    }
}

#[cfg(test)]
mod tests;
