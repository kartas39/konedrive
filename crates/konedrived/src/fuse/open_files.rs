//! What programs hold open: files and directories, each by the file handle the kernel was
//! given for it.
//!
//! [`OpenFiles`] holds the table and nothing else: it gives the handle numbers, keeps an
//! [`OpenFile`] for each open file, and for each open directory what its entries were made
//! from when it was first read. It knows nothing of the store or of the disk; whoever opens a file brings the
//! [`OpenFile`]. It answers whether a OneDrive file is open for writing
//! ([`OpenFiles::is_open_for_writing`]), which the sync state alone does not say once a
//! switch to read-only dropped the file's row from the outbox under a program that still
//! writes it.

use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::directory_contents::EntrySource;

/// A file a program holds open. A write operation that opens a file returns one, and the
/// request that asked puts it into [`OpenFiles`].
#[derive(Clone)]
pub(super) struct OpenFile {
    /// The node id it was opened by.
    pub(super) node_id: u64,
    /// Its OneDrive id; `None` for a local-only file.
    pub(super) onedrive_id: Option<String>,
    /// Its path in the folder when it was opened.
    pub(super) rel: PathBuf,
    /// The file in the backing directory, once there is one: a OneDrive file that is not
    /// downloaded has none.
    pub(super) file_on_disk: Option<Arc<File>>,
    /// Whether the program may write through this handle.
    pub(super) may_write: bool,
}

/// The files and directories programs hold open, by file handle.
pub(super) struct OpenFiles {
    files: Mutex<HashMap<u64, OpenFile>>,
    /// What the entries of each open directory were made from when it was first read, in
    /// the order they are handed out: a program that reads a directory in parts reads the
    /// same names, whatever changes meanwhile.
    directories: Mutex<HashMap<u64, Arc<Vec<EntrySource>>>>,
    /// The file handle given next. None is ever 0: that is the handle of a file made
    /// without being opened.
    next_handle: AtomicU64,
}

impl Default for OpenFiles {
    fn default() -> Self {
        OpenFiles { files: Mutex::default(), directories: Mutex::default(), next_handle: AtomicU64::new(1) }
    }
}

impl OpenFiles {
    fn take_next_handle(&self) -> u64 {
        self.next_handle.fetch_add(1, Ordering::SeqCst)
    }

    /// A program opened `file`: the file handle it holds it by from now on.
    pub(super) fn add_file(&self, file: OpenFile) -> u64 {
        let handle = self.take_next_handle();
        crate::panic::lock(&self.files).insert(handle, file);
        handle
    }

    /// The file held open by `handle`.
    pub(super) fn find_file(&self, handle: u64) -> Option<OpenFile> {
        crate::panic::lock(&self.files).get(&handle).cloned()
    }

    /// The file held open by `handle` has `file_on_disk` in the backing directory from now
    /// on: it was downloaded since it was opened.
    pub(super) fn set_file_on_disk(&self, handle: u64, file_on_disk: Arc<File>) {
        if let Some(file) = crate::panic::lock(&self.files).get_mut(&handle) {
            file.file_on_disk = Some(file_on_disk);
        }
    }

    /// The program closed `handle`: the file it held open by it.
    pub(super) fn remove_file(&self, handle: u64) -> Option<OpenFile> {
        crate::panic::lock(&self.files).remove(&handle)
    }

    /// Whether a program holds the OneDrive file `onedrive_id` open for writing now.
    pub(super) fn is_open_for_writing(&self, onedrive_id: &str) -> bool {
        crate::panic::lock(&self.files).values().any(|file| file.may_write && file.onedrive_id.as_deref() == Some(onedrive_id))
    }

    /// A program opened a directory: the file handle it holds it by. What the directory
    /// holds is kept once it is first read ([`keep_entry_sources`](Self::keep_entry_sources)).
    pub(super) fn add_directory(&self) -> u64 {
        self.take_next_handle()
    }

    /// What the entries of the directory held open by `handle` were made from when it was
    /// first read.
    pub(super) fn find_entry_sources(&self, handle: u64) -> Option<Arc<Vec<EntrySource>>> {
        crate::panic::lock(&self.directories).get(&handle).cloned()
    }

    /// The directory held open by `handle` is read from its start: `entry_sources` is what
    /// it holds, for every part of the reading after this one.
    pub(super) fn keep_entry_sources(&self, handle: u64, entry_sources: Arc<Vec<EntrySource>>) {
        crate::panic::lock(&self.directories).insert(handle, entry_sources);
    }

    /// The program closed the directory it held open by `handle`.
    pub(super) fn remove_directory(&self, handle: u64) {
        crate::panic::lock(&self.directories).remove(&handle);
    }
}

#[cfg(test)]
mod tests;
