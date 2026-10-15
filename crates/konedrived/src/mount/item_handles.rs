//! What programs hold open, by the kernel's `fh`, each file by the item it is.
//!
//! **State owned:** the open files and directories by `fh`; which items are open for
//! writing and whether anything was written through a handle. Changed by the kernel's
//! open, create, release, opendir and releasedir alone, through the functions here.
//!
//! **Answers for:** what is open for writing: the backing file of an item's writer, for
//! the size and the times of a file open for writing (a rule of correctness:
//! `docs/design/decisions.md` §2.2), and yes or no for one item (free-up, the
//! steps). The plan's `items_open_for_writing` is NOT read here: it is read from the rows
//! of the plan's own moment (`content_before_write`, which the row-first order keeps set
//! from before the first writer is added until after the last one left).
//!
//! **The last close** is three steps, so that getattr never finds a file with neither a
//! writer nor its new size in the row: [`find_closing_writer`](ItemHandles::find_closing_writer)
//! says what the close will leave, the row is written, and only then
//! [`remove_writer`](ItemHandles::remove_writer) takes the writer out.
//!
//! **Tested without the rest:**
// test: the_first_writer_of_an_item_is_told_so_and_a_second_is_not
// test: the_backing_file_of_an_items_writer_is_found_by_the_item
// test: an_item_with_no_writer_has_no_writers_file
// test: the_last_close_says_whether_anything_was_written_and_the_writer_stays_until_it_is_removed
// test: a_writer_that_is_closing_still_gives_its_file_for_the_size
// test: a_reader_not_on_disk_keeps_the_content_it_was_opened_as
// test: a_reader_that_was_not_on_disk_goes_on_from_the_backing_file_once_it_landed
// test: a_directory_read_in_parts_reads_the_items_it_listed_first

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::sync::{Arc, Mutex};

use konedrive_tree::{ContentVersion, Item, LocalItemId};


/// A file a program holds open.
#[derive(Clone)]
pub(super) enum OpenItemFile {
    /// Opened to be read while its content was not on disk: nothing of it is in the
    /// backing directory. Read as ranges from OneDrive, of the content it was opened as:
    /// once the row's local content is another (a listing's step took OneDrive's new
    /// content), a read through this handle is `ESTALE`, never bytes of the new content at
    /// the old size.
    ReadingNotOnDisk { item: LocalItemId, content: ContentVersion },
    /// Opened to be read from its file in the backing directory.
    ReadingBackingFile { item: LocalItemId, backing_file: Arc<File> },
    /// Opened for writing, into its file in the backing directory.
    Writing { item: LocalItemId, backing_file: Arc<File> },
}

/// A writer that is being closed: what its close will leave, and its file, which the last
/// close reads the size and the times from. The writer is still among the handles.
#[derive(Clone)]
pub(super) struct ClosingWriter {
    pub(super) item: LocalItemId,
    pub(super) outcome: CloseOutcome,
    pub(super) backing_file: Arc<File>,
}

enum OpenItemHandle {
    File(OpenItemFile),
    /// A directory, with the items it listed when it was first read.
    Directory(Option<Arc<Vec<Item>>>),
}

#[derive(Default)]
struct Handles {
    by_fh: HashMap<Fh, OpenItemHandle>,
    written_items: HashSet<LocalItemId>,
    last_fh: u64,
}

impl Handles {
    /// Keeps `handle` under an `fh` of its own. None is ever `Fh::NOT_OPENED`.
    fn add(&mut self, handle: OpenItemHandle) -> Fh {
        self.last_fh += 1;
        self.by_fh.insert(Fh(self.last_fh), handle);
        Fh(self.last_fh)
    }

    /// The backing files of the writers of `item`, but the one `except` holds.
    fn list_writer_files(&self, item: LocalItemId, except: Option<Fh>) -> impl Iterator<Item = &Arc<File>> {
        self.by_fh.iter().filter_map(move |(fh, handle)| match handle {
            OpenItemHandle::File(OpenItemFile::Writing { item: written, backing_file }) if *written == item && Some(*fh) != except => Some(backing_file),
            _ => None,
        })
    }

