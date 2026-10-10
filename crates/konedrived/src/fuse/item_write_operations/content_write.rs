//! A file's content changed by a program: open for writing, truncate, the time set, and
//! the last close. The row first (design §8).
//!
//! **Decides:** [`decide_open_change`] and [`decide_close_change`]. **Writes:**
//! `OpenedForWriting` at the first writer's open, before a byte goes to disk;
//! `ClosedChanged` with the size and the times read from the backing file through the
//! handle, or `ClosedUnchanged`, at the last close; `Truncated` for a truncation with no
//! writer; `ItemStore::set_local_modified_time` for a time alone. One rule for both
//! access modes: a close is written whatever the mode is by then (the map's E22).
//!
//! **Three of these make a backing file**, and with it an entry and maybe the directories
//! above it: the first writer's open and the open to overwrite of a file that is not on
//! disk, and its truncation to nothing. Row first, so between the row and the file a
//! reader would find a row that says on disk over no file. Each therefore begins a place
//! change BEFORE its row is written and makes the file through `PlaceChange::make_entry`
//! AFTER it: the count is odd from the row to the file, a reader waits, and
//! `mend_missing_copy` is never asked about a file that is being made. A row refused:
//! nothing was made. A file that cannot be made after its row: the mount's take-down.
//! Where the file is on disk already nothing of a name changes and no change is begun;
//! the backing file of a row is opened on this thread, under the tree lock, where no
//! place change can run.
//!
//! **The row's time and the file's are kept the same**: whatever writes a time into the
//! row (a truncation, a time set) gives the backing file that very time after it changed
//! the file, so that a step towards OneDrive finds the content it was planned from by its
//! size and time.
//!
//! **Tested without the rest:**
// test: the_first_writers_open_raises_the_change_count_before_the_file_is_opened
// test: a_second_writers_open_writes_no_row
// test: a_file_not_on_disk_and_not_empty_is_downloaded_before_it_is_opened_for_writing
// test: an_empty_file_not_on_disk_is_opened_and_said_on_disk_in_one_transaction
// test: a_reader_between_the_row_that_says_on_disk_and_the_file_being_made_waits_and_gets_the_file
// test: a_file_opened_to_be_overwritten_is_not_downloaded
// test: the_last_close_of_a_changed_file_writes_the_size_and_times_of_the_backing_file
// test: the_last_close_of_an_untouched_file_puts_the_content_before_the_write_back
// test: a_close_while_another_writer_holds_the_file_writes_nothing
// test: the_writer_leaves_the_handles_only_after_the_row_of_its_last_close_is_written
// test: a_truncation_with_no_writer_writes_the_new_size_before_the_file_is_cut
// test: a_truncated_file_has_the_time_its_row_was_given
// test: a_truncation_of_a_file_open_for_writing_marks_it_written_and_writes_no_row
// test: a_truncation_through_a_handle_whose_row_is_gone_cuts_the_handles_file
// test: a_time_set_alone_goes_to_the_backing_file_and_to_the_row
// test: a_close_in_a_folder_switched_to_read_only_meanwhile_is_still_written
#![allow(dead_code, unused_variables)]

use std::fs::File;
use std::sync::Arc;

use fuser::{Errno, FileAttr};
use konedrive_tree::{ContentPresence, LocalItemId, LocalVersionChange};

use super::super::item_downloader::ItemToDownload;
use super::super::item_handles::ClosingWriter;
use super::super::open_handles::{Fh, WriterRank};
use super::ItemWriteOperations;

/// A file whose whole content must be on disk before a program writes into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fuse) enum FileToFill {
    /// Its content is on disk, or it is empty, or it is to be cut to nothing.
    Nothing,
    /// It is downloaded first.
    Download(ItemToDownload),
}

/// A writer that was opened.
pub(in crate::fuse) struct OpenedWriter {
    pub(in crate::fuse) fh: Fh,
    pub(in crate::fuse) backing_file: Arc<File>,
}

