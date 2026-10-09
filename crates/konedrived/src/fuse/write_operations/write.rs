//! A file a program writes: opened for writing in the backing directory, where its content
//! lies, and sent once its last writer closed it.
//!
//! A OneDrive file is written in its downloaded copy: from the first change on the outbox
//! holds the file, so nothing from OneDrive replaces the copy, and the file is modified
//! here — what the mount shows of it is the copy's own.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use fuser::{Errno, TimeOrNow};
use konedrive_tree::Row;

use super::WriteOperations;
use crate::fuse::access_mode_switch::AccessMode;
use crate::fuse::backing_directory::DiskEntry;
use crate::fuse::detections::{FileWritten, LocalOnlyFileWrite, OneDriveFileWrite};
use crate::fuse::directory_contents::{DirectoryEntry, EntryState, FileSyncState};
use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::entry_place::EntryPlace;
use crate::fuse::node_table::NodeId;
use crate::fuse::open_handles::{CloseOutcome, Fh};
use crate::fuse::errno::to_errno;
use crate::fuse::upload_rules::UploadStatus;

/// A OneDrive file whose content has to be downloaded before it can be written: its row,
/// and its path in the folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fuse) struct NeededDownload {
    pub(in crate::fuse) row: Row,
    pub(in crate::fuse) path: PathBuf,
}

impl WriteOperations {
    /// The download that writing the node `node_id` needs first: of a OneDrive file that is
    /// not downloaded and holds something. Not asked for a file that is cut to nothing: its
    /// content is not needed. A read-only folder refuses here, before anything is
    /// downloaded for a write it will not take.
    pub(in crate::fuse) fn find_needed_download(&self, node_id: NodeId) -> Result<Option<NeededDownload>, Errno> {
        self.access_mode_switch.check_read_write()?;
        let Some((entry, rel)) = self.node_resolver.find_entry(node_id) else { return Ok(None) };
        Ok(match entry.state {
            EntryState::File(FileSyncState::RemoteOnly(row)) if row.size > 0 => Some(NeededDownload { row, path: rel }),
            _ => None,
        })
    }

    /// Opens the node `node_id` for writing, with the content it has: the `fh` the program
    /// holds it open by. The copy of a OneDrive file that is not downloaded is the caller's
    /// to download first ([`find_needed_download`](Self::find_needed_download)).
    pub(in crate::fuse) fn open_to_write(&self, node_id: NodeId) -> Result<Fh, Errno> {
        self.access_mode_switch.check_read_write()?;
        let (entry, rel) = self.node_resolver.find_entry(node_id).ok_or(Errno::ENOENT)?;
        let EntryState::File(file) = &entry.state else { return Err(Errno::EISDIR) };
        self.make_writable(file, &rel)?;
        let backing_file = self.backing_directory.open_to_write(&rel, false).map_err(|e| to_errno(&e))?;
        let added = self.add_writer(node_id, &entry, backing_file)?;
        // Its content is the user's own already when it is modified here.
        if matches!(file, FileSyncState::ModifiedHere(..)) {
            self.open_handles.mark_written(node_id);
        }
        Ok(added)
    }

    /// Opens the node `node_id` for writing, cut to nothing (`O_TRUNC`): the `fh` the
    /// program holds it open by. Nothing of a OneDrive file is downloaded for it.
    pub(in crate::fuse) fn open_to_overwrite(&self, node_id: NodeId) -> Result<Fh, Errno> {
        self.access_mode_switch.check_read_write()?;
        let (entry, rel) = self.node_resolver.find_entry(node_id).ok_or(Errno::ENOENT)?;
        let EntryState::File(file) = &entry.state else { return Err(Errno::EISDIR) };
        self.make_writable_without_content(file, &rel)?;
        let backing_file = self.backing_directory.open_to_write(&rel, true).map_err(|e| to_errno(&e))?;
        let added = self.add_writer(node_id, &entry, backing_file)?;
        // Cut to nothing: its content is the user's own from here on.
        self.open_handles.mark_written(node_id);
        Ok(added)
    }

    /// Puts the writer of the node `node_id`, which stands for `entry`, into the open
    /// handles: the `fh` it holds `backing_file` open by. The node's first writer is
    /// recorded: from it on the outbox holds the file. An open that cannot be recorded is
    /// closed again.
    fn add_writer(&self, node_id: NodeId, entry: &DirectoryEntry, backing_file: File) -> Result<Fh, Errno> {
        let added = self.open_handles.add_writer(node_id, entry.onedrive_id().map(str::to_owned), Arc::new(backing_file));
        if added.is_first {
            if let Err(e) = self.record_content(node_id, LocalOnlyFileWrite::Opened) {
                self.open_handles.close_file(added.fh);
                return Err(e);
            }
        }
        Ok(added.fh)
    }

