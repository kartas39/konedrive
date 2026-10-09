//! A file a program writes: opened for writing in the backing directory, where its content
//! lies, and sent once its last writer closed it.
//!
//! An item's file is written in its downloaded copy, which stays the item's: from the first
//! change on the outbox holds the item, so nothing from OneDrive replaces the copy, and what
//! the mount shows of the file is the copy's own.

use std::path::{Path, PathBuf};

use fuser::Errno;
use konedrive_tree::Row;

use super::{OpenedFile, WriteOperations};
use crate::fuse::detections::{OneDriveFileWrite, LocalOnlyFileWrite, FileWritten};
use crate::fuse::local_only_entries::{folder_place, local_object};
use crate::fuse::shared::errno;
use crate::fuse::directory_contents::Entry;

impl WriteOperations {
    /// The item's file and its place, when writing node `ino` needs its content downloaded
    /// first: an item's file that is not downloaded and is not cut to nothing (`empty`).
    pub(in crate::fuse) fn needs_download(&self, node_id: u64, empty: bool) -> Option<(Row, PathBuf)> {
        match self.node_lookup.find_entry(node_id)? {
            (Entry::Item(row), rel) if !empty && row.size > 0 && !self.backing_directory.has_downloaded_copy(&rel, &row.id) => Some((row, rel)),
            _ => None,
        }
    }

    /// Opens node `ino` for writing, cut to nothing when `truncate`.
    pub(in crate::fuse) fn open_to_write(&self, node_id: u64, truncate: bool) -> Result<OpenedFile, Errno> {
        self.check_writable()?;
        let (entry, rel) = self.node_lookup.find_entry(node_id).ok_or(Errno::ENOENT)?;
        if entry.is_dir() {
            return Err(Errno::EISDIR);
        }
        self.make_writable(&entry, &rel, truncate)?;
        let file = self.backing_directory.write(&rel, truncate).map_err(|e| errno(&e))?;
        // Its content is the user's own already when the outbox holds a change of it; from
        // the first writer on the outbox holds it in any case.
        let own = truncate || entry.id().is_some_and(|id| self.directory_contents.unsent_changes().changed.contains(id));
        if self.open_writers.add_writer(node_id, own) {
            if let Err(e) = self.record_content(node_id, LocalOnlyFileWrite::Opened) {
                self.open_writers.remove_writer(node_id);
                return Err(e);
            }
        }
        Ok(OpenedFile { node_id, id: entry.id().map(str::to_owned), rel, file })
    }

    /// The last writer of node `ino` closed it: its content is ready to be sent when it was
    /// `changed`; when nothing was written, the outbox lets go of an item's file.
    pub(in crate::fuse) fn closed(&self, node_id: u64, changed: bool) {
        let untouched = match self.node_lookup.find_entry(node_id) {
            Some((Entry::Item(row), _)) if !changed => row,
            // A local file is sent all the same: it was never sent.
            _ => {
                let _ = self.record_content(node_id, LocalOnlyFileWrite::Closed);
                return;
            }
        };
        // A node the item no longer is — its name was taken over by a file saved over it —
        // says nothing of the item's content.
        if self.node_lookup.find_node_id(&untouched.id) != Some(node_id) {
            return;
        }
        let Some((_, above)) = self.node_lookup.find_parent(node_id) else { return };
        let event = FileWritten::InOneDrive { id: untouched.id.clone(), place: folder_place(&above, &untouched.name), write: OneDriveFileWrite::ClosedUntouched };
        if let Err(e) = self.outbox_recorder.record(event) {
            tracing::warn!("a file closed unchanged could not be let go of: {e}");
        }
    }

    /// Cuts or extends node `ino` to `size`.
    pub(in crate::fuse) fn truncate(&self, node_id: u64, size: u64) -> Result<(), Errno> {
        self.check_writable()?;
        let (entry, rel) = self.node_lookup.find_entry(node_id).ok_or(Errno::ENOENT)?;
        if entry.is_dir() {
            return Err(Errno::EISDIR);
        }
        self.make_writable(&entry, &rel, size == 0)?;
        let file = self.backing_directory.write(&rel, false).map_err(|e| errno(&e))?;
        // Recorded before it is cut: a daemon that dies in between leaves a row for content
        // that did not change, never a changed file without a row.
        if self.open_writers.is_open(node_id) {
            self.open_writers.mark_written(node_id);
        } else {
            self.record_content(node_id, LocalOnlyFileWrite::Cut)?;
        }
        file.set_len(size).map_err(|e| errno(&e))
    }

    /// The file of an entry at `rel` in the backing directory, made ready to be written: an
    /// item's downloaded copy made writable, or made empty when it is to be cut to nothing
    /// (`empty`). A copy that is needed and missing is the caller's to download first.
    fn make_writable(&self, entry: &Entry, rel: &Path, empty: bool) -> Result<(), Errno> {
        let Entry::Item(row) = entry else { return Ok(()) };
        if !self.backing_directory.has_downloaded_copy(rel, &row.id) {
            if !empty && row.size > 0 {
                return Err(Errno::EIO);
            }
            self.directory_contents.make_marked_directories(rel.parent().unwrap_or(Path::new(""))).map_err(|e| errno(&e))?;
            self.backing_directory.make_file(rel).map_err(|e| errno(&e))?;
            self.backing_directory.mark_downloaded_copy(rel, &row.id).map_err(|e| errno(&e))?;
        }
        self.backing_directory.writable(rel).map_err(|e| errno(&e))
    }

    /// Records that the content of node `ino` is the user's own now, by `write`: still being
    /// written, closed, or cut.
    fn record_content(&self, node_id: u64, write: LocalOnlyFileWrite) -> Result<(), Errno> {
        // Gone since: removed while it was open.
        let (Some((entry, rel)), Some((_, above))) = (self.node_lookup.find_entry(node_id), self.node_lookup.find_parent(node_id)) else { return Ok(()) };
        let Some(object) = self.backing_directory.find_disk_entry(&rel) else { return Ok(()) };
        let (place, file) = (folder_place(&above, entry.name()), local_object(&object));
        let event = match entry {
            Entry::Local(_) if self.stays_local(&above, &place.name) => return Ok(()),
            Entry::Local(_) => FileWritten::LocalOnly { place, file, write },
            Entry::Item(row) => {
                let write = match write {
                    LocalOnlyFileWrite::Opened => OneDriveFileWrite::Opened(file),
                    LocalOnlyFileWrite::Closed => OneDriveFileWrite::ClosedChanged(file),
                    LocalOnlyFileWrite::Cut => OneDriveFileWrite::Cut(file),
                };
                FileWritten::InOneDrive { id: row.id, place, write }
            }
        };
        self.record_or_undo(event, format_args!("the change of {}", rel.display()), || {})
    }
}

#[cfg(test)]
mod tests;
