//! The operations a program asks for that change the folder: make, remove, rename and
//! write, each in a module of its own.
//!
//! [`WriteOperations`] holds the parts the operations use and nothing else: what each
//! directory holds ([`DirectoryContents`]), the disk ([`BackingDirectory`]), the node ids
//! ([`NodeResolver`]), what programs hold open ([`OpenHandles`]), the [`OutboxRecorder`],
//! the [`UploadRules`] and the [`AccessModeSwitch`]. It owns no state of its own, and runs
//! on the write-operation thread ([`super::write_operation_thread`]), which whoever asks
//! for an operation puts it on: every operation, the time set on a file among them. Each
//! operation asks the access mode at its start, on that thread — where the switch to
//! read-only runs too, so an operation that was let through has recorded, or written, before
//! the folder stops taking changes. Whoever puts an operation on the thread has asked the
//! mode before, to refuse a read-only folder's request without waiting for the thread; the
//! answer on the thread is the one that counts. An operation is then the same short
//! sequence of steps:
//!
//! 1. look up what stands at the places, through the node resolver, the directory contents
//!    and the rules;
//! 2. decide: a function of what was found gives a plan, or the errno of a refusal
//!    ([`make_check`], [`remove_plan`], [`rename_plan`]);
//! 3. do it in the backing directory;
//! 4. record it: build the event ([`super::detections`]) and hand it to the recorder — on a
//!    failure what was done on disk is undone and the program is answered `EIO`
//!    ([`WriteOperations::record_or_undo`]);
//! 5. the node ids, through the node resolver.
//!
//! An operation that opens a file for writing puts it into the open handles itself, so that
//! whether it is the file's first writer — the one the outbox is told of — is answered with
//! the handle added, in one step; it returns the `fh`, which the request answers with.
//!
//! - [`make`]: a file or directory made;
//! - [`remove`]: one removed;
//! - [`rename`]: one renamed or moved;
//! - [`write`](mod@write): a file opened for writing, closed, cut, and the time set on it.

use std::fmt::Display;
use std::path::Path;
use std::sync::Arc;

use fuser::Errno;

use super::access_mode_switch::AccessModeSwitch;
use super::backing_directory::BackingDirectory;
use super::detections::{FolderEvent, WriteState};
use super::directory_contents::{DirectoryContents, DirectoryEntry, EntryState};
use super::directory_reference::DirectoryReference;
use super::node_resolver::NodeResolver;
use super::node_table::NodeId;
use super::open_handles::OpenHandles;
use super::outbox_recorder::OutboxRecorder;
use super::upload_rules::{UploadRules, UploadStatus};

mod make;
mod make_check;
mod remove;
mod remove_plan;
mod rename;
mod rename_plan;
mod write;

pub(super) use make_check::EntryToMake;
pub(super) use rename_plan::RenameMode;

pub(super) struct WriteOperations {
    directory_contents: Arc<DirectoryContents>,
    backing_directory: Arc<BackingDirectory>,
    node_resolver: Arc<NodeResolver>,
    open_handles: Arc<OpenHandles>,
    outbox_recorder: Arc<OutboxRecorder>,
    upload_rules: Arc<UploadRules>,
    access_mode_switch: Arc<AccessModeSwitch>,
}

impl WriteOperations {
    pub(super) fn new(
        directory_contents: Arc<DirectoryContents>,
        backing_directory: Arc<BackingDirectory>,
        node_resolver: Arc<NodeResolver>,
        open_handles: Arc<OpenHandles>,
        outbox_recorder: Arc<OutboxRecorder>,
        upload_rules: Arc<UploadRules>,
        access_mode_switch: Arc<AccessModeSwitch>,
    ) -> Self {
        WriteOperations { directory_contents, backing_directory, node_resolver, open_handles, outbox_recorder, upload_rules, access_mode_switch }
    }

    /// Whether what is `name` in `directory` and is not in OneDrive is uploaded from there.
    fn find_upload_status(&self, directory: &DirectoryReference, name: &str) -> UploadStatus {
        self.upload_rules.find_upload_status(&directory.path, name, |at| self.backing_directory.read_onedrive_id(at).is_some())
    }

    /// Whether a program holds the node `node_id` open for writing now.
    fn find_write_state(&self, node_id: NodeId) -> WriteState {
        match self.open_handles.is_open_for_writing_by_node_id(node_id) {
            true => WriteState::OpenForWriting,
            false => WriteState::NotOpen,
        }
    }

    /// Whether `entry`, which the mount shows at `path`, is a directory that holds
    /// something.
    fn holds_entries(&self, entry: &DirectoryEntry, path: &Path) -> bool {
        let EntryState::Directory(_) = entry.state else { return false };
        let directory = DirectoryReference { onedrive_id: entry.onedrive_id().map(str::to_owned), path: path.to_owned() };
        !self.directory_contents.list_names(&directory).is_empty()
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
