//! The scan for the local-only entries the outbox has no row for, run when the folder starts
//! taking changes ([`UnrecordedEntries`]): one walk of the backing directory, recorded in
//! one go.
//!
//! [`UnrecordedEntryScan`] holds the parts the walk uses and no state of its own: the
//! [`DirectoryContents`], which say what is local-only, the [`BackingDirectory`] it walks,
//! the [`UploadRules`], the [`OutboxRecorder`], and the [`AccessModeSwitch`]. It runs on the
//! write-operation thread, as a write operation does: it records.

use std::sync::Arc;

use super::access_mode_switch::{AccessMode, AccessModeSwitch};
use super::backing_directory::BackingDirectory;
use super::detections::UnrecordedEntries;
use super::directory_contents::DirectoryContents;
use super::directory_reference::DirectoryReference;
use super::local_only_entry::{collect_local_only_entries, LocalOnlyEntry};
use super::outbox_recorder::OutboxRecorder;
use super::upload_rules::UploadRules;

pub(super) struct UnrecordedEntryScan {
    directory_contents: Arc<DirectoryContents>,
    backing_directory: Arc<BackingDirectory>,
    upload_rules: Arc<UploadRules>,
    outbox_recorder: Arc<OutboxRecorder>,
    access_mode_switch: Arc<AccessModeSwitch>,
}

impl UnrecordedEntryScan {
    pub(super) fn new(
        directory_contents: Arc<DirectoryContents>,
        backing_directory: Arc<BackingDirectory>,
        upload_rules: Arc<UploadRules>,
        outbox_recorder: Arc<OutboxRecorder>,
        access_mode_switch: Arc<AccessModeSwitch>,
    ) -> Self {
        UnrecordedEntryScan { directory_contents, backing_directory, upload_rules, outbox_recorder, access_mode_switch }
    }

    /// Records every local-only entry the outbox has no row for; nothing in a folder that
    /// is read-only by now. Called on the write-operation thread.
    pub(super) fn record_unrecorded_entries(&self) {
        if self.access_mode_switch.access_mode() == AccessMode::ReadOnly {
            return;
        }
        let found = self.collect_local_only_entries();
        if found.is_empty() {
            return;
        }
        match self.outbox_recorder.record(UnrecordedEntries(found)) {
            Ok(recorded) if !recorded.applied.queued.is_empty() => {
                tracing::info!("{} file(s) and folder(s) made here were not waiting to be uploaded; they are now", recorded.applied.queued.len());
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("what the folder holds of the user's own could not be looked for: {e}"),
        }
    }

    /// The local-only entries the folder holds, each with what it holds when it is a
    /// directory, without what is ignored: one walk of the directories of OneDrive in the
    /// backing directory.
    fn collect_local_only_entries(&self) -> Vec<LocalOnlyEntry> {
        let mut found = Vec::new();
        let mut directories = vec![self.directory_contents.find_root_directory()];
        while let Some(directory) = directories.pop() {
            for listed in self.backing_directory.list_directory(&directory.path) {
                let Some(name) = listed.file_name().to_str().map(str::to_owned) else { continue };
                let rel = directory.path.join(&name);
                if self.directory_contents.find_local_only_at(&rel).is_some() {
                    found.extend(collect_local_only_entries(&self.backing_directory, &self.upload_rules, &directory, &name));
                } else if listed.file_type().is_ok_and(|kind| kind.is_dir()) {
                    directories.push(DirectoryReference { onedrive_id: self.backing_directory.read_onedrive_id(&rel), path: rel });
                }
            }
        }
        found
    }
}

#[cfg(test)]
mod tests;
