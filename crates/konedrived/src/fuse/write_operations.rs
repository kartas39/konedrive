//! The operations a program asks for that change the folder: make, remove, rename and
//! write, each in a module of its own.
//!
//! [`WriteOperations`] holds the parts the operations use and nothing else: what each
//! directory holds ([`DirectoryContents`]), the disk ([`BackingDirectory`]), the node ids
//! ([`NodeResolver`]), what programs hold open ([`OpenHandles`]), the [`OutboxRecorder`],
//! the [`UploadRules`] and the [`AccessMode`]. It owns no state of its own. Each operation is the same short
//! sequence of steps:
//!
//! 1. check, through the directory contents and the rules: what is where, and the errno of a
//!    refusal;
//! 2. do it in the backing directory;
//! 3. record it: build the event ([`super::detections`]) and hand it to the recorder — on a
//!    failure what was done on disk is undone and the program is answered `EIO`
//!    ([`WriteOperations::record_or_undo`]);
//! 4. the node ids, through the node resolver.
//!
//! An operation that opens a file for writing puts it into the open handles itself, so that
//! whether it is the file's first writer — the one the outbox is told of — is answered with
//! the handle added, in one step; it returns the `fh`, which the request answers with.
//!
//! - [`make`]: a file or directory made;
//! - [`remove`]: one removed;
//! - [`rename`]: one renamed or moved;
//! - [`write`](mod@write): a file opened for writing, closed, cut;
//! - [`unrecorded_entries`]: the local-only entries the outbox has no row for, recorded once
//!   the folder takes changes.

use std::fmt::Display;
use std::sync::Arc;

use fuser::Errno;

use super::access_mode::AccessMode;
use super::backing_directory::BackingDirectory;
use super::detections::FolderEvent;
use super::directory_reference::DirectoryReference;
use super::directory_contents::DirectoryContents;
use super::node_resolver::NodeResolver;
use super::open_handles::OpenHandles;
use super::outbox_recorder::OutboxRecorder;
use super::upload_rules::UploadRules;

mod make;
mod remove;
mod rename;
mod unrecorded_entries;
mod write;

pub(super) struct WriteOperations {
    directory_contents: Arc<DirectoryContents>,
    backing_directory: Arc<BackingDirectory>,
    node_resolver: Arc<NodeResolver>,
    open_handles: Arc<OpenHandles>,
    outbox_recorder: Arc<OutboxRecorder>,
    upload_rules: Arc<UploadRules>,
    access_mode: Arc<AccessMode>,
}

impl WriteOperations {
    pub(super) fn new(
        directory_contents: Arc<DirectoryContents>,
        backing_directory: Arc<BackingDirectory>,
        node_resolver: Arc<NodeResolver>,
        open_handles: Arc<OpenHandles>,
        outbox_recorder: Arc<OutboxRecorder>,
        upload_rules: Arc<UploadRules>,
        access_mode: Arc<AccessMode>,
    ) -> Self {
        WriteOperations { directory_contents, backing_directory, node_resolver, open_handles, outbox_recorder, upload_rules, access_mode }
    }

    /// Whether what is `name` in `directory` and is not in OneDrive is ignored.
    fn is_ignored(&self, directory: &DirectoryReference, name: &str) -> bool {
        self.upload_rules.is_ignored(&directory.path, name, |at| self.backing_directory.read_onedrive_id(at).is_some())
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
