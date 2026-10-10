//! What each request of the kernel does in the model of rows: the function it calls and
//! the change of a local version it writes. The table of design §9, request by request.
//!
//! **State owned:** none; [`ItemKernelRequests`] holds the parts the requests go through.
//! At the switch `kernel_requests` (the `fuser::Filesystem`) calls one function here per
//! request and turns its answer into the reply; node ids are local item ids from the
//! first line on. It is a layer of its own, under the `Filesystem`, so that every request
//! is tested with no kernel and no mount.
//!
//! | Request | Thread | Calls | Writes |
//! |---|---|---|---|
//! | lookup | mount | `ItemLookup::find_entry` | — |
//! | getattr | mount | `ItemLookup::find_attributes` | — |
//! | opendir, readdir, readdirplus, releasedir | mount | `ItemLookup::list_entries`, `describe` | — |
//! | getxattr, listxattr | mount | `ItemLookup::find_xattr`, `list_xattr_names` | — |
//! | open (read) | mount | `decide_open_to_read`, `PlaceChanges::open_at_row_path` | — |
//! | read | mount, then runtime for a range | `decide_read`, `fetch_read_range` | — |
//! | open (write) | mount, runtime, write | `find_file_to_fill`, `open_to_write` | `OpenedForWriting` (+ on disk) |
//! | open (`O_TRUNC`) | mount, write | `open_to_overwrite` | `OpenedForWriting` (+ on disk) |
//! | create | mount, write | `make(FileOpenedForWriting)` | `apply_made_open_for_writing` |
//! | mknod, mkdir | mount, write | `make` | `Made` |
//! | unlink, rmdir | mount, write | `remove` | `Removed` |
//! | rename | mount, write | `rename` | `Moved` or `SavedOver` |
//! | setattr (size) | mount, runtime, write | `find_file_to_fill`, `truncate` | `Truncated` (+ on disk), or none with a writer |
//! | setattr (time) | mount, write | `set_modified_time` | the time (`set_local_modified_time`) |
//! | write, fsync, flush | mount | `ItemHandles::find_file_to_write` | none per write |
//! | release | mount, write | `close_writer` | `ClosedChanged` or `ClosedUnchanged` |
//!
//! **Threads.** Every answer here is a plain function on the mount's own thread, but
//! three kinds:
//!
//! - *what goes to the runtime* (a range of a file that is not on disk, the download
//!   before an open for writing or a truncation): the mount's thread decides and reads
//!   every row it needs (`decide_read`, `decide_open_to_write`, `decide_truncate`), and
//!   answers a typed case that carries VALUES; an `async fn` over those values does the
//!   rest on the runtime, with the reply moved into it, so that none of fuser's threads
//!   is held for a fetch and no worker of the runtime reads through `ReadConnections` or
//!   sleeps on a place change. A read of a backing file, the hot path, never leaves the
//!   mount's thread;
//! - *what goes to the write-operation thread* (make, remove, rename, the writers'
//!   opens, truncate, the time, a writer's release): queued as one job with the reply
//!   moved into the job (`ReplyTo`), as today (`kernel_requests/write_requests.rs`);
//! - everything else is answered where it was asked.
//!
//! Unchanged, and not here: init, statfs, access, interrupt, symlink, link, setxattr,
//! removexattr (refused as today), forget (nothing is kept per node: design §14).
//!
//! **Tested without the rest**, over the fixture (`item_fixture`), and no kernel:
// test: each_request_writes_the_change_its_line_of_the_table_names
// test: a_request_by_a_node_whose_row_is_gone_is_stale
// test: a_request_with_a_handle_whose_row_is_gone_is_answered_from_the_handles_file
// test: a_changing_request_in_a_read_only_folder_is_refused_before_it_is_queued
// test: a_read_of_a_backing_file_is_answered_on_the_mounts_thread
// test: a_read_of_a_file_not_on_disk_reads_its_rows_on_the_mounts_thread_and_fetches_on_the_runtime
// test: an_append_is_put_at_the_size_the_writers_file_has
// test: the_bridges_own_lookup_is_answered_from_the_announcement_in_progress
// test: an_open_of_a_row_that_says_on_disk_over_no_file_mends_a_clean_row_and_reads_from_onedrive
#![allow(dead_code, unused_variables)]

