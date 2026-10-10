//! The check of the backing directory at start ([`BackingDirectoryCheck`]): before the mount
//! goes over the folder, the backing directory is compared with the rows of `items`, once,
//! and the rows are made to say what the disk has. It is the one place where the disk is
//! read to learn what the folder contains: a file edited, made, moved or removed by hand
//! while no daemon ran; a daemon that died between a change on disk and its row; a store
//! that was lost.
//!
//! [`BackingDirectoryCheck`] holds what a run is given and no state of its own. A run is
//! five steps, each a part that owns what it gathers:
//!
//! - [`item_rows`]: all the rows, read whole once, by id, by place and by binding;
//! - [`backing_directory_contents`]: all the backing directory holds, read once on several
//!   threads, with the marks of each entry;
//! - [`entry_identities`]: which item each entry is, or that it cannot be said — the rows
//!   and the contents matched in memory, a directory before what is in it;
//! - [`path_findings`]: the finding of each path, by the one decision
//!   ([`disk_finding`]), which has no disk and no store in it;
//! - [`write_order`]: which finding is written before which, so that the schema accepts
//!   each write; what a finding writes is [`row_writer`]'s, through [`ItemStore`] and
//!   over [`local_places`], the places as the writes so far leave them.
//!
//! Four rules stand over all of it (`docs/tasks/item-versions-design.md`, §10).
//!
//! - What the person could not see is not removed, and was not moved by them: an item of
//!   OneDrive with nothing of its own on disk loses its place and stays OneDrive's
//!   ([`DiskFinding::PlaceLost`]), and a file whose content was not on disk is not found
//!   moved by a mark.
//! - What may be in a part of the backing directory that was not read is left alone: a row
//!   whose place is in that part, a row no entry is taken as, and an entry that carries the
//!   mark of a row whose place could not be looked at are neither removed, nor moved, nor
//!   made new. They are said ([`UnresolvedReason::MayBeUnread`]), and the rows among them
//!   are held ([`DiskFindings::list_held_items`]): the folder is not.
//! - A mark names an item id and no drive, and the check guesses none: an entry is the row
//!   at its own place when that row has the mark's id, else the one row that has the id;
//!   when rows of two drives have it, the entry is left with what depends on it
//!   ([`UnresolvedReason::ItemIdInTwoDrives`]).
//! - An entry taken as the own entry of an item OneDrive has gets that item's id as its
//!   mark when it carries none.
//!
//! It reads the backing directory through [`BackingDirectory`] and writes rows through
//! [`ItemStore`]. It changes two things on disk, both marks: one that is not the row's own
//! is taken off the entry that carries it, and the id of its item is written on an entry
//! that is an item's own and does not carry it. It knows nothing of the outbox, of
//! `onedrive_items`, of the node table or of the mount.

use std::path::PathBuf;

use konedrive_tree::{DriveId, ItemStore, LocalItemId, TreeError};

use super::backing_directory::BackingDirectory;
use super::upload_rules::UploadRules;

mod backing_directory_contents;
mod disk_finding;
mod entry_identities;
mod item_rows;
mod local_places;
mod path_findings;
mod row_writer;
mod write_order;

pub(in crate::fuse) use disk_finding::{DiskFinding, UnresolvedReason};

use backing_directory_contents::BackingDirectoryContents;
use entry_identities::EntryIdentities;
use item_rows::ItemRows;
use path_findings::PathFindings;
use write_order::WriteOrder;

/// A path the check left as it found it, with what is below it, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fuse) struct UnresolvedPath {
    /// Relative to the backing directory.
    pub(in crate::fuse) path: PathBuf,
    pub(in crate::fuse) reason: UnresolvedReason,
}

/// What a check answers: how many of each finding it wrote to the rows, the paths it left
/// with why, the rows it left as they were found, and whether any part of the backing
/// directory was not read. A path that is not among those it left, and its row, agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fuse) struct DiskFindings {
    counts: [usize; DiskFinding::ALL.len()],
    unresolved: Vec<UnresolvedPath>,
    held_items: Vec<LocalItemId>,
    has_unread_part: bool,
}

impl DiskFindings {
    /// How many times `finding` was found and written.
    pub(in crate::fuse) fn count(&self, finding: DiskFinding) -> usize {
        self.counts[finding as usize]
    }

    /// The paths that were left as they were found, by path: nothing is left silently.
    pub(in crate::fuse) fn unresolved(&self) -> &[UnresolvedPath] {
        &self.unresolved
    }

    /// The rows that were left as they were found, though the disk may not have what they
    /// say: a row whose own place could not be looked at, a row that would have left the
    /// folder or lost its content while it may be in a part that was not read or be an
    /// entry that was left, and a row whose write was refused or waits. Nothing of these,
    /// and of what the rows say is below them, is to be sent before a check decides them;
    /// every other row is as the disk has it.
    pub(in crate::fuse) fn list_held_items(&self) -> &[LocalItemId] {
        &self.held_items
    }

    /// Whether any part of the backing directory was left unread: a directory that could
    /// not be read or is too deep, an entry whose marks could not be read. It holds
    /// nothing by itself; it says why rows may be among [`list_held_items`](Self::list_held_items).
    pub(in crate::fuse) fn has_unread_part(&self) -> bool {
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
pub(in crate::fuse) struct BackingDirectoryCheck<'a> {
    backing_directory: &'a BackingDirectory,
    /// What says of a made or moved item's name whether it is ignored.
    upload_rules: &'a UploadRules,
    /// The drive of the folder: an entry marked with an item id no row has is bound in the
    /// drive of the nearest directory of OneDrive above it, or in this one. No row is looked
    /// for by it.
    drive_id: &'a DriveId,
}

impl<'a> BackingDirectoryCheck<'a> {
    pub(in crate::fuse) fn new(backing_directory: &'a BackingDirectory, upload_rules: &'a UploadRules, drive_id: &'a DriveId) -> Self {
        BackingDirectoryCheck { backing_directory, upload_rules, drive_id }
    }

    /// Compares the backing directory with the rows and writes what it finds: afterwards
    /// the rows say what the disk has, but for the paths the answer names. An error is the
    /// store's, from reading the rows: nothing was written then. Nothing else may read or
    /// write the rows or the backing directory meanwhile: no mount lies over the folder.
    pub(in crate::fuse) fn run(&self, item_store: &mut ItemStore<'_>) -> Result<DiskFindings, TreeError> {
        let rows = ItemRows::new(item_store.list_items()?);
        let contents = BackingDirectoryContents::read(self.backing_directory);
        let identities = EntryIdentities::settle(&rows, &contents);
        let findings = PathFindings::decide(&rows, &contents, &identities);
        Ok(WriteOrder::new(item_store, self.backing_directory, self.upload_rules, self.drive_id, &rows, &contents, &identities, &findings).write())
    }
}

#[cfg(test)]
mod tests;
