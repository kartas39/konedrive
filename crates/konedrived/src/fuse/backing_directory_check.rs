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
//! - [`write_order`]: the findings written through [`ItemStore`], in an order the schema
//!   accepts, over [`local_places`], the places as the writes so far leave them.
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

pub(in crate::fuse) use disk_finding::{DiskFinding, UnresolvedReason};

use backing_directory_contents::BackingDirectoryContents;
use entry_identities::EntryIdentities;
use item_rows::ItemRows;
use path_findings::PathFindings;
use write_order::{WriteInputs, WriteOrder};

/// A path the check left as it found it, with what is below it, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fuse) struct UnresolvedPath {
    /// Relative to the backing directory.
    pub(in crate::fuse) path: PathBuf,
    pub(in crate::fuse) reason: UnresolvedReason,
}

/// What a check answers: how many of each finding it wrote to the rows, and the paths it
/// left. A path that is not among those it left, and its row, agree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::fuse) struct DiskFindings {
    counts: [usize; DiskFinding::ALL.len()],
    unresolved: Vec<UnresolvedPath>,
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
        let identities = EntryIdentities::settle(&rows, &contents);
        let findings = PathFindings::decide(&rows, &contents, &identities);
        let inputs = WriteInputs { rows: &rows, contents: &contents, identities: &identities, findings: &findings };
        Ok(WriteOrder::new(item_store, self.backing_directory, self.upload_rules, self.drive_id, inputs).write())
    }
}

#[cfg(test)]
mod tests;