use std::ffi::OsStr;
use std::sync::Arc;

use fuser::{Errno, FileAttr};
use konedrive_tree::{Item, ItemKind, LocalItemId};

use super::item_downloader::ItemToDownload;
use super::item_read_origin::RangeSource;
use super::item_write_operations::FileToFill;

use super::item_download_states::ItemDownloadStates;
use super::item_downloader::ItemDownloader;
use super::item_free_up::ItemFreeUp;
use super::item_handles::ItemHandles;
use super::item_inotify_bridge::ItemInotifyBridge;
use super::item_lookup::{ItemLookup, ShownEntry};
use super::item_range_fetcher::ItemRangeFetcher;
use super::item_write_operations::{ItemWriteOperations, MadeItem, MadeKind, RenameMode};
use super::open_handles::Fh;
use super::place_changes::PlaceChanges;
use super::read_connections::ReadConnections;
use super::write_operation_thread::WriteOperationThread;

/// A file opened: its handle, and whether the kernel may keep what it cached of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OpenedFile {
    pub(super) fh: Fh,
    pub(super) keeps_cache: bool,
}

/// Where the answer of a job or of a future goes: the kernel's reply, moved into it.
pub(super) type ReplyTo<T> = Box<dyn FnOnce(Result<T, Errno>) + Send + 'static>;

/// What a read is answered with, decided on the mount's thread.
pub(super) enum ReadAnswer {
    /// The bytes, read from a backing file (also once the content landed since the open,
    /// and from the file of the download of this content that runs).
    Bytes(Vec<u8>),
    /// A range to fetch from OneDrive on the runtime.
    Range(RangeToFetch),
}

/// A range of a file that is not on disk, with everything its fetch needs, as values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RangeToFetch {
    pub(super) item: LocalItemId,
    pub(super) source: RangeSource,
    pub(super) from: u64,
    pub(super) end: u64,
    /// The download to start when this read reaches the threshold of bytes read as
    /// ranges.
    pub(super) download_at_threshold: ItemToDownload,
}

/// The answers to the kernel's requests, over the rows. Opaque outside `fuse`: the
/// switch's `fuser::Filesystem` is made over it.
pub struct ItemKernelRequests {
    lookup: Arc<ItemLookup>,
    read_connections: Arc<ReadConnections>,
    place_changes: Arc<PlaceChanges>,
    item_handles: Arc<ItemHandles>,
    write_operations: Arc<ItemWriteOperations>,
    write_operation_thread: Arc<WriteOperationThread>,
    inotify_bridge: Arc<ItemInotifyBridge>,
    downloader: Arc<ItemDownloader>,
    download_states: Arc<ItemDownloadStates>,
    range_fetcher: Arc<ItemRangeFetcher>,
    free_up: Arc<ItemFreeUp>,
    runtime: tokio::runtime::Handle,
}