    /// The program closed the file it held open for writing by `fh`. The handle is closed
    /// here, on the write-operation thread, and what its close left is recorded in the same
    /// step: no other write operation — an open of the same file, a cut of it — runs between
    /// the answer of the open handles and the record made from it. Nothing for an `fh` that
    /// holds no file open for writing.
    pub(in crate::fuse) fn close_writer(&self, fh: Fh) {
        if let Some(closed) = self.open_handles.close_file(fh) {
            self.record_close(closed.node_id, closed.outcome);
        }
    }

    /// Records the close of a writer of the node `node_id`, with `outcome`: what it is
    /// recorded as is decided by [`describe_last_close`], from what the node stands for now.
    /// Nothing for a node whose file is gone.
    fn record_close(&self, node_id: NodeId, outcome: CloseOutcome) {
        let (Some((entry, rel)), Some((_, parent_directory))) = (self.node_resolver.find_entry(node_id), self.node_resolver.find_parent(node_id)) else { return };
        let copy = self.backing_directory.find_disk_entry(&rel);
        let upload_status = self.find_upload_status(&parent_directory, &entry.name);
        let node_stands_for_file = entry.onedrive_id().is_some_and(|id| self.node_resolver.find_node_id(id) == Some(node_id));
        let access_mode = self.access_mode_switch.access_mode();
        if let Some(event) = describe_last_close(outcome, access_mode, &entry, &parent_directory, copy, upload_status, node_stands_for_file) {
            let _ = self.record_or_undo(event, format_args!("the close of {}", rel.display()), || {});
        }
    }

    /// Sets the time the node `node_id` was last modified, when `modified` gives one: the
    /// file's own from then on. A read-only folder refuses, also when no time is given.
    /// Called on the write-operation thread, as every write operation: the mode it asked
    /// holds until the time is written.
    pub(in crate::fuse) fn set_modified_time(&self, node_id: NodeId, modified: Option<TimeOrNow>) -> Result<(), Errno> {
        self.access_mode_switch.check_read_write()?;
        self.write_modified_time(node_id, modified);
        Ok(())
    }

    /// Writes the time `modified` gives on the file of the node `node_id` in the backing
    /// directory. Nothing for a node that has no file there.
    fn write_modified_time(&self, node_id: NodeId, modified: Option<TimeOrNow>) {
        let (Some(modified), Some((_, rel))) = (modified, self.node_resolver.find_entry(node_id)) else { return };
        let at = match modified {
            TimeOrNow::SpecificTime(at) => at,
            TimeOrNow::Now => SystemTime::now(),
        };
        if let Ok(file) = self.backing_directory.open_to_read(&rel) {
            let _ = konedrive_fs::placeholder::set_mtime(&file, at);
        }
    }

    /// Cuts or extends the node `node_id` to `size`, and then sets the time it was last
    /// modified, when `modified` gives one.
    pub(in crate::fuse) fn truncate(&self, node_id: NodeId, size: u64, modified: Option<TimeOrNow>) -> Result<(), Errno> {
        self.access_mode_switch.check_read_write()?;
        let (entry, rel) = self.node_resolver.find_entry(node_id).ok_or(Errno::ENOENT)?;
        let EntryState::File(file) = &entry.state else { return Err(Errno::EISDIR) };
        match size {
            0 => self.make_writable_without_content(file, &rel)?,
            _ => self.make_writable(file, &rel)?,
        }
        let backing_file = self.backing_directory.open_to_write(&rel, false).map_err(|e| to_errno(&e))?;
        // Recorded before it is cut: a daemon that dies in between leaves a row for content
        // that did not change, never a changed file without a row.
        if self.open_handles.is_open_for_writing_by_node_id(node_id) {
            self.open_handles.mark_written(node_id);
        } else {
            self.record_content(node_id, LocalOnlyFileWrite::Cut)?;
        }
        backing_file.set_len(size).map_err(|e| to_errno(&e))?;
        self.write_modified_time(node_id, modified);
        Ok(())
    }

    /// The file at `rel` in the backing directory, made ready to be written with the
    /// content it has: the downloaded copy of a OneDrive file made writable. A OneDrive file
    /// that is not downloaded and holds nothing gets its empty copy; one that holds
    /// something is the caller's to download first, and is an error here.
    fn make_writable(&self, file: &FileSyncState, rel: &Path) -> Result<(), Errno> {
        match file {
            FileSyncState::RemoteOnly(row) if row.size > 0 => Err(Errno::EIO),
            FileSyncState::RemoteOnly(_) | FileSyncState::LocalOnly(_) | FileSyncState::Synced(..) | FileSyncState::ModifiedHere(..) => self.make_writable_without_content(file, rel),
        }
    }