    fn has_writer(&self, item: LocalItemId) -> bool {
        self.list_writer_files(item, None).next().is_some()
    }
}

/// The files and directories programs hold open.
#[derive(Default)]
pub(crate) struct ItemHandles {
    handles: Mutex<Handles>,
}

impl ItemHandles {
    /// Nothing is open. Made by the mount's build.
    pub fn new() -> Self {
        Self::default()
    }

    /// A file opened to be read. Called by the open request's read branch.
    pub(super) fn add_file(&self, file: OpenItemFile) -> Fh {
        crate::panic::lock(&self.handles).add(OpenItemHandle::File(file))
    }

    /// A writer of `item` over `backing_file`, and whether it is the first: the first
    /// writer's open is what writes `LocalVersionChange::OpenedForWriting`. Called by the
    /// write operations on the write-operation thread, under the tree lock, after the row
    /// was written (`docs/design/mount.md` §3.3: the row first).
    pub(crate) fn add_writer(&self, item: LocalItemId, backing_file: Arc<File>) -> (Fh, WriterRank) {
        let mut handles = crate::panic::lock(&self.handles);
        let rank = match handles.has_writer(item) {
            true => WriterRank::Later,
            false => WriterRank::First,
        };
        (handles.add(OpenItemHandle::File(OpenItemFile::Writing { item, backing_file })), rank)
    }

    /// What `fh` holds open; `None` for an `fh` that is no file's. Called by read.
    pub(super) fn find_file(&self, fh: Fh) -> Option<OpenItemFile> {
        match crate::panic::lock(&self.handles).by_fh.get(&fh) {
            Some(OpenItemHandle::File(file)) => Some(file.clone()),
            Some(OpenItemHandle::Directory(_)) | None => None,
        }
    }

    /// The item and the backing file a writer's `fh` writes. Called by write and fsync, on
    /// the mount's own threads: no row is touched per write (`docs/design/mount.md` §3.3).
    pub(super) fn find_file_to_write(&self, fh: Fh) -> Option<(LocalItemId, Arc<File>)> {
        match self.find_file(fh)? {
            OpenItemFile::Writing { item, backing_file } => Some((item, backing_file)),
            OpenItemFile::ReadingNotOnDisk { .. } | OpenItemFile::ReadingBackingFile { .. } => None,
        }
    }

    /// The backing file of a writer of `item`, any of them: what getattr, lookup and a
    /// directory reading stat for the size and the times of a file open for writing, and
    /// what the last close reads them from (`docs/design/sync.md` §3). `None` when no program writes it.
    pub(super) fn find_writer_file(&self, item: LocalItemId) -> Option<Arc<File>> {
        crate::panic::lock(&self.handles).list_writer_files(item, None).next().cloned()
    }

    /// Whether a program holds `item` open for writing. Called by free-up and by a step
    /// before it touches or sends the file.
    pub(crate) fn is_open_for_writing(&self, item: LocalItemId) -> bool {
        crate::panic::lock(&self.handles).has_writer(item)
    }

    /// Bytes were written through a handle of `item`, or it was cut: the last close then
    /// says the content changed. Called by write and by a truncation of an open file.
    pub(super) fn mark_written(&self, item: LocalItemId) {
        let mut handles = crate::panic::lock(&self.handles);
        if handles.has_writer(item) {
            handles.written_items.insert(item);
        }
    }

    /// A reader of a file that was not on disk reads `backing_file` from now on: its
    /// download landed. Called by read.
    pub(super) fn switch_to_backing_file(&self, fh: Fh, backing_file: Arc<File>) {
        if let Some(OpenItemHandle::File(file @ OpenItemFile::ReadingNotOnDisk { .. })) = crate::panic::lock(&self.handles).by_fh.get_mut(&fh) {
            let OpenItemFile::ReadingNotOnDisk { item, .. } = *file else { return };
            *file = OpenItemFile::ReadingBackingFile { item, backing_file };
        }
    }

