//! What programs hold open, by the kernel's `fh`, each file by the item it is: what
//! `open_handles` is today by node id, when the node id is the local item id.
//!
//! **State owned:** the open files and directories by `fh`; which items are open for
//! writing and whether anything was written through a handle. Changed by the kernel's
//! open, create, release, opendir and releasedir alone, through the functions here.
//!
//! **Answers for:** what is open for writing (the map's §3 B): the backing file of an
//! item's writer, for the size and the times of a file open for writing (design §4, a
//! rule of correctness: §13's measurement), and yes or no for one item (free-up, the
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
#![allow(dead_code, unused_variables)]

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::sync::{Arc, Mutex};

use konedrive_tree::{ContentVersion, Item, LocalItemId};

use super::open_handles::{CloseOutcome, Fh, WriterRank};

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

/// The files and directories programs hold open.
#[derive(Default)]
pub struct ItemHandles {
    handles: Mutex<Handles>,
}

impl ItemHandles {
    /// Nothing is open. Made by the mount's build.
    pub fn new() -> Self {
        todo!()
    }

    /// A file opened to be read. Called by the open request's read branch.
    pub(super) fn add_file(&self, file: OpenItemFile) -> Fh {
        todo!()
    }

    /// A writer of `item` over `backing_file`, and whether it is the first: the first
    /// writer's open is what writes `LocalVersionChange::OpenedForWriting`. Called by the
    /// write operations on the write-operation thread, under the tree lock, after the row
    /// was written (design §8: the row first).
    pub(super) fn add_writer(&self, item: LocalItemId, backing_file: Arc<File>) -> (Fh, WriterRank) {
        todo!()
    }

    /// What `fh` holds open; `None` for an `fh` that is no file's. Called by read.
    pub(super) fn find_file(&self, fh: Fh) -> Option<OpenItemFile> {
        todo!()
    }

    /// The item and the backing file a writer's `fh` writes. Called by write and fsync, on
    /// the mount's own threads: no row is touched per write (design §8).
    pub(super) fn find_file_to_write(&self, fh: Fh) -> Option<(LocalItemId, Arc<File>)> {
        todo!()
    }

    /// The backing file of a writer of `item`, any of them: what getattr, lookup and a
    /// directory reading stat for the size and the times of a file open for writing, and
    /// what the last close reads them from (design §4). `None` when no program writes it.
    pub(super) fn find_writer_file(&self, item: LocalItemId) -> Option<Arc<File>> {
        todo!()
    }

    /// Whether a program holds `item` open for writing. Called by free-up and by a step
    /// before it touches or sends the file.
    pub fn is_open_for_writing(&self, item: LocalItemId) -> bool {
        todo!()
    }

    /// Bytes were written through a handle of `item`, or it was cut: the last close then
    /// says the content changed. Called by write and by a truncation of an open file.
    pub(super) fn mark_written(&self, item: LocalItemId) {
        todo!()
    }

    /// A reader of a file that was not on disk reads `backing_file` from now on: its
    /// download landed. Called by read.
    pub(super) fn switch_to_backing_file(&self, fh: Fh, backing_file: Arc<File>) {
        todo!()
    }

    /// Closes the reader `fh` holds. Called by release, on the mount's thread.
    pub(super) fn close_reader(&self, fh: Fh) {
        todo!()
    }

    /// The writer `fh` holds, and what its close will leave: `StillOpenElsewhere` while
    /// another handle writes the item, else whether anything was written. Nothing is
    /// removed. Called by release of a writer, on the write-operation thread, first.
    pub(super) fn find_closing_writer(&self, fh: Fh) -> Option<ClosingWriter> {
        todo!()
    }

    /// Takes the writer `fh` out. Called by release of a writer after the row of its last
    /// close was written (or at once, when another handle still writes the item).
    pub(super) fn remove_writer(&self, fh: Fh) {
        todo!()
    }

    /// A directory opened. Called by opendir.
    pub(super) fn add_directory(&self) -> Fh {
        todo!()
    }

    /// The items the directory `fh` holds listed at its first reading.
    /// Kept, as today: a directory read in parts reads one listing, whatever is made or
    /// removed in between (the map's E4).
    pub(super) fn find_listed_items(&self, fh: Fh) -> Option<Arc<Vec<Item>>> {
        todo!()
    }

    /// Keeps what the directory `fh` holds listed. Called by the first readdir of an `fh`.
    pub(super) fn keep_listed_items(&self, fh: Fh, listed: Arc<Vec<Item>>) {
        todo!()
    }

    /// Closes the directory `fh` holds. Called by releasedir.
    pub(super) fn close_directory(&self, fh: Fh) {
        todo!()
    }
}