/// What the row is told when a writer opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OpenChange {
    /// Another handle writes the file already: `OpenedForWriting` was written.
    Nothing,
    /// The first writer of a file whose content is on disk: `OpenedForWriting`.
    OpenedForWriting,
    /// The first writer of a file whose content is not on disk and that is empty or is to
    /// be overwritten: `OpenedForWriting` and `OnDisk` in one transaction
    /// (`ItemStore::apply_with_content_on_disk`), then the backing file is made.
    OpenedForWritingAndOnDisk,
}

/// What a writer's open writes. Pure.
pub(super) fn decide_open_change(rank: WriterRank, content_presence: ContentPresence) -> OpenChange {
    todo!()
}

/// What the last close writes, from what the close will leave (`ClosingWriter::outcome`)
/// and the backing file's size and times; `None` while another handle still writes, and
/// for an item the folder does not show any more. Pure.
pub(super) fn decide_close_change(closing: &ClosingWriter, is_shown: bool, size: u64, mtime: i64, mtime_nsec: u32) -> Option<LocalVersionChange> {
    todo!()
}

impl ItemWriteOperations {
    /// Whether the file `node` must be downloaded whole before it is opened for writing
    /// or truncated to `new_size` (`None`: opened): its content is not on disk and it is
    /// not empty, and it is not cut to nothing. Called on the mount's thread, over its
    /// reading connection, before the job is queued; the download then runs on the runtime
    /// and the job after it (as today, `write_operations/write.rs:54`).
    pub(in crate::fuse) fn find_file_to_fill(&self, node: LocalItemId, new_size: Option<u64>) -> Result<FileToFill, Errno> {
        todo!()
    }

    /// Open for writing (no `O_TRUNC`): one read job (the row and its path);
    /// [`decide_open_change`]; the row; then the backing file opened and the writer added
    /// to `ItemHandles`. A file that is still not on disk and not empty is `ESTALE`: its
    /// download did not land. Called on the write-operation thread.
    pub(in crate::fuse) fn open_to_write(&self, node: LocalItemId) -> Result<OpenedWriter, Errno> {
        todo!()
    }

    /// Open with `O_TRUNC`: as [`open_to_write`](Self::open_to_write) with nothing
    /// downloaded, then the backing file cut to nothing and the handle marked written.
    pub(in crate::fuse) fn open_to_overwrite(&self, node: LocalItemId) -> Result<OpenedWriter, Errno> {
        todo!()
    }

    /// Truncate (setattr with a size). With a writer open (found by the item, or `fh`
    /// given): the backing file is cut and the handle marked written; no row. With none:
    /// the row first — `Truncated { item, size, mtime, mtime_nsec }`, with `OnDisk` in the
    /// same transaction for a file that was not on disk and is cut to nothing — then the
    /// file is cut and given the time the row was given. A node whose row is gone, asked
    /// with the `fh` of a writer: the handle's file is cut, and no row written (design
    /// §3). Answers the attributes after it.
    pub(in crate::fuse) fn truncate(&self, node: LocalItemId, fh: Option<Fh>, size: u64) -> Result<FileAttr, Errno> {
        todo!()
    }

    /// Setattr with a time and no size: the time goes to the backing file where the file
    /// has one, then to the row (`ItemStore::set_local_modified_time`); with a writer open
    /// the backing file alone, and the last close writes it. A node whose row is gone,
    /// asked with an `fh`: the handle's file alone. Answers the attributes.
    pub(in crate::fuse) fn set_modified_time(&self, node: LocalItemId, fh: Option<Fh>, mtime: i64, mtime_nsec: u32) -> Result<FileAttr, Errno> {
        todo!()
    }

    /// Release of a writer, in this order: `ItemHandles::find_closing_writer` (the writer
    /// stays among the handles); at the last close the size and the times read from the
    /// backing file through the handle and [`decide_close_change`]'s answer written; then
    /// `ItemHandles::remove_writer`; then the plan woken. So getattr finds the writer's
    /// file until the row has the new size. The program was answered already: a failure
    /// is logged, and the check at start mends the row. Called on the write-operation
    /// thread; a job dropped at the mount's stop leaves `content_before_write` set, which
    /// the check at start reads (the map's RISK-7.d).
    pub(in crate::fuse) fn close_writer(&self, fh: Fh) {
        todo!()
    }
}
