//! A file a program writes: opened for writing in the backing directory, where its content
//! lies, and sent once its last writer closed it.
//!
//! A OneDrive file is written in its downloaded copy: from the first change on the outbox
//! holds the file, so nothing from OneDrive replaces the copy, and the file is modified
//! here — what the mount shows of it is the copy's own.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use fuser::{Errno, TimeOrNow};

use super::WriteOperations;
use crate::fuse::access_mode_switch::AccessMode;
use crate::fuse::backing_directory::DiskEntry;
use crate::fuse::detections::{FileWritten, LocalOnlyFileWrite, OneDriveFileWrite};
use crate::fuse::directory_contents::{DirectoryEntry, EntryState, FileSyncState};
use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::entry_place::EntryPlace;
use crate::fuse::node_table::NodeId;
use crate::fuse::open_handles::{CloseOutcome, Fh, WriterRank};
use crate::fuse::errno::to_errno;
use crate::fuse::file_downloader::FileToDownload;
use crate::fuse::upload_rules::UploadDecision;

/// What a write operation did to a file, of OneDrive or local-only: what is recorded of it
/// is built from this ([`describe_file_write`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileWrite {
    /// Its first writer opened it.
    Opened,
    /// Its last writer closed it, and its content is to be sent.
    Closed,
    /// It was truncated or extended by a program that does not hold it open.
    Truncated,
}

impl WriteOperations {
    /// The file that writing the node `node_id` needs downloaded first: a OneDrive file that
    /// is not downloaded and holds something. Not asked for a file that is truncated to nothing: its
    /// content is not needed. A read-only folder refuses here, before anything is
    /// downloaded for a write it will not take.
    pub(in crate::fuse) fn find_file_to_download(&self, node_id: NodeId) -> Result<Option<FileToDownload>, Errno> {
        self.access_mode_switch.check_read_write()?;
        let Some((entry, rel)) = self.node_resolver.find_entry(node_id) else { return Ok(None) };
        Ok(match entry.state {
            EntryState::File(FileSyncState::RemoteOnly(row)) if row.size > 0 => Some(FileToDownload { row, path: rel }),
            _ => None,
        })
    }

    /// The file the node `node_id` stands for, for an operation that writes it: its entry,
    /// its sync state and its path in the folder. Refused in a read-only folder (`EROFS`),
    /// for a node whose file is gone (`ENOENT`) and for a directory (`EISDIR`).
    fn find_file_to_write(&self, node_id: NodeId) -> Result<(DirectoryEntry, FileSyncState, PathBuf), Errno> {
        self.access_mode_switch.check_read_write()?;
        let (entry, path) = self.node_resolver.find_entry(node_id).ok_or(Errno::ENOENT)?;
        match entry.state.clone() {
            EntryState::File(file) => Ok((entry, file, path)),
            EntryState::Directory(_) => Err(Errno::EISDIR),
        }
    }

    /// Opens the node `node_id` for writing, with the content it has: the `fh` the program
    /// holds it open by. The copy of a OneDrive file that is not downloaded is the caller's
    /// to download first ([`find_file_to_download`](Self::find_file_to_download)).
    pub(in crate::fuse) fn open_to_write(&self, node_id: NodeId) -> Result<Fh, Errno> {
        let (entry, file, path) = self.find_file_to_write(node_id)?;
        self.make_writable(&file, &path)?;
        self.add_writer(node_id, &entry, self.backing_directory.open_to_write(&path))
    }

    /// Opens the node `node_id` for writing, truncated to nothing (`O_TRUNC`): the `fh` the
    /// program holds it open by. Nothing of a OneDrive file is downloaded for it.
    pub(in crate::fuse) fn open_to_overwrite(&self, node_id: NodeId) -> Result<Fh, Errno> {
        let (entry, file, path) = self.find_file_to_write(node_id)?;
        self.make_writable_without_content(&file, &path)?;
        let fh = self.add_writer(node_id, &entry, self.backing_directory.open_to_overwrite(&path))?;
        // Truncated to nothing: its content is the user's own from here on.
        self.open_handles.mark_written(node_id);
        Ok(fh)
    }

    /// Puts the writer of the node `node_id`, which stands for `entry`, into the open
    /// handles: the `fh` it holds `opened`, its file in the backing directory, open by. The
    /// node's first writer is recorded: from it on the outbox holds the file. An open that
    /// cannot be recorded is closed again. A file that was modified here before is marked
    /// written: its content is the user's own already.
    fn add_writer(&self, node_id: NodeId, entry: &DirectoryEntry, opened: io::Result<File>) -> Result<Fh, Errno> {
        let backing_file = opened.map_err(|e| to_errno(&e))?;
        let (fh, rank) = self.open_handles.add_writer(node_id, entry.onedrive_id().map(str::to_owned), Arc::new(backing_file));
        if rank == WriterRank::First {
            if let Err(e) = self.record_file_write(node_id, "change", |entry, parent_directory, copy, upload_decision| describe_file_write(entry, parent_directory, copy, upload_decision, FileWrite::Opened)) {
                self.open_handles.close_file(fh);
                return Err(e);
            }
        }
        if matches!(entry.state, EntryState::File(FileSyncState::ModifiedHere(..))) {
            self.open_handles.mark_written(node_id);
        }
        Ok(fh)
    }

    /// The program closed the file it held open for writing by `fh`. The handle is closed
    /// here, on the write-operation thread, and what its close left is recorded in the same
    /// step: no other write operation — an open of the same file, a truncate of it — runs between
    /// the answer of the open handles and the record made from it. Nothing for an `fh` that
    /// holds no file open for writing.
    pub(in crate::fuse) fn close_writer(&self, fh: Fh) {
        if let Some(closed) = self.open_handles.close_file(fh) {
            self.record_close(closed.node_id, closed.outcome);
        }
    }

