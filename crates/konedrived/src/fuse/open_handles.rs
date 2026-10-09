//! What programs hold open: files and directories, each by the `fh` the kernel was given for
//! it.
//!
//! [`OpenHandles`] is the one owner of the fact "a file is open for writing": it holds every
//! open handle, so the writers of a node are the handles that write it, counted here and
//! nowhere else, and it remembers whether one of them wrote. It knows nothing of the store or
//! of the disk; whoever opens a file brings what the handle is of.
//!
//! What it answers, and who asks:
//!
//! - a file opened to be read ([`OpenHandles::add_file`]), the file an `fh` holds open
//!   ([`OpenHandles::find_file`]), and a handle that reads its file from the backing
//!   directory from now on ([`OpenHandles::switch_to_backing_file`]): the read requests;
//! - a file opened for writing, and whether it is its node's first writer
//!   ([`OpenHandles::add_writer`]): the write operations, which record the first writer in
//!   the outbox;
//! - the file to write through an `fh` ([`OpenHandles::find_file_to_write`]): the write
//!   requests;
//! - something was written to a node ([`OpenHandles::mark_written`]): the write requests,
//!   and the write operation that truncates a file a program holds open;
//! - a file closed ([`OpenHandles::close_file`]), and for one that was open for writing the
//!   node it wrote and what the close left of its content (a [`ClosedFile`] with its
//!   [`CloseOutcome`]): the release request closes a file opened to be read, at once; a
//!   file opened for writing is closed by a write operation, on the write-operation thread,
//!   which decides and records what the close means in the same step — so until then the
//!   file counts as open for writing — and by the one that takes back an open it could not
//!   record;
//! - whether a file is open for writing, by node id
//!   ([`OpenHandles::is_open_for_writing_by_node_id`]): the write operations (a rename, a
//!   truncate), and, for the node that stands for a OneDrive id, the `Files` interface (a
//!   free-up, how a path stands) and the cycle step. A handle says which node it writes and
//!   nothing else: which OneDrive file that is, is the node table's alone to say
//!   ([`NodeResolver::find_node_id`](super::node_resolver::NodeResolver::find_node_id)) — a
//!   local-only file saved over a OneDrive file by rename is that file from then on, and so
//!   is the handle that writes it. The sync state does not say whether a file is open for
//!   writing: a file is modified here only while the outbox holds its row, and a forced
//!   switch to read-only drops that row under a program that still writes the file;
//! - an open directory and the names it listed when it was first read
//!   ([`OpenHandles::add_directory`], [`OpenHandles::find_listed_names`],
//!   [`OpenHandles::keep_listed_names`], [`OpenHandles::close_directory`]): the directory
//!   requests.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::directory_contents::ListedName;
use super::node_table::NodeId;

/// The number the kernel is given for a file or directory a program opened, and names it by
/// in every later request: the kernel's `fh`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Fh(pub(super) u64);

impl Fh {
    /// The `fh` of a file that was made without being opened: no open file or directory
    /// ever has it.
    pub(super) const NOT_OPENED: Fh = Fh(0);
}

/// A file a program holds open: how it was opened, and what exists of it then.
#[derive(Clone)]
pub(super) enum OpenFile {
    /// Opened to be read while it was in OneDrive and not downloaded: nothing of it is in
    /// the backing directory. Its OneDrive id, and its path in the folder when it was
    /// opened.
    ReadingRemoteOnly { onedrive_id: String, path: PathBuf },
    /// Opened to be read from its file in the backing directory.
    ReadingBackingFile { backing_file: Arc<File> },
    /// Opened for writing, into its file in the backing directory: the node id it was
    /// opened by. What that node stands for is the node table's to say.
    Writing { node_id: NodeId, backing_file: Arc<File> },
}

/// What an `fh` holds open.
enum OpenHandle {
    File(OpenFile),
    /// A directory, with the names it listed when it was first read, in the order they are
    /// handed out: a program that reads a directory in parts reads the same names, whatever
    /// changes meanwhile. `None` until it is read.
    Directory(Option<Arc<Vec<ListedName>>>),
}

/// Where a writer stands among the handles that write its node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriterRank {
    /// No other handle writes the node: the writer the outbox is told of.
    First,
    /// Another handle writes the node already.
    Later,
}

/// What closing a file that was open for writing left of its content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CloseOutcome {
    /// Other handles still write the file.
    StillOpenElsewhere,
    /// The last writer closed it, and its content was changed.
    ContentChanged,
    /// The last writer closed it, and nothing was written.
    ContentUntouched,
}

/// A file that was open for writing, closed: the node it wrote, and what the close left of
/// the file's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ClosedFile {
    pub(super) node_id: NodeId,
    pub(super) outcome: CloseOutcome,
}

#[derive(Default)]
struct Handles {
    by_fh: HashMap<Fh, OpenHandle>,
    /// The nodes open for writing whose content was changed through a handle, or was the
    /// user's own before: each for as long as a handle writes it.
    written_node_ids: HashSet<NodeId>,
    /// The `fh` given last. None is ever [`Fh::NOT_OPENED`].
    last_fh: u64,
}

impl Handles {
    fn add(&mut self, handle: OpenHandle) -> Fh {
        self.last_fh += 1;
        self.by_fh.insert(Fh(self.last_fh), handle);
        Fh(self.last_fh)
    }

    /// Whether a handle writes the node `node_id`.
    fn has_writer(&self, node_id: NodeId) -> bool {
        self.by_fh.values().any(|handle| matches!(handle, OpenHandle::File(OpenFile::Writing { node_id: written, .. }) if *written == node_id))
    }
}