    /// The file at `rel` in the backing directory, made ready to be written by whoever does
    /// not need its content — it is cut to nothing: the downloaded copy of a OneDrive file
    /// made writable, and a OneDrive file that is not downloaded given an empty copy,
    /// whatever it holds in OneDrive.
    fn make_writable_without_content(&self, file: &FileSyncState, rel: &Path) -> Result<(), Errno> {
        match file {
            FileSyncState::LocalOnly(_) => return Ok(()),
            FileSyncState::RemoteOnly(row) => {
                self.directory_contents.make_marked_directories(rel.parent().unwrap_or(Path::new(""))).map_err(|e| to_errno(&e))?;
                self.backing_directory.make_file(rel).map_err(|e| to_errno(&e))?;
                self.backing_directory.mark_downloaded_copy(rel, &row.id).map_err(|e| to_errno(&e))?;
            }
            FileSyncState::Synced(..) | FileSyncState::ModifiedHere(..) => {}
        }
        self.backing_directory.grant_write_bit(rel).map_err(|e| to_errno(&e))
    }

    /// Records that the content of the node `node_id` is the user's own now, by `write`:
    /// still being written, closed, or cut ([`describe_file_write`]).
    fn record_content(&self, node_id: NodeId, write: LocalOnlyFileWrite) -> Result<(), Errno> {
        // Gone since: removed while it was open.
        let (Some((entry, rel)), Some((_, parent_directory))) = (self.node_resolver.find_entry(node_id), self.node_resolver.find_parent(node_id)) else { return Ok(()) };
        let copy = self.backing_directory.find_disk_entry(&rel);
        let upload_status = self.find_upload_status(&parent_directory, &entry.name);
        match describe_file_write(&entry, &parent_directory, copy, upload_status, write) {
            Some(event) => self.record_or_undo(event, format_args!("the change of {}", rel.display()), || {}),
            None => Ok(()),
        }
    }
}

/// What a change of the content of the file `entry`, in `parent_directory`, is recorded as:
/// `write` says what was done to it. `copy` is what the backing directory holds at its path,
/// and `upload_status` what the upload rules say of a local-only file there. `None` when
/// nothing is recorded:
///
/// - a local-only file that is ignored at its place;
/// - a file of OneDrive the backing directory holds no copy of: there is no content of the
///   user's own to send.
pub(super) fn describe_file_write(entry: &DirectoryEntry, parent_directory: &DirectoryReference, copy: Option<DiskEntry>, upload_status: UploadStatus, write: LocalOnlyFileWrite) -> Option<FileWritten> {
    if let Some(local_only) = entry.local_only() {
        return (upload_status == UploadStatus::Uploaded).then(|| FileWritten::LocalOnly { file: local_only.clone(), write });
    }
    let (row, copy) = (entry.row()?, copy?);
    let write = match write {
        LocalOnlyFileWrite::Opened => OneDriveFileWrite::Opened(copy),
        LocalOnlyFileWrite::Closed => OneDriveFileWrite::ClosedChanged(copy),
        LocalOnlyFileWrite::Cut => OneDriveFileWrite::Cut(copy),
    };
    Some(FileWritten::InOneDrive { onedrive_id: row.id.clone(), place: EntryPlace::new(parent_directory, &entry.name), write })
}

/// What the close of a writer of the file `entry` is recorded as, for the `outcome` of the
/// close and the folder's `access_mode`; `None` when nothing is.
///
/// - Still open for writing elsewhere: nothing.
/// - In a read-only folder: nothing — what a program still held open when the folder was
///   switched to read-only is not sent.
/// - A file of OneDrive closed with nothing written: the outbox lets go of it — unless the
///   node no longer stands for the file (`node_stands_for_file`): its name was taken over by
///   a file saved over it, and the node says nothing of the file's content.
/// - A file whose content changed, and a local-only file whatever was written — it was
///   never sent: its content is due ([`describe_file_write`]).
pub(super) fn describe_last_close(
    outcome: CloseOutcome,
    access_mode: AccessMode,
    entry: &DirectoryEntry,
    parent_directory: &DirectoryReference,
    copy: Option<DiskEntry>,
    upload_status: UploadStatus,
    node_stands_for_file: bool,
) -> Option<FileWritten> {
    match (outcome, access_mode, entry.row()) {
        (CloseOutcome::StillOpenElsewhere, _, _) | (_, AccessMode::ReadOnly, _) => None,
        (CloseOutcome::ContentUntouched, AccessMode::ReadWrite, Some(row)) => {
            node_stands_for_file.then(|| FileWritten::InOneDrive { onedrive_id: row.id.clone(), place: EntryPlace::new(parent_directory, &row.name), write: OneDriveFileWrite::ClosedUntouched })
        }
        (CloseOutcome::ContentChanged, AccessMode::ReadWrite, _) | (CloseOutcome::ContentUntouched, AccessMode::ReadWrite, None) => describe_file_write(entry, parent_directory, copy, upload_status, LocalOnlyFileWrite::Closed),
    }
}

#[cfg(test)]
mod tests;