    /// Records the close of a writer of the node `node_id`, with `outcome`: what it is
    /// recorded as is decided by [`describe_last_close`], from what the node stands for now.
    fn record_close(&self, node_id: NodeId, outcome: CloseOutcome) {
        let access_mode = self.access_mode_switch.access_mode();
        let _ = self.record_file_write(node_id, "close", |entry, parent_directory, copy, upload_decision| {
            let node_stands_for_file = entry.onedrive_id().is_some_and(|id| self.node_resolver.find_node_id(id) == Some(node_id));
            describe_last_close(outcome, access_mode, entry, parent_directory, copy, upload_decision, node_stands_for_file)
        });
    }

    /// Records what `describe` makes of a write of the file the node `node_id` stands for
    /// now, named `what` in the log: it is given the file's entry, the directory it is in,
    /// what the backing directory holds at its path and what the upload rules say of a
    /// local-only file there, and gives what is recorded, if anything is. Nothing for a node
    /// whose file is gone: removed while it was open.
    fn record_file_write(&self, node_id: NodeId, what: &str, describe: impl FnOnce(&DirectoryEntry, &DirectoryReference, Option<DiskEntry>, UploadDecision) -> Option<FileWritten>) -> Result<(), Errno> {
        let (Some((entry, path)), Some((_, parent_directory))) = (self.node_resolver.find_entry(node_id), self.node_resolver.find_parent(node_id)) else { return Ok(()) };
        let copy = self.backing_directory.find_disk_entry(&path);
        let upload_decision = self.decide_upload(&parent_directory, &entry.name);
        match describe(&entry, &parent_directory, copy, upload_decision) {
            Some(event) => self.record_or_undo(event, format_args!("the {what} of {}", path.display()), || {}),
            None => Ok(()),
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

    /// Truncates or extends the node `node_id` to `size`, and then sets the time it was last
    /// modified, when `modified` gives one.
    pub(in crate::fuse) fn truncate(&self, node_id: NodeId, size: u64, modified: Option<TimeOrNow>) -> Result<(), Errno> {
        let (_, file, path) = self.find_file_to_write(node_id)?;
        match size {
            0 => self.make_writable_without_content(&file, &path)?,
            _ => self.make_writable(&file, &path)?,
        }
        let backing_file = self.backing_directory.open_to_write(&path).map_err(|e| to_errno(&e))?;
        // Recorded before it is truncated: a daemon that dies in between leaves a row for content
        // that did not change, never a changed file without a row.
        if self.open_handles.is_open_for_writing_by_node_id(node_id) {
            self.open_handles.mark_written(node_id);
        } else {
            self.record_file_write(node_id, "change", |entry, parent_directory, copy, upload_decision| describe_file_write(entry, parent_directory, copy, upload_decision, FileWrite::Truncated))?;
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
    /// not need its content — it is truncated to nothing: the downloaded copy of a OneDrive file
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
}

/// What a change of the content of the file `entry`, in `parent_directory`, is recorded as:
/// `write` says what was done to it. `copy` is what the backing directory holds at its path,
/// and `upload_decision` what the upload rules say of a local-only file there. `None` when
/// nothing is recorded:
///
/// - a local-only file that is ignored at its place;
/// - a file of OneDrive the backing directory holds no copy of: there is no content of the
///   user's own to send.
pub(super) fn describe_file_write(entry: &DirectoryEntry, parent_directory: &DirectoryReference, copy: Option<DiskEntry>, upload_decision: UploadDecision, write: FileWrite) -> Option<FileWritten> {
    if let Some(local_only) = entry.local_only() {
        let write = match write {
            FileWrite::Opened => LocalOnlyFileWrite::Opened,
            FileWrite::Closed => LocalOnlyFileWrite::Closed,
            FileWrite::Truncated => LocalOnlyFileWrite::Truncated,
        };
        return (upload_decision == UploadDecision::Upload).then(|| FileWritten::LocalOnly { file: local_only.clone(), write });
    }
    let (row, copy) = (entry.row()?, copy?);
    // A close that comes here left content to send: one that left a OneDrive file untouched
    // is told apart by its `CloseOutcome`, in `describe_last_close`.
    let write = match write {
        FileWrite::Opened => OneDriveFileWrite::Opened(copy),
        FileWrite::Closed => OneDriveFileWrite::ClosedChanged(copy),
        FileWrite::Truncated => OneDriveFileWrite::Truncated(copy),
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
    upload_decision: UploadDecision,
    node_stands_for_file: bool,
) -> Option<FileWritten> {
    match (outcome, access_mode, entry.row()) {
        (CloseOutcome::StillOpenElsewhere, _, _) | (_, AccessMode::ReadOnly, _) => None,
        (CloseOutcome::ContentUntouched, AccessMode::ReadWrite, Some(row)) => {
            node_stands_for_file.then(|| FileWritten::InOneDrive { onedrive_id: row.id.clone(), place: EntryPlace::new(parent_directory, &row.name), write: OneDriveFileWrite::ClosedUntouched })
        }
        (CloseOutcome::ContentChanged, AccessMode::ReadWrite, _) | (CloseOutcome::ContentUntouched, AccessMode::ReadWrite, None) => describe_file_write(entry, parent_directory, copy, upload_decision, FileWrite::Closed),
    }
}

#[cfg(test)]
mod tests;