/// The files and directories programs hold open, by `fh`.
#[derive(Default)]
pub(super) struct OpenHandles {
    handles: Mutex<Handles>,
}

impl OpenHandles {
    /// A program opened `file` to be read: the `fh` it holds it by from now on.
    pub(super) fn add_file(&self, file: OpenFile) -> Fh {
        crate::panic::lock(&self.handles).add(OpenHandle::File(file))
    }

    /// A program opened the node `node_id` for writing, into `backing_file`. The `fh` it
    /// holds it by, and its rank among the node's writers. A file whose content is the
    /// user's own already is said so next ([`mark_written`](Self::mark_written)).
    pub(super) fn add_writer(&self, node_id: NodeId, backing_file: Arc<File>) -> (Fh, WriterRank) {
        let mut handles = crate::panic::lock(&self.handles);
        let rank = match handles.has_writer(node_id) {
            true => WriterRank::Later,
            false => WriterRank::First,
        };
        (handles.add(OpenHandle::File(OpenFile::Writing { node_id, backing_file })), rank)
    }

    /// The file held open by `fh`.
    pub(super) fn find_file(&self, fh: Fh) -> Option<OpenFile> {
        match crate::panic::lock(&self.handles).by_fh.get(&fh) {
            Some(OpenHandle::File(file)) => Some(file.clone()),
            Some(OpenHandle::Directory(_)) | None => None,
        }
    }

    /// The node id and the file in the backing directory of the file held open by `fh`,
    /// when it was opened for writing.
    pub(super) fn find_file_to_write(&self, fh: Fh) -> Option<(NodeId, Arc<File>)> {
        match self.find_file(fh)? {
            OpenFile::Writing { node_id, backing_file } => Some((node_id, backing_file)),
            OpenFile::ReadingRemoteOnly { .. } | OpenFile::ReadingBackingFile { .. } => None,
        }
    }

    /// The file `fh` holds open to be read was downloaded since it was opened: it is read
    /// from `backing_file` from now on.
    pub(super) fn switch_to_backing_file(&self, fh: Fh, backing_file: Arc<File>) {
        if let Some(OpenHandle::File(file @ OpenFile::ReadingRemoteOnly { .. })) = crate::panic::lock(&self.handles).by_fh.get_mut(&fh) {
            *file = OpenFile::ReadingBackingFile { backing_file };
        }
    }

    /// The content of the node `node_id` is the user's own: something was written to it
    /// through a handle that writes it, or it was so when the handle was added — a file just
    /// made, one truncated to nothing at its open, one that was modified here before.
    pub(super) fn mark_written(&self, node_id: NodeId) {
        let mut handles = crate::panic::lock(&self.handles);
        if handles.has_writer(node_id) {
            handles.written_node_ids.insert(node_id);
        }
    }

    /// The program closed the file it held open by `fh`. For a file that was open for
    /// writing: the node it wrote, and what the close left of its content. `None` when `fh`
    /// held a file open to be read, or no file at all.
    ///
    /// A file open for writing is closed on the write-operation thread alone: what its
    /// close left is true only until the next write operation, so whoever asks acts on it
    /// before another one runs.
    pub(super) fn close_file(&self, fh: Fh) -> Option<ClosedFile> {
        let mut handles = crate::panic::lock(&self.handles);
        // A directory is closed as one ([`close_directory`](Self::close_directory)).
        if !matches!(handles.by_fh.get(&fh), Some(OpenHandle::File(_))) {
            return None;
        }
        let Some(OpenHandle::File(OpenFile::Writing { node_id, .. })) = handles.by_fh.remove(&fh) else { return None };
        let outcome = if handles.has_writer(node_id) {
            CloseOutcome::StillOpenElsewhere
        } else if handles.written_node_ids.remove(&node_id) {
            CloseOutcome::ContentChanged
        } else {
            CloseOutcome::ContentUntouched
        };
        Some(ClosedFile { node_id, outcome })
    }

    /// Whether a program holds the node `node_id` open for writing now.
    pub(super) fn is_open_for_writing_by_node_id(&self, node_id: NodeId) -> bool {
        crate::panic::lock(&self.handles).has_writer(node_id)
    }

    /// A program opened a directory: the `fh` it holds it by. The names it lists are kept
    /// once it is first read ([`keep_listed_names`](Self::keep_listed_names)).
    pub(super) fn add_directory(&self) -> Fh {
        crate::panic::lock(&self.handles).add(OpenHandle::Directory(None))
    }

    /// The names the directory held open by `fh` listed when it was first read.
    pub(super) fn find_listed_names(&self, fh: Fh) -> Option<Arc<Vec<ListedName>>> {
        match crate::panic::lock(&self.handles).by_fh.get(&fh) {
            Some(OpenHandle::Directory(listed_names)) => listed_names.clone(),
            Some(OpenHandle::File(_)) | None => None,
        }
    }

    /// The directory held open by `fh` is read from its start: `listed_names` is what it
    /// holds, for every part of the reading after this one.
    pub(super) fn keep_listed_names(&self, fh: Fh, listed_names: Arc<Vec<ListedName>>) {
        if let Some(OpenHandle::Directory(kept)) = crate::panic::lock(&self.handles).by_fh.get_mut(&fh) {
            *kept = Some(listed_names);
        }
    }

    /// The program closed the directory it held open by `fh`.
    pub(super) fn close_directory(&self, fh: Fh) {
        let mut handles = crate::panic::lock(&self.handles);
        if matches!(handles.by_fh.get(&fh), Some(OpenHandle::Directory(_))) {
            handles.by_fh.remove(&fh);
        }
    }
}

#[cfg(test)]
mod tests;
