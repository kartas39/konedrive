//! A file a program writes: opened for writing in the backing directory, where its content
//! lies, and sent once its last writer closed it.
//!
//! A OneDrive file is written in its downloaded copy: from the first change on the outbox
//! holds the file, so nothing from OneDrive replaces the copy, and the file is modified
//! here — what the mount shows of it is the copy's own.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use fuser::Errno;
use konedrive_tree::Row;

use super::WriteOperations;
use crate::fuse::detections::{FileWritten, LocalOnlyFileWrite, OneDriveFileWrite};
use crate::fuse::directory_contents::{EntryState, FileSyncState};
use crate::fuse::local_only_entries::folder_place;
use crate::fuse::open_files::OpenFile;
use crate::fuse::errno::to_errno;

impl WriteOperations {
    /// The row of the file and its path, when writing the node `node_id` needs its content
    /// downloaded first: a OneDrive file that is not downloaded and is not cut to nothing
    /// (`empty`).
    pub(in crate::fuse) fn needs_download(&self, node_id: u64, empty: bool) -> Option<(Row, PathBuf)> {
        let (entry, rel) = self.node_lookup.find_entry(node_id)?;
        match entry.state {
            EntryState::File(FileSyncState::RemoteOnly(row)) if !empty && row.size > 0 => Some((row, rel)),
            _ => None,
        }
    }

    /// Opens the node `node_id` for writing, cut to nothing when `truncate`.
    pub(in crate::fuse) fn open_to_write(&self, node_id: u64, truncate: bool) -> Result<OpenFile, Errno> {
        self.access_mode.check_read_write()?;
        let (entry, rel) = self.node_lookup.find_entry(node_id).ok_or(Errno::ENOENT)?;
        let EntryState::File(file) = &entry.state else { return Err(Errno::EISDIR) };
        self.make_writable(file, &rel, truncate)?;
        let backing_file = self.backing_directory.write(&rel, truncate).map_err(|e| to_errno(&e))?;
        // Its content is the user's own already when it is modified here; from the first
        // writer on the outbox holds it in any case.
        let content_changed = truncate || matches!(file, FileSyncState::ModifiedHere(..));
        if self.open_writers.add_writer(node_id, content_changed) {
            if let Err(e) = self.record_content(node_id, LocalOnlyFileWrite::Opened) {
                self.open_writers.remove_writer(node_id);
                return Err(e);
            }
        }
        Ok(OpenFile { node_id, onedrive_id: entry.onedrive_id().map(str::to_owned), rel, file_on_disk: Some(Arc::new(backing_file)), may_write: true })
    }

    /// The last writer of the node `node_id` closed it: its content is ready to be sent when
    /// it was `changed`; when nothing was written, the outbox lets go of a OneDrive file.
    pub(in crate::fuse) fn close_last_writer(&self, node_id: u64, changed: bool) {
        let untouched = self.node_lookup.find_entry(node_id).and_then(|(entry, _)| entry.row().cloned()).filter(|_| !changed);
        // A local-only file is sent all the same: it was never sent.
        let Some(untouched) = untouched else {
            let _ = self.record_content(node_id, LocalOnlyFileWrite::Closed);
            return;
        };
        // A node that no longer stands for the file — its name was taken over by a file saved
        // over it — says nothing of the file's content.
        if self.node_lookup.find_node_id(&untouched.id) != Some(node_id) {
            return;
        }
        let Some((_, parent_directory)) = self.node_lookup.find_parent(node_id) else { return };
        let event = FileWritten::InOneDrive { id: untouched.id.clone(), place: folder_place(&parent_directory, &untouched.name), write: OneDriveFileWrite::ClosedUntouched };
        if let Err(e) = self.outbox_recorder.record(event) {
            tracing::warn!("a file closed unchanged could not be let go of: {e}");
        }
    }

    /// Cuts or extends the node `node_id` to `size`.
    pub(in crate::fuse) fn truncate(&self, node_id: u64, size: u64) -> Result<(), Errno> {
        self.access_mode.check_read_write()?;
        let (entry, rel) = self.node_lookup.find_entry(node_id).ok_or(Errno::ENOENT)?;
        let EntryState::File(file) = &entry.state else { return Err(Errno::EISDIR) };
        self.make_writable(file, &rel, size == 0)?;
        let backing_file = self.backing_directory.write(&rel, false).map_err(|e| to_errno(&e))?;
        // Recorded before it is cut: a daemon that dies in between leaves a row for content
        // that did not change, never a changed file without a row.
        if self.open_writers.is_open(node_id) {
            self.open_writers.mark_written(node_id);
        } else {
            self.record_content(node_id, LocalOnlyFileWrite::Cut)?;
        }
        backing_file.set_len(size).map_err(|e| to_errno(&e))
    }

    /// The file at `rel` in the backing directory, made ready to be written: the downloaded
    /// copy of a OneDrive file made writable, or made empty when the file is not downloaded
    /// and is to be cut to nothing (`empty`). A copy that is needed and missing is the
    /// caller's to download first.
    fn make_writable(&self, file: &FileSyncState, rel: &Path, empty: bool) -> Result<(), Errno> {
        match file {
            FileSyncState::LocalOnly(_) => return Ok(()),
            FileSyncState::RemoteOnly(row) => {
                if !empty && row.size > 0 {
                    return Err(Errno::EIO);
                }
                self.directory_contents.make_marked_directories(rel.parent().unwrap_or(Path::new(""))).map_err(|e| to_errno(&e))?;
                self.backing_directory.make_file(rel).map_err(|e| to_errno(&e))?;
                self.backing_directory.mark_downloaded_copy(rel, &row.id).map_err(|e| to_errno(&e))?;
            }
            FileSyncState::Synced(..) | FileSyncState::ModifiedHere(..) => {}
        }
        self.backing_directory.make_writable(rel).map_err(|e| to_errno(&e))
    }

    /// Records that the content of the node `node_id` is the user's own now, by `write`:
    /// still being written, closed, or cut.
    fn record_content(&self, node_id: u64, write: LocalOnlyFileWrite) -> Result<(), Errno> {
        // Gone since: removed while it was open.
        let (Some((entry, rel)), Some((_, parent_directory))) = (self.node_lookup.find_entry(node_id), self.node_lookup.find_parent(node_id)) else { return Ok(()) };
        if let Some(local_only) = entry.local_only() {
            if self.stays_local(&parent_directory, &entry.name) {
                return Ok(());
            }
            let event = FileWritten::LocalOnly { file: local_only.clone(), write };
            return self.record_or_undo(event, format_args!("the change of {}", rel.display()), || {});
        }
        let (Some(row), Some(copy)) = (entry.row(), self.backing_directory.find_disk_entry(&rel)) else { return Ok(()) };
        let write = match write {
            LocalOnlyFileWrite::Opened => OneDriveFileWrite::Opened(copy),
            LocalOnlyFileWrite::Closed => OneDriveFileWrite::ClosedChanged(copy),
            LocalOnlyFileWrite::Cut => OneDriveFileWrite::Cut(copy),
        };
        let event = FileWritten::InOneDrive { id: row.id.clone(), place: folder_place(&parent_directory, &entry.name), write };
        self.record_or_undo(event, format_args!("the change of {}", rel.display()), || {})
    }
}

#[cfg(test)]
mod tests;
