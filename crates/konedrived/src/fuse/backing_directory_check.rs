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
//! - [`entry_identities`]: which item each entry is — the rows and the contents matched in
//!   memory, a directory before what is in it;
//! - [`path_findings`]: the finding of each path, by the one decision
//!   ([`disk_finding`]), which has no disk and no store in it;
//! - [`write_order`]: which finding is written before which, so that the schema accepts
//!   each write; what a finding writes is [`written_rows`]'s, through [`ItemStore`] and
//!   over [`local_places`], the places as the writes so far leave them.
//!
//! Two rules stand over all of it. What the person could not see is not removed: an item of
//! OneDrive with nothing of its own on disk loses its place and stays OneDrive's
//! ([`DiskFinding::PlaceLost`]). And nothing leaves the folder while part of the backing
//! directory was not read ([`DiskFindings::is_read_whole`]).
//!
//! It reads the backing directory through [`BackingDirectory`] and writes rows through
//! [`ItemStore`]. It changes nothing on disk but one thing: a mark that is not the row's
//! own is taken off the entry that carries it. It knows nothing of the outbox, of
//! `onedrive_items`, of the node table or of the mount.

use std::path::PathBuf;

use konedrive_tree::{DriveId, ItemStore, TreeError};

use super::backing_directory::BackingDirectory;
use super::upload_rules::UploadRules;

mod backing_directory_contents;
mod disk_finding;
mod entry_identities;
mod item_rows;
mod local_places;
mod path_findings;
mod write_order;
mod written_rows;

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

/// What a check answers: how many of each finding it wrote to the rows, the paths it left,
/// and whether it read the whole backing directory. A path that is not among those it
/// left, and its row, agree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fuse) struct DiskFindings {
    counts: [usize; DiskFinding::ALL.len()],
    unresolved: Vec<UnresolvedPath>,
    is_read_whole: bool,
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

    /// Whether the whole backing directory was read. While it was not, what is missing may
    /// be in the part that was not read: nothing was found removed, and the folder sends
    /// nothing before a check has read everything.
    pub(in crate::fuse) fn is_read_whole(&self) -> bool {
        self.is_read_whole
    }

    /// Nothing written yet: the paths that were not read are left already.
    fn begin(unread: Vec<UnresolvedPath>, is_read_whole: bool) -> Self {
        DiskFindings { counts: [0; DiskFinding::ALL.len()], unresolved: unread, is_read_whole }
    }

    /// `path` is left as it was found.
    fn leave(&mut self, path: PathBuf, reason: UnresolvedReason) {
        self.unresolved.push(UnresolvedPath { path, reason });
    }

    /// The answer: the paths that were left by path, each said once for a reason.
    fn finish(mut self) -> Self {
        self.unresolved.sort_by(|a, b| a.path.cmp(&b.path));
        self.unresolved.dedup();
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
    /// The drive of the folder: a mark holds an item's id and no drive, and an entry whose
    /// mark no row has is bound in the drive of the directory it is in, or in this one.
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
        let identities = EntryIdentities::settle(&rows, &contents, self.drive_id);
        let findings = PathFindings::decide(&rows, &contents, &identities);
        Ok(WriteOrder::new(item_store, self.backing_directory, self.upload_rules, self.drive_id, &rows, &contents, &identities, &findings).write())
    }
}

#[cfg(test)]
mod tests;