    /// Closes the reader `fh` holds. Called by release, on the mount's thread.
    pub(super) fn close_reader(&self, fh: Fh) {
        let mut handles = crate::panic::lock(&self.handles);
        if matches!(handles.by_fh.get(&fh), Some(OpenItemHandle::File(OpenItemFile::ReadingNotOnDisk { .. } | OpenItemFile::ReadingBackingFile { .. }))) {
            handles.by_fh.remove(&fh);
        }
    }

    /// The writer `fh` holds, and what its close will leave: `StillOpenElsewhere` while
    /// another handle writes the item, else whether anything was written. Nothing is
    /// removed. Called by release of a writer, on the write-operation thread, first.
    pub(super) fn find_closing_writer(&self, fh: Fh) -> Option<ClosingWriter> {
        let handles = crate::panic::lock(&self.handles);
        let Some(OpenItemHandle::File(OpenItemFile::Writing { item, backing_file })) = handles.by_fh.get(&fh) else { return None };
        let outcome = if handles.list_writer_files(*item, Some(fh)).next().is_some() {
            CloseOutcome::StillOpenElsewhere
        } else if handles.written_items.contains(item) {
            CloseOutcome::ContentChanged
        } else {
            CloseOutcome::ContentUntouched
        };
        Some(ClosingWriter { item: *item, outcome, backing_file: Arc::clone(backing_file) })
    }

    /// Takes the writer `fh` out. Called by release of a writer after the row of its last
    /// close was written (or at once, when another handle still writes the item).
    pub(crate) fn remove_writer(&self, fh: Fh) {
        let mut handles = crate::panic::lock(&self.handles);
        let Some(OpenItemHandle::File(OpenItemFile::Writing { item, .. })) = handles.by_fh.get(&fh) else { return };
        let item = *item;
        handles.by_fh.remove(&fh);
        // What was written through the item's handles is kept for as long as one writes it.
        if !handles.has_writer(item) {
            handles.written_items.remove(&item);
        }
    }

    /// A directory opened. Called by opendir.
    pub(super) fn add_directory(&self) -> Fh {
        crate::panic::lock(&self.handles).add(OpenItemHandle::Directory(None))
    }

    /// The items the directory `fh` holds listed at its first reading.
    /// Kept: a directory read in parts reads one listing, whatever is made or removed in
    /// between.
    pub(super) fn find_listed_items(&self, fh: Fh) -> Option<Arc<Vec<Item>>> {
        match crate::panic::lock(&self.handles).by_fh.get(&fh) {
            Some(OpenItemHandle::Directory(listed)) => listed.clone(),
            Some(OpenItemHandle::File(_)) | None => None,
        }
    }

    /// Keeps what the directory `fh` holds listed. Called by the first readdir of an `fh`.
    pub(super) fn keep_listed_items(&self, fh: Fh, listed: Arc<Vec<Item>>) {
        if let Some(OpenItemHandle::Directory(kept)) = crate::panic::lock(&self.handles).by_fh.get_mut(&fh) {
            *kept = Some(listed);
        }
    }

    /// Closes the directory `fh` holds. Called by releasedir.
    pub(super) fn close_directory(&self, fh: Fh) {
        let mut handles = crate::panic::lock(&self.handles);
        if matches!(handles.by_fh.get(&fh), Some(OpenItemHandle::Directory(_))) {
            handles.by_fh.remove(&fh);
        }
    }
}

/// The number the kernel is given for a file or directory a program opened, and names it by
/// in every later request: the kernel's `fh`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Fh(pub(crate) u64);

impl Fh {
    /// The `fh` of a file that was made without being opened: no open file or directory
    /// ever has it.
    pub(super) const NOT_OPENED: Fh = Fh(0);
}

/// Where a writer stands among the handles that write its node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriterRank {
    /// No other handle writes the node: the writer the upload queue is told of.
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

#[cfg(test)]
mod tests;
