//! The operations a program asks for that change the folder: make, remove, rename and
//! write, each in a module of its own.
//!
//! [`WriteOperations`] holds the parts the operations use and nothing else: the folder as
//! the mount shows it ([`DirectoryContents`]) and on disk ([`BackingDirectory`]), the nodes ([`NodeLookup`]), the files
//! open for writing ([`OpenWriters`]), the [`OutboxRecorder`] and the [`UploadRules`]. It owns no state
//! of its own. Each operation is the same short sequence of steps:
//!
//! 1. check, through the view and the rules: what stands where, and the errno of a refusal;
//! 2. do it in the backing directory, through `BackingDirectory`;
//! 3. record it: build the event ([`super::detections`]) and hand it to the recorder — on a
//!    failure what was done on disk is undone and the program is answered `EIO`
//!    ([`WriteOperations::record_or_undo`]);
//! 4. the nodes, through `NodeLookup`.
//!
//! An operation never touches the table of open handles: one that opens a file returns it
//! ([`OpenedFile`]), and the request that asked keeps the handle.
//!
//! - [`make`]: a file or directory made;
//! - [`remove`]: one removed;
//! - [`rename`]: one renamed or moved;
//! - [`write`](mod@write): a file opened for writing, closed, cut;
//! - [`unrecorded_objects`]: what the folder holds that the outbox has no row for, recorded
//!   once the folder takes changes.

use std::fmt::Display;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use fuser::Errno;

use super::detections::FolderEvent;
use super::outbox_recorder::OutboxRecorder;
use super::node_lookup::NodeLookup;
use super::backing_directory::BackingDirectory;
use super::upload_rules::UploadRules;
use super::directory_contents::{Directory, DirectoryContents};
use super::open_writers::OpenWriters;

mod make;
mod remove;
mod rename;
mod unrecorded_entries;
mod write;

pub(super) struct WriteOperations {
    directory_contents: Arc<DirectoryContents>,
    backing_directory: Arc<BackingDirectory>,
    node_lookup: Arc<NodeLookup>,
    open_writers: Arc<OpenWriters>,
    outbox_recorder: Arc<OutboxRecorder>,
    rules: Arc<UploadRules>,
}

/// A file an operation opened for writing: the node, its item when it has one, its place,
/// and the file in the backing directory. Whoever asked keeps the handle.
pub(super) struct OpenedFile {
    pub(super) node_id: u64,
    pub(super) id: Option<String>,
    pub(super) rel: PathBuf,
    pub(super) file: File,
}

impl WriteOperations {
    pub(super) fn new(
        directory_contents: Arc<DirectoryContents>,
        backing_directory: Arc<BackingDirectory>,
        node_lookup: Arc<NodeLookup>,
        open_writers: Arc<OpenWriters>,
        outbox_recorder: Arc<OutboxRecorder>,
        rules: Arc<UploadRules>,
    ) -> Self {
        WriteOperations { directory_contents, backing_directory, node_lookup, open_writers, outbox_recorder, rules }
    }

    /// Refuses a change to a read-only folder as a read-only filesystem does.
    fn check_writable(&self) -> Result<(), Errno> {
        if self.outbox_recorder.is_writable() {
            Ok(())
        } else {
            Err(Errno::EROFS)
        }
    }

    /// Whether what stands as `name` in `dir` and has no item stays on this computer.
    fn stays_local(&self, dir: &Directory, name: &str) -> bool {
        self.rules.stays_local(&dir.rel, name, |at| self.backing_directory.read_onedrive_id(at).is_some())
    }

    /// Records `event`. On a failure it is logged as `what`, `undo` takes back what was done
    /// on disk, and the program is answered `EIO`.
    fn record_or_undo(&self, event: impl FolderEvent, what: impl Display, undo: impl FnOnce()) -> Result<(), Errno> {
        self.outbox_recorder.record(event).map(drop).map_err(|e| {
            tracing::warn!("{what} could not be recorded: {e}");
            undo();
            Errno::EIO
        })
    }
}

#[cfg(test)]
pub(super) mod tests;