impl ItemKernelRequests {
    /// Made by the mount's build, last before the session.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        lookup: Arc<ItemLookup>,
        read_connections: Arc<ReadConnections>,
        place_changes: Arc<PlaceChanges>,
        item_handles: Arc<ItemHandles>,
        write_operations: Arc<ItemWriteOperations>,
        write_operation_thread: Arc<WriteOperationThread>,
        inotify_bridge: Arc<ItemInotifyBridge>,
        downloader: Arc<ItemDownloader>,
        download_states: Arc<ItemDownloadStates>,
        range_fetcher: Arc<ItemRangeFetcher>,
        free_up: Arc<ItemFreeUp>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        todo!()
    }

    /// lookup. For the inotify bridge's own thread (`ItemInotifyBridge::is_own_thread` of
    /// `thread_id`) first `item_announcements::find_announced_name` with
    /// `ItemInotifyBridge::find_in_progress`; else, and for any other name,
    /// `ItemLookup::find_entry`: one row by parent and name.
    pub(super) fn answer_lookup(&self, directory: LocalItemId, name: &OsStr, thread_id: u32) -> Result<ShownEntry, Errno> {
        todo!()
    }

    /// getattr. `ItemLookup::find_attributes`: one row by key; a file open for writing is
    /// answered with its writer's file's size and times (design §4); a node whose row is
    /// gone, from the file of the handle the request carries (design §3).
    pub(super) fn answer_getattr(&self, node: LocalItemId, fh: Option<Fh>) -> Result<FileAttr, Errno> {
        todo!()
    }

    /// opendir. `ItemLookup::find_shown_item`, then `ItemHandles::add_directory`: a node
    /// whose row is gone is `ESTALE` (today opendir always succeeds).
    pub(super) fn answer_opendir(&self, node: LocalItemId) -> Result<Fh, Errno> {
        todo!()
    }

    /// readdir and readdirplus: the items of the directory from `offset` on. The first
    /// reading of an `fh` calls `ItemLookup::list_entries` and keeps it
    /// (`ItemHandles::keep_listed_items`); a later part reads what was kept. readdirplus
    /// then calls `ItemLookup::describe` per item.
    pub(super) fn answer_readdir(&self, node: LocalItemId, fh: Fh, offset: usize) -> Result<Vec<Item>, Errno> {
        todo!()
    }

    /// releasedir. `ItemHandles::close_directory`.
    pub(super) fn answer_releasedir(&self, fh: Fh) {
        todo!()
    }

    /// getxattr. `ItemLookup::find_xattr`.
    pub(super) fn answer_getxattr(&self, node: LocalItemId, name: &OsStr) -> Result<Vec<u8>, Errno> {
        todo!()
    }

    /// listxattr. `ItemLookup::list_xattr_names`: one read of the row, where today each of
    /// six names resolves the node again.
    pub(super) fn answer_listxattr(&self, node: LocalItemId) -> Result<Vec<&'static str>, Errno> {
        todo!()
    }

    /// open for reading. `decide_open_to_read` of the shown item; a file on disk is
    /// opened through `PlaceChanges::open_at_row_path` (the row and its path read in one
    /// closure of the thread's connection); a file that is not gets a handle that holds
    /// the item and its content (`ItemHandles::add_file`). `ENOENT` from the disk for a
    /// row that says on disk: `ItemFreeUp::mend_missing_copy` as a job of the
    /// write-operation thread, then decided once more. No local version is written.
    pub(super) fn answer_open_to_read(&self, node: LocalItemId) -> Result<OpenedFile, Errno> {
        todo!()
    }

    /// read, on the mount's thread. A handle on a backing file reads it. One opened not
    /// on disk: the row is read again and `decide_not_on_disk_read_origin` asked with the
    /// content the handle was opened as; then the backing file once the content landed
    /// (`PlaceChanges::open_at_row_path`, `ItemHandles::switch_to_backing_file`), or the
    /// file of the running download of that content
    /// (`ItemDownloadStates::find_running_download`), or a range to fetch.
    pub(super) fn decide_read(&self, fh: Fh, offset: u64, size: u32) -> Result<ReadAnswer, Errno> {
        todo!()
    }

    /// The rest of a read as ranges, on the runtime: `ItemRangeFetcher::fetch_range`, the
    /// bytes counted towards the whole download (`count_bytes_read`, then
    /// `ItemDownloader::download` at the threshold). No row is read here. The caller keeps
    /// the task's abort handle for the kernel's interrupt, as today
    /// (`interruptible_reads`).
    pub(super) async fn fetch_read_range(&self, range: RangeToFetch) -> Result<Vec<u8>, Errno> {
        todo!()
    }

    /// open for writing, without `O_TRUNC`, on the mount's thread: what must be on disk
    /// first (`ItemWriteOperations::find_file_to_fill`).
    pub(super) fn decide_open_to_write(&self, node: LocalItemId) -> Result<FileToFill, Errno> {
        todo!()
    }

    /// The rest of an open for writing, on the runtime: the download awaited
    /// (`ItemDownloader::download` with a waiter: `ESTALE`, `EEXIST` and `EIO` reach the
    /// program), then `open_to_write` queued as a job of the write-operation thread, which
    /// writes `OpenedForWriting` for the first writer before the file is opened and
    /// answers `reply`. With nothing to fill, the caller queues the job at once
    /// ([`queue_open_to_write`](Self::queue_open_to_write)).
    pub(super) async fn fill_and_open_to_write(&self, node: LocalItemId, fill: ItemToDownload, reply: ReplyTo<OpenedFile>) {
        todo!()
    }

    /// `open_to_write` as a job of the write-operation thread, which answers `reply`.
    pub(super) fn queue_open_to_write(&self, node: LocalItemId, reply: ReplyTo<OpenedFile>) {
        todo!()
    }

    /// open with `O_TRUNC`. `open_to_overwrite` on the write-operation thread: nothing is
    /// downloaded.
    pub(super) fn queue_open_to_overwrite(&self, node: LocalItemId, reply: ReplyTo<OpenedFile>) {
        todo!()
    }

    /// mkdir, mknod, create. `ItemWriteOperations::make` on the write-operation thread:
    /// the disk, then `LocalVersionChange::Made`; a created file made and opened in one
    /// transaction. For the bridge's own thread (mkdir, mknod): answered from the rows,
    /// nothing made.
    pub(super) fn queue_make(&self, directory: LocalItemId, name: &OsStr, kind: MadeKind, thread_id: u32, reply: ReplyTo<MadeItem>) {
        todo!()
    }

    /// unlink and rmdir. `ItemWriteOperations::remove` on the write-operation thread: the
    /// disk, then `LocalVersionChange::Removed`. For the bridge's own thread: answered
    /// from the rows, nothing removed.
    pub(super) fn queue_remove(&self, directory: LocalItemId, name: &OsStr, asked_kind: ItemKind, thread_id: u32, reply: ReplyTo<()>) {
        todo!()
    }

    /// rename. `ItemWriteOperations::rename` on the write-operation thread: the disk, then
    /// `Moved` or `SavedOver`.
    pub(super) fn queue_rename(&self, directory: LocalItemId, name: &OsStr, new_directory: LocalItemId, new_name: &OsStr, mode: RenameMode, reply: ReplyTo<()>) {
        todo!()
    }

    /// setattr with a size, on the mount's thread: what must be on disk first
    /// (`find_file_to_fill` with the size).
    pub(super) fn decide_truncate(&self, node: LocalItemId, size: u64) -> Result<FileToFill, Errno> {
        todo!()
    }

    /// The rest of a truncation: with a download, awaited on the runtime first; then
    /// `ItemWriteOperations::truncate`, with the `fh` the request carries, as a job of the
    /// write-operation thread, which answers `reply`.
    pub(super) async fn fill_and_truncate(&self, node: LocalItemId, fh: Option<Fh>, size: u64, fill: FileToFill, reply: ReplyTo<FileAttr>) {
        todo!()
    }

    /// setattr with a time and no size. `ItemWriteOperations::set_modified_time`, with the
    /// `fh` the request carries. Mode, owner and the other times are ignored, as today.
    /// For the bridge's own thread: answered as getattr.
    pub(super) fn queue_set_modified_time(&self, node: LocalItemId, fh: Option<Fh>, mtime: i64, mtime_nsec: u32, thread_id: u32, reply: ReplyTo<FileAttr>) {
        todo!()
    }

    /// write. `ItemHandles::find_file_to_write`, `mark_written`, the bytes into the
    /// backing file, on the mount's own thread. No row is touched (design §8).
    pub(super) fn answer_write(&self, fh: Fh, offset: u64, bytes: &[u8]) -> Result<u32, Errno> {
        todo!()
    }

    /// fsync. The backing file of the handle synced, on the mount's own thread.
    pub(super) fn answer_fsync(&self, fh: Fh) -> Result<(), Errno> {
        todo!()
    }

    /// release. The kernel is answered at once; a reader's handle is closed on the
    /// mount's thread (`ItemHandles::close_reader`), a writer's as a job of the
    /// write-operation thread (`ItemWriteOperations::close_writer`), which writes
    /// `ClosedChanged` or `ClosedUnchanged` at the last close and only then takes the
    /// writer out.
    pub(super) fn answer_release(&self, fh: Fh) {
        todo!()
    }
}
