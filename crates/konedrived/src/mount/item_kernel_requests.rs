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
//! | opendir, readdir, readdirplus, releasedir | mount | `ItemLookup::list_entries`, `describe` (`describe_listed_item`, an item at a time) | — |
//! | getxattr, listxattr | mount | `ItemLookup::find_xattr`, `list_xattr_names` | — |
//! | open (read) | mount | `decide_open_to_read`, `PlaceChanges::open_at_row_path` | — (a clean row over a missing copy is mended: `NotOnDisk`) |
//! | read | mount, then runtime for a range | `decide_read`, `fetch_read_range` or `fetch_range_after_download` | — |
//! | open (write) | mount, runtime, write | `find_file_to_fill`, `open_to_write` | `OpenedForWriting` (+ on disk) |
//! | open (`O_TRUNC`) | mount, write | `open_to_overwrite` | `OpenedForWriting` (+ on disk) |
//! | create | mount, write | `make(FileOpenedForWriting)` | `apply_made_open_for_writing` |
//! | mknod, mkdir | mount, write | `make` | `Made` |
//! | unlink, rmdir | mount, write | `remove` | `Removed` |
//! | rename | mount, write | `rename` | `Moved` or `SavedOver` |
//! | setattr (size) | mount, runtime, write | `decide_truncate`, `queue_truncate` or `fill_and_truncate` | `Truncated` (+ on disk), or none with a writer |
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
//! **A read-only folder** refuses every request that changes the folder here, on the
//! mount's thread, before anything is queued or downloaded (`EROFS`), as today
//! (`write_requests.rs`, `run_in_read_write_folder`): the thread's jobs wait for the tree
//! lock, and a program is not made to wait for it to be told. The operation asks the
//! mode again on the thread, and that answer counts. A mount whose take-down was asked
//! is read-only (`MountTakeDown::ask`), so it refuses the same way. The last close is
//! written whatever the mode is.
//!
//! **The attributes a write operation's reply carries** (make, truncate, the time) are
//! made in the operation's job, on the write-operation thread, by [`WriteReplyAttributes`]:
//! by `ItemLookup`'s own code (`describe_through`, `describe_node`), given the row and the
//! path read through the writer — never on the connection of a thread of the mount, which
//! that thread has none of.
//!
//! **Through a handle, bytes of the content it was opened as, or none.** [`decide_read`]
//! answers bytes from three places: the handle's own backing file; the backing file the
//! row names once the content landed (the row and its path are read in one read
//! transaction, and the row must still say that content is on disk, or kept from before
//! a write: `decide_not_on_disk_read_origin`); and the file of the running download of
//! that very item and content, below a mark the download's source raises only for
//! answers it has told to be of that content (`item_downloader::download_source`), and
//! which is taken back to 0 the moment the download finds what it wrote not to be the
//! content. [`fetch_read_range`] answers what the range fetcher lets through
//! (`item_range_fetcher::decide_is_another_content`), which through Graph cannot tell
//! another content of the item until a download or a listing did.
//!
//! **A content OneDrive stated another one for** in its answer to the last download of it
//! (`ItemDownloadStates::is_another_content`) is not an error by itself: that is a fact
//! from one answer, and it only changes what the read asks of OneDrive. No range is
//! trusted for it, so the read asks for the whole download of the content — or joins
//! the one that runs and has no answer yet — and waits for it
//! ([`fetch_range_after_download`]): the download asks OneDrive anew, and what was
//! remembered is void at the first answer it accepts, not before: until then every read
//! of the content joins that one download. Stated another content again:
//! `ESTALE`, remembered again. Stated the asked content: the download lands, and the
//! read is answered its range. An old handle and a new open of the same content go the
//! same way.
//!
//! **Nothing is announced to the inotify bridge here.** What a program asks for through
//! the mount the kernel tells its watchers itself; the bridge is for what a step towards
//! the folder changed (design §9). The bridge is held to tell its own requests
//! (`is_own_thread`) and the name it is announcing (`find_in_progress`).
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
// test: the_reading_requests_answer_what_the_rows_show
// test: a_range_of_a_file_not_on_disk_per_case
// test: a_removal_the_bridge_announces_per_case
// test: a_read_of_a_small_file_not_on_disk_downloads_it_once_all_of_it_was_read
// test: a_read_through_a_handle_whose_content_onedrive_changed_is_stale
// test: a_file_not_on_disk_is_downloaded_before_it_is_opened_for_writing
// test: a_download_that_fails_refuses_the_open_for_writing_and_the_truncation_with_its_errno
// test: a_truncation_to_nothing_of_a_file_not_on_disk_downloads_nothing
// test: a_truncation_of_a_file_not_on_disk_downloads_it_first
// test: a_created_file_written_and_closed_is_one_unsent_item_with_its_bytes
// test: a_file_renamed_while_it_is_open_is_closed_into_the_row_at_its_new_place
// test: a_directory_read_during_renames_describes_each_listed_item_as_itself
// test: the_bridges_own_requests_are_answered_from_the_rows_and_make_nothing
// test: after_the_take_down_was_asked_every_change_is_refused_and_reading_goes_on
// test: each_refusal_of_a_write_operation_reaches_the_reply_with_its_errno
// test: a_row_the_store_refuses_reaches_the_reply_with_the_errno_of_its_rule
// test: a_request_by_a_handle_that_is_not_open_is_ebadf
// test: a_file_opened_to_be_overwritten_is_cut_and_nothing_is_downloaded
// test: the_time_set_on_a_file_is_what_its_reply_and_the_next_getattr_say
// test: a_read_through_a_handle_is_never_answered_from_a_download_of_another_content
// test: a_read_past_the_threshold_starts_no_download_again_while_downloads_end_without_landing
// test: a_reader_whose_content_landed_reads_the_backing_file_beside_a_writer_that_opened_since
// test: a_part_of_a_directory_reading_hands_out_the_kept_listing_and_copies_nothing
// test: a_read_of_a_content_onedrive_answered_with_another_once_asks_anew_and_succeeds
// test: every_read_is_stale_and_costs_one_asking_while_onedrive_states_another_content
// test: a_read_of_an_item_onedrive_did_not_have_once_goes_on_as_ranges
// test: a_read_is_not_answered_from_a_downloads_file_once_its_bytes_failed_the_hash
// test: what_a_read_does_once_its_whole_download_ended_per_case
// test: reads_of_a_remembered_content_join_its_download_until_onedrive_has_answered
#![allow(dead_code)]

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fuser::{Errno, FileAttr};
use konedrive_tree::{Item, ItemKind, LocalItemId, Store, TreeError};
use tokio::sync::oneshot;

use super::access_mode_switch::AccessModeSwitch;
use super::errno::to_errno;
use super::item_announcements::{find_announced_name, AnnouncedName, ItemAnnouncement};
use super::item_download_states::{DownloadAsker, DownloadKey, ItemDownloadStates};
use super::item_downloader::{ItemDownloader, ItemToDownload};
use super::item_free_up::ItemFreeUp;
use super::item_handles::{ItemHandles, OpenItemFile};
use super::item_inotify_bridge::ItemInotifyBridge;
use super::item_lookup::{to_node_row, ItemLookup, ShownEntry};
use super::item_range_fetcher::{ItemRangeFetcher, RangeRefusal};
use super::item_read_origin::{decide_not_on_disk_read_origin, decide_open_to_read, NotOnDiskReadOrigin, OpenToRead, RangeSource};
use super::item_write_operations::{FileToFill, ItemWriteOperations, MadeKind, RenameMode};
use super::item_handles::Fh;
use super::place_changes::{decide_open_errno, OpenAtRowError, OpenMode, PlaceChanges};
use super::read_connections::ReadConnections;
use super::write_operation_thread::WriteOperationThread;

#[cfg(test)]
mod tests;

/// For how long the kernel may keep the attributes and the name of an entry the folder
/// shows, as today (`attributes::TTL`). The one such time of the mount: the kernel's
/// interface (`kernel_requests`) answers with it too.
pub(super) const ATTRIBUTE_LIFETIME: Duration = Duration::from_secs(1);

/// How far behind its written-up-to mark the file of a running download is surely
/// written: a piece the download writes is no larger than this. Today's
/// `kernel_requests::read_origin::WRITTEN_BEHIND`, which goes with that module.
const WRITTEN_BEHIND: u64 = 1 << 20;

/// How often an open or a read starts over because the row changed between its two
/// reads of it (a landing, a free-up, a writer's open): each start reads the row anew.
const ROW_TRIES: usize = 3;

/// A file opened: its handle, and whether the kernel may keep what it cached of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OpenedFile {
    pub(super) fh: Fh,
    pub(super) keeps_cache: bool,
}

/// Where the answer of a job or of a future goes: the kernel's reply, moved into it.
pub(super) type ReplyTo<T> = Box<dyn FnOnce(Result<T, Errno>) + Send + 'static>;

/// What a lookup is answered with.
#[derive(Debug, Clone)]
pub(super) enum LookupAnswer {
    /// The entry the folder shows under the name.
    Shown(ShownEntry),
    /// The attributes of the item that had the name, for the inotify bridge's own lookup
    /// of the name it is announcing as gone: no row shows it there any more.
    Vanishing(FileAttr),
}

impl LookupAnswer {
    pub(super) fn attributes(&self) -> &FileAttr {
        match self {
            LookupAnswer::Shown(entry) => &entry.attributes,
            LookupAnswer::Vanishing(attributes) => attributes,
        }
    }

    /// For how long the kernel may keep the answer: not at all for a name that is
    /// vanishing.
    pub(super) fn lifetime(&self) -> Duration {
        match self {
            LookupAnswer::Shown(_) => ATTRIBUTE_LIFETIME,
            LookupAnswer::Vanishing(_) => Duration::ZERO,
        }
    }
}

/// What mkdir, mknod and create are answered with: the entry that was made, and the
/// writer's handle of a created file (`Fh::NOT_OPENED` otherwise).
#[derive(Debug, Clone)]
pub(super) struct MadeEntry {
    pub(super) entry: ShownEntry,
    pub(super) fh: Fh,
}

/// What a read is answered with, decided on the mount's thread.
#[derive(Debug)]
pub(super) enum ReadAnswer {
    /// The bytes, read from a backing file (also once the content landed since the open,
    /// and from the file of the download of this content that runs).
    Bytes(Vec<u8>),
    /// A range to fetch from OneDrive on the runtime.
    Range(RangeToFetch),
    /// A range of a content OneDrive stated another one for in its last answer to a
    /// download of it: on the runtime, the whole download of the content is asked for
    /// first, or the one that runs and has no answer yet is joined, which asks OneDrive
    /// anew, and the range fetched once it landed (`fetch_range_after_download`).
    WholeDownload(RangeToFetch),
}

/// A range of a file that is not on disk, with everything its fetch needs, as values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RangeToFetch {
    pub(super) item: LocalItemId,
    pub(super) source: RangeSource,
    pub(super) from: u64,
    pub(super) end: u64,
    /// The whole download of the content the range is of: started when this read reaches
    /// the threshold of bytes read as ranges, and asked for before the range where no
    /// range is trusted (`ReadAnswer::WholeDownload`).
    pub(super) download_at_threshold: ItemToDownload,
}

/// What the download states say of the content a read is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContentDownload {
    /// No download of it runs, or the one that runs has no file yet.
    None,
    /// One runs, OneDrive answered it with this content, and it has written its file up
    /// to here in order.
    WrittenUpTo(u64),
    /// OneDrive stated another content in its answer to the last one, and has answered
    /// none since: none was started, or the one that runs has no answer yet.
    AnotherContent,
}

/// Where the bytes of one read of a file that is not on disk come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RangeOrigin {
    /// Nowhere: the read starts at or past the end of the content.
    Nothing,
    /// The file the running download of this content writes into, which is surely
    /// written that far: the bytes from `from` up to `end`.
    RunningDownload { from: u64, end: u64 },
    /// OneDrive: the bytes from `from` up to `end`.
    OneDrive { from: u64, end: u64 },
    /// OneDrive, once the whole download of the content was asked for and landed: the
    /// bytes from `from` up to `end`.
    WholeDownload { from: u64, end: u64 },
}

/// Where the `size` bytes at `offset` of a content `content_size` long come from, by what
/// the download states say of that content. The read ends at the content's end, and one
/// at or past it asks nothing of anybody. Pure.
pub(super) fn decide_range_origin(content_size: u64, download: ContentDownload, offset: u64, size: u32) -> RangeOrigin {
    let end = offset.saturating_add(u64::from(size)).min(content_size);
    if offset >= end {
        return RangeOrigin::Nothing;
    }
    match download {
        ContentDownload::WrittenUpTo(written_up_to) if end.saturating_add(WRITTEN_BEHIND) <= written_up_to => RangeOrigin::RunningDownload { from: offset, end },
        ContentDownload::WrittenUpTo(_) | ContentDownload::None => RangeOrigin::OneDrive { from: offset, end },
        ContentDownload::AnotherContent => RangeOrigin::WholeDownload { from: offset, end },
    }
}

/// What a read that asked for the whole download of its content does once the download
/// ended and its waiter heard `heard`: `Ok` to fetch its range, or the errno it is
/// answered. Pure.
///
/// - Landed: OneDrive stated the asked content, and the range is fetched.
/// - `EEXIST`, a place held on disk: OneDrive stated the asked content all the same —
///   the file was whole and checked — and the range is fetched, as the reads of such a
///   file are answered while nothing was remembered of it.
/// - `ESTALE`: OneDrive stated another content again, or does not have the item, or the
///   row changed. Any other errno: the asking failed, and so does the read.
pub(super) fn decide_read_after_download(heard: Result<(), i32>) -> Result<(), Errno> {
    match heard {
        Ok(()) | Err(libc::EEXIST) => Ok(()),
        Err(errno) => Err(Errno::from_i32(errno)),
    }
}

/// What the inotify bridge's own unlink or rmdir is answered, by what a lookup of the
/// name found: done when the folder shows no such name (`ENOENT`, or a directory that is
/// gone itself), and `EROFS` when it still shows one — nothing is removed for the bridge,
/// as today (`lookup_requests.rs`, `check_announced_remove`). Any other errno is the
/// lookup's own. Pure.
pub(super) fn decide_announced_removal(found: Result<(), Errno>) -> Result<(), Errno> {
    match found {
        Ok(()) => Err(Errno::EROFS),
        Err(errno) if errno == Errno::ENOENT || errno == Errno::ESTALE => Ok(()),
        Err(errno) => Err(errno),
    }
}

/// Up to `size` bytes of `file` at `offset`: fewer at its end.
fn read_file_range(file: &File, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
    let mut bytes = vec![0u8; size as usize];
    let mut read = 0;
    while read < bytes.len() {
        match file.read_at(&mut bytes[read..], offset + read as u64) {
            Ok(0) => break,
            Ok(count) => read += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(to_errno(&error)),
        }
    }
    bytes.truncate(read);
    Ok(bytes)
}

/// The attributes a write operation's reply carries, made on the write-operation thread
/// once the operation wrote its row: of the item mkdir, mknod and create made, and of the
/// node a truncation or a time was set on.
///
/// **State owned:** none. It reads the row and the path through the writer
/// (`Store::call_blocking`), as every job of that thread reads, and hands both to
/// `ItemLookup`, whose code makes the attributes for the reading requests too: one code
/// for what getattr answers and for what a reply carries.
struct WriteReplyAttributes {
    store: Store,
    lookup: Arc<ItemLookup>,
}

impl WriteReplyAttributes {
    /// The path of the local version of `node`, read through the writer.
    fn find_path(&self, node: LocalItemId) -> Result<Option<PathBuf>, TreeError> {
        self.store.call_blocking(move |store| store.item_store().reader().find_path(node))
    }

    /// The attributes of `item`, a row the folder shows; `ESTALE` for one it does not.
    fn describe(&self, item: Item) -> Result<ShownEntry, Errno> {
        self.lookup.describe_through(item, &|node| self.find_path(node))
    }

    /// The attributes of `node` after a truncation or a time set on it, as getattr
    /// answers them (`ItemLookup::describe_node`): of its row when the folder shows it;
    /// of the file `fh` holds when the row is gone or shows nothing and the handle is
    /// that node's (design §3); `ESTALE` otherwise.
    fn find_attributes(&self, node: LocalItemId, fh: Option<Fh>) -> Result<FileAttr, Errno> {
        let row = self.store.call_blocking(move |store| store.item_store().reader().find_item(node)).map_err(|error| {
            tracing::warn!("the row of item {node} could not be read for the attributes of a reply: {error}");
            Errno::EIO
        })?;
        self.lookup.describe_node(to_node_row(row), node, fh, &|node| self.find_path(node))
    }
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
    access_mode_switch: Arc<AccessModeSwitch>,
    reply_attributes: Arc<WriteReplyAttributes>,
    runtime: tokio::runtime::Handle,
}

impl ItemKernelRequests {
    /// Made by the mount's build, last before the session. `store` is for the attributes
    /// a write operation's reply carries ([`WriteReplyAttributes`]); `access_mode_switch`
    /// refuses a change of a read-only folder before it is queued.
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
        access_mode_switch: Arc<AccessModeSwitch>,
        store: Store,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let reply_attributes = Arc::new(WriteReplyAttributes { store, lookup: Arc::clone(&lookup) });
        Self { lookup, read_connections, place_changes, item_handles, write_operations, write_operation_thread, inotify_bridge, downloader, download_states, range_fetcher, free_up, access_mode_switch, reply_attributes, runtime }
    }

    /// lookup. For the inotify bridge's own thread (`ItemInotifyBridge::is_own_thread` of
    /// `thread_id`) first `item_announcements::find_announced_name` with
    /// `ItemInotifyBridge::find_in_progress`; else, and for any other name,
    /// `ItemLookup::find_entry`: one row by parent and name. A name that is vanishing is
    /// answered with `ItemLookup::describe_vanishing` (no row shows it there any more) and
    /// no lifetime; one that is appearing, `ENOENT`.
    pub(super) fn answer_lookup(&self, directory: LocalItemId, name: &OsStr, thread_id: u32) -> Result<LookupAnswer, Errno> {
        let in_progress = match self.inotify_bridge.is_own_thread(thread_id) {
            true => self.inotify_bridge.find_in_progress(),
            false => None,
        };
        self.find_lookup_answer(in_progress.as_ref(), directory, name)
    }

    /// What a lookup of `name` in `directory` is answered while the bridge announces
    /// `in_progress` and the lookup is the bridge's own (`None`: it is anyone's, or
    /// nothing is being announced).
    fn find_lookup_answer(&self, in_progress: Option<&ItemAnnouncement>, directory: LocalItemId, name: &OsStr) -> Result<LookupAnswer, Errno> {
        match find_announced_name(in_progress, directory, name) {
            Some(AnnouncedName::Vanishing { item, kind }) => Ok(LookupAnswer::Vanishing(self.lookup.describe_vanishing(item, kind))),
            Some(AnnouncedName::Appearing) => Err(Errno::ENOENT),
            None => self.lookup.find_entry(directory, name).map(LookupAnswer::Shown),
        }
    }

    /// getattr. `ItemLookup::find_attributes`: one row by key; a file open for writing is
    /// answered with its writer's file's size and times (design §4); a node whose row is
    /// gone, from the file of the handle the request carries (design §3).
    pub(super) fn answer_getattr(&self, node: LocalItemId, fh: Option<Fh>) -> Result<FileAttr, Errno> {
        self.lookup.find_attributes(node, fh)
    }

    /// opendir. `ItemLookup::find_shown_item`, then `ItemHandles::add_directory`: a node
    /// whose row is gone is `ESTALE` (today opendir always succeeds), and a file is
    /// `ENOTDIR`.
    pub(super) fn answer_opendir(&self, node: LocalItemId) -> Result<Fh, Errno> {
        match self.lookup.find_shown_item(node)?.kind {
            ItemKind::Directory => Ok(self.item_handles.add_directory()),
            ItemKind::File => Err(Errno::ENOTDIR),
        }
    }

    /// readdir and readdirplus: the listing the items of a part are taken from — the
    /// whole of it, kept and handed out, never copied: the caller skips to `offset` and
    /// clones the items it puts into its reply, so that a part costs what it answers. A
    /// reading from the start (`offset` 0) calls `ItemLookup::list_entries` and keeps it
    /// (`ItemHandles::keep_listed_items`); a later part reads what was kept, as today:
    /// offsets are places in that one listing. readdirplus then calls
    /// [`describe_listed_item`](Self::describe_listed_item) for each item it puts into
    /// its reply.
    pub(super) fn answer_readdir(&self, node: LocalItemId, fh: Fh, offset: usize) -> Result<Arc<Vec<Item>>, Errno> {
        if let Some(kept) = self.item_handles.find_listed_items(fh).filter(|_| offset > 0) {
            return Ok(kept);
        }
        let listed = Arc::new(self.lookup.list_entries(node)?);
        self.item_handles.keep_listed_items(fh, Arc::clone(&listed));
        Ok(listed)
    }

    /// readdirplus, for one item of the listing: `ItemLookup::describe`. The attributes
    /// are those of the listed item's own row and of the file at its own row's path
    /// (`PlaceChanges::stat_at_row_path` by the item, not by the listed name): never
    /// another item's, whatever was renamed since the listing. `None` for an item the
    /// listing holds with no local version: left out of the reply.
    pub(super) fn describe_listed_item(&self, item: Item) -> Option<ShownEntry> {
        self.lookup.describe(item).ok()
    }

    /// releasedir. `ItemHandles::close_directory`.
    pub(super) fn answer_releasedir(&self, fh: Fh) {
        self.item_handles.close_directory(fh);
    }

    /// getxattr. `ItemLookup::find_xattr`.
    pub(super) fn answer_getxattr(&self, node: LocalItemId, name: &OsStr) -> Result<Vec<u8>, Errno> {
        self.lookup.find_xattr(node, name)
    }

    /// listxattr. `ItemLookup::list_xattr_names`: no name; the row is read for the node's
    /// `ESTALE` alone.
    pub(super) fn answer_listxattr(&self, node: LocalItemId) -> Result<Vec<&'static str>, Errno> {
        self.lookup.list_xattr_names(node)
    }

    /// open for reading. `decide_open_to_read` of the shown item; a file on disk is
    /// opened through `PlaceChanges::open_at_row_path` (the row and its path read in one
    /// closure of the thread's connection); a file that is not gets a handle that holds
    /// the item and its content (`ItemHandles::add_file`). `ENOENT` from the disk for a
    /// row that says on disk: `ItemFreeUp::mend_missing_copy` as a job of the
    /// write-operation thread, waited for here, then decided once more; a second time it
    /// is `ENOENT`, as today. No local version is written. Opening downloads nothing.
    pub(super) fn answer_open_to_read(&self, node: LocalItemId) -> Result<OpenedFile, Errno> {
        let mut is_mended = false;
        for _try in 0..ROW_TRIES {
            match decide_open_to_read(&self.lookup.find_shown_item(node)?)? {
                OpenToRead::NotOnDisk { item, content } => return Ok(OpenedFile { fh: self.item_handles.add_file(OpenItemFile::ReadingNotOnDisk { item, content }), keeps_cache: true }),
                OpenToRead::BackingFile => {}
            }
            let opened = self.place_changes.open_at_row_path(OpenMode::Read, || {
                self.read_connections.read(|reader| match reader.find_item(node)? {
                    Some(row) if matches!(decide_open_to_read(&row), Ok(OpenToRead::BackingFile)) => Ok(reader.find_path(node)?.map(|path| (path, ()))),
                    Some(_) | None => Ok(None),
                })
            });
            match opened {
                Ok(Some((backing_file, ()))) => return Ok(OpenedFile { fh: self.item_handles.add_file(OpenItemFile::ReadingBackingFile { item: node, backing_file: Arc::new(backing_file) }), keeps_cache: true }),
                // The row is another since it was decided on: decided again.
                Ok(None) => {}
                Err(OpenAtRowError::NoFile { path }) if !is_mended => {
                    is_mended = true;
                    let free_up = Arc::clone(&self.free_up);
                    let mended = self.write_operation_thread.run_and_wait(move || free_up.mend_missing_copy(node)).blocking_recv().map_err(|_| Errno::EIO)?;
                    tracing::warn!("item {node} is opened, its row says its content is on disk, and nothing is at {}: {}", path.display(), if mended { "the row says not on disk now" } else { "the row is left as it is" });
                }
                Err(error) => return Err(Errno::from_i32(decide_open_errno(&error))),
            }
        }
        Err(Errno::EIO)
    }

    /// read, on the mount's thread. A handle on a backing file reads it. One opened not
    /// on disk: the row is read again and `decide_not_on_disk_read_origin` asked with the
    /// content the handle was opened as; then the backing file once the content landed
    /// (`PlaceChanges::open_at_row_path`, whose closure asks the same decision of the row
    /// it reads the path with; `ItemHandles::switch_to_backing_file`), or the file of the
    /// running download of that content (`ItemDownloadStates::find_running_download`),
    /// or a range to fetch, or — for a content OneDrive stated another one for in its
    /// last answer to a download of it (`ItemDownloadStates::is_another_content`, asked
    /// before the running download's mark counts: also while the next download runs and
    /// has no answer) — a range to fetch once the whole download was asked for anew
    /// ([`decide_range_origin`]). `EBADF` for an `fh` that holds no file.
    pub(super) fn decide_read(&self, fh: Fh, offset: u64, size: u32) -> Result<ReadAnswer, Errno> {
        let (item, content) = match self.item_handles.find_file(fh).ok_or(Errno::EBADF)? {
            OpenItemFile::ReadingBackingFile { backing_file, .. } | OpenItemFile::Writing { backing_file, .. } => return read_file_range(&backing_file, offset, size).map(ReadAnswer::Bytes),
            OpenItemFile::ReadingNotOnDisk { item, content } => (item, content),
        };
        for _try in 0..ROW_TRIES {
            let source = match decide_not_on_disk_read_origin(&self.lookup.find_node_row(item)?, &content) {
                NotOnDiskReadOrigin::Refused(errno) => return Err(errno),
                NotOnDiskReadOrigin::OneDriveRange(source) => source,
                NotOnDiskReadOrigin::LandedSince => {
                    let opened = self.place_changes.open_at_row_path(OpenMode::Read, || {
                        // The row the path is read with, in one read: it must still say
                        // the content the handle was opened as is on disk.
                        self.read_connections.read(|reader| match decide_not_on_disk_read_origin(&to_node_row(reader.find_item(item)?), &content) {
                            NotOnDiskReadOrigin::LandedSince => Ok(reader.find_path(item)?.map(|path| (path, ()))),
                            NotOnDiskReadOrigin::OneDriveRange(_) | NotOnDiskReadOrigin::Refused(_) => Ok(None),
                        })
                    });
                    match opened {
                        Ok(Some((backing_file, ()))) => {
                            let backing_file = Arc::new(backing_file);
                            self.item_handles.switch_to_backing_file(fh, Arc::clone(&backing_file));
                            return read_file_range(&backing_file, offset, size).map(ReadAnswer::Bytes);
                        }
                        // The row is another since it was decided on: decided again.
                        Ok(None) => continue,
                        // A read is not answered "no such file": the file is open.
                        Err(OpenAtRowError::NoFile { path }) => {
                            tracing::warn!("item {item} is read, its row says its content is on disk, and nothing is at {}", path.display());
                            return Err(Errno::EIO);
                        }
                        Err(error) => return Err(Errno::from_i32(decide_open_errno(&error))),
                    }
                }
            };
            let key = DownloadKey { item, content: content.clone() };
            // The running download is read first and the memory after it, and the memory
            // decides: a download that ended as another content between the two is not
            // taken for none, and one that asks anew and has no answer yet is joined, not
            // read past.
            let running_download = self.download_states.find_running_download(&key);
            let download = match &running_download {
                // What one answer of OneDrive said: it answers nothing here, and only
                // changes what the read asks of OneDrive.
                _ if self.download_states.is_another_content(&key) => ContentDownload::AnotherContent,
                Some(running) => ContentDownload::WrittenUpTo(running.written_up_to),
                None => ContentDownload::None,
            };
            let to_fetch = |from, end| RangeToFetch { item, from, end, download_at_threshold: ItemToDownload { item, onedrive_item: source.onedrive_item.clone(), content: content.clone() }, source: source.clone() };
            return match decide_range_origin(content.size(), download, offset, size) {
                RangeOrigin::Nothing => Ok(ReadAnswer::Bytes(Vec::new())),
                RangeOrigin::RunningDownload { from, end } => match running_download {
                    Some(running) => read_file_range(&running.file, from, (end - from) as u32).map(ReadAnswer::Bytes),
                    None => Err(Errno::EIO),
                },
                RangeOrigin::OneDrive { from, end } => Ok(ReadAnswer::Range(to_fetch(from, end))),
                RangeOrigin::WholeDownload { from, end } => Ok(ReadAnswer::WholeDownload(to_fetch(from, end))),
            };
        }
        Err(Errno::EIO)
    }

    /// The rest of a read as ranges, on the runtime: `ItemRangeFetcher::fetch_range`
    /// (another content than the one opened is `ESTALE`), the bytes counted towards the
    /// whole download (`count_bytes_read`, then `ItemDownloader::download` as
    /// `DownloadAsker::Read`, with no waiter, at the threshold). No row is read here. The
    /// caller keeps the task's abort handle for the kernel's interrupt, as today
    /// (`interruptible_reads`).
    pub(super) async fn fetch_read_range(&self, range: RangeToFetch) -> Result<Vec<u8>, Errno> {
        let bytes = self.fetch_range(&range).await?;
        if self.download_states.count_bytes_read(range.item, bytes.len() as u64, range.source.content.size()) {
            self.downloader.download(range.download_at_threshold, DownloadAsker::Read, None);
        }
        Ok(bytes)
    }

    /// The rest of a read of a content OneDrive stated another one for
    /// (`ReadAnswer::WholeDownload`), on the runtime: the whole download of the content
    /// is asked for, or joined (`ItemDownloader::download` as `DownloadAsker::Read`, with
    /// a waiter) — it asks OneDrive anew, and its first accepted answer voids what was
    /// remembered; every read of the content that comes before that answer joins it —
    /// and awaited; then what its waiter heard decides ([`decide_read_after_download`]):
    /// `ESTALE` where OneDrive stated another content again, which is remembered again,
    /// and the range fetched where it stated the asked one. That range is not counted
    /// towards a whole download: one just ended. No row is read here. The caller keeps
    /// the task's abort handle for the kernel's interrupt, as for
    /// [`fetch_read_range`](Self::fetch_read_range).
    ///
    /// **What one such read costs.** One asking of OneDrive for the content — through
    /// Graph the item's metadata and the request for its bytes, whose body is dropped
    /// unread where the metadata states another content — in one slot of the pool's open
    /// class, given back when the download ends; reads that come before OneDrive has
    /// answered it join it. While OneDrive states another content again, nothing but
    /// the first such answer is told, and while it gives no answer and the asking keeps
    /// failing the same way, nothing but the first such failure
    /// (`item_downloader::decide_end_news`).
    /// Nothing but the program's own reads bounds how often: a read is asked by a
    /// program, and each is answered what OneDrive states then.
    pub(super) async fn fetch_range_after_download(&self, range: RangeToFetch) -> Result<Vec<u8>, Errno> {
        let (waiter, downloaded) = oneshot::channel();
        self.downloader.download(range.download_at_threshold.clone(), DownloadAsker::Read, Some(waiter));
        decide_read_after_download(downloaded.await.unwrap_or(Err(libc::EIO)))?;
        self.fetch_range(&range).await
    }

    /// The bytes of `range`, from OneDrive (`ItemRangeFetcher::fetch_range`); another
    /// content than the one opened is `ESTALE`.
    async fn fetch_range(&self, range: &RangeToFetch) -> Result<Vec<u8>, Errno> {
        self.range_fetcher.fetch_range(range.item, &range.source, range.from, range.end).await.map_err(|refusal| match refusal {
            RangeRefusal::AnotherContent => Errno::ESTALE,
            RangeRefusal::Failed { errno } => Errno::from_i32(errno),
        })
    }

    /// Downloads `file` for a program that writes it, and waits for the landing: what
    /// the waiter hears is the errno (`ESTALE` for a row or a content that changed,
    /// `EEXIST` for a held place, `EIO` otherwise, also for a download that was dropped).
    async fn download_to_write(&self, file: ItemToDownload) -> Result<(), Errno> {
        let (waiter, downloaded) = oneshot::channel();
        self.downloader.download(file, DownloadAsker::OpenForWriting, Some(waiter));
        match downloaded.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(errno)) => Err(Errno::from_i32(errno)),
            Err(_) => Err(Errno::EIO),
        }
    }

    /// Has `job` run on the write-operation thread and `reply` answered with what it
    /// gives, in a folder that takes changes; a read-only folder's `reply` is refused
    /// here, at once. A thread that is stopped drops the job and `reply` with it: the
    /// kernel's reply answers an error when it is dropped.
    fn queue_change<T: 'static>(&self, reply: ReplyTo<T>, job: impl FnOnce() -> Result<T, Errno> + Send + 'static) {
        if let Err(read_only) = self.access_mode_switch.check_read_write() {
            return reply(Err(read_only));
        }
        self.write_operation_thread.run(move || reply(job()));
    }

    /// open for writing, without `O_TRUNC`, on the mount's thread: what must be on disk
    /// first (`ItemWriteOperations::find_file_to_fill`). `EROFS` in a read-only folder.
    pub(super) fn decide_open_to_write(&self, node: LocalItemId) -> Result<FileToFill, Errno> {
        self.write_operations.find_file_to_fill(node, None)
    }

    /// The rest of an open for writing, on the runtime: the download awaited
    /// (`ItemDownloader::download` as `DownloadAsker::OpenForWriting` with a waiter: `ESTALE`, `EEXIST` and `EIO` reach the
    /// program), then `open_to_write` queued as a job of the write-operation thread, which
    /// writes `OpenedForWriting` for the first writer before the file is opened and
    /// answers `reply`. With nothing to fill, the caller queues the job at once
    /// ([`queue_open_to_write`](Self::queue_open_to_write)).
    pub(super) async fn fill_and_open_to_write(&self, node: LocalItemId, fill: ItemToDownload, reply: ReplyTo<OpenedFile>) {
        match self.download_to_write(fill).await {
            Ok(()) => self.queue_open_to_write(node, reply),
            Err(errno) => reply(Err(errno)),
        }
    }

    /// `open_to_write` as a job of the write-operation thread, which answers `reply`.
    pub(super) fn queue_open_to_write(&self, node: LocalItemId, reply: ReplyTo<OpenedFile>) {
        let write_operations = Arc::clone(&self.write_operations);
        self.queue_change(reply, move || write_operations.open_to_write(node).map(|opened| OpenedFile { fh: opened.fh, keeps_cache: false }));
    }

    /// open with `O_TRUNC`. `open_to_overwrite` on the write-operation thread: nothing is
    /// downloaded.
    pub(super) fn queue_open_to_overwrite(&self, node: LocalItemId, reply: ReplyTo<OpenedFile>) {
        let write_operations = Arc::clone(&self.write_operations);
        self.queue_change(reply, move || write_operations.open_to_overwrite(node).map(|opened| OpenedFile { fh: opened.fh, keeps_cache: false }));
    }

    /// mkdir, mknod, create. `ItemWriteOperations::make` on the write-operation thread:
    /// the disk, then `LocalVersionChange::Made`; a created file made and opened in one
    /// transaction. The operation answers the made item's row (`MadeItem::item`): the
    /// attributes of the reply are made here, in the job, from that row
    /// ([`WriteReplyAttributes`]: `ItemLookup::describe_through`, with the facts of the
    /// backing file).
    /// For the bridge's own thread (mkdir, mknod): answered from the rows, on the mount's
    /// thread, nothing made.
    pub(super) fn queue_make(&self, directory: LocalItemId, name: &OsStr, kind: MadeKind, thread_id: u32, reply: ReplyTo<MadeEntry>) {
        if self.inotify_bridge.is_own_thread(thread_id) {
            return reply(self.lookup.find_entry(directory, name).map(|entry| MadeEntry { entry, fh: Fh::NOT_OPENED }));
        }
        let (write_operations, reply_attributes, name) = (Arc::clone(&self.write_operations), Arc::clone(&self.reply_attributes), name.to_owned());
        self.queue_change(reply, move || {
            let made = write_operations.make(directory, &name, kind)?;
            Ok(MadeEntry { entry: reply_attributes.describe(made.item)?, fh: made.fh })
        });
    }

    /// unlink and rmdir. `ItemWriteOperations::remove` on the write-operation thread: the
    /// disk, then `LocalVersionChange::Removed`. For the bridge's own thread: answered
    /// from the rows ([`decide_announced_removal`]), nothing removed.
    pub(super) fn queue_remove(&self, directory: LocalItemId, name: &OsStr, asked_kind: ItemKind, thread_id: u32, reply: ReplyTo<()>) {
        if self.inotify_bridge.is_own_thread(thread_id) {
            return reply(decide_announced_removal(self.lookup.find_entry(directory, name).map(drop)));
        }
        let (write_operations, name) = (Arc::clone(&self.write_operations), name.to_owned());
        self.queue_change(reply, move || write_operations.remove(directory, &name, asked_kind));
    }

    /// rename. `ItemWriteOperations::rename` on the write-operation thread: the disk, then
    /// `Moved` or `SavedOver`. An exchange is `EINVAL` there, and `EROFS` here in a
    /// read-only folder, as today.
    pub(super) fn queue_rename(&self, directory: LocalItemId, name: &OsStr, new_directory: LocalItemId, new_name: &OsStr, mode: RenameMode, reply: ReplyTo<()>) {
        let (write_operations, name, new_name) = (Arc::clone(&self.write_operations), name.to_owned(), new_name.to_owned());
        self.queue_change(reply, move || write_operations.rename(directory, &name, new_directory, &new_name, mode));
    }

    /// setattr with a size, on the mount's thread: what must be on disk first
    /// (`find_file_to_fill` with the size). A file a program holds open for writing has
    /// its content on disk, and is asked nothing of its row: so a truncation through the
    /// handle of a file that was removed or saved over since is not refused here (design
    /// §3). `EROFS` in a read-only folder.
    pub(super) fn decide_truncate(&self, node: LocalItemId, size: u64) -> Result<FileToFill, Errno> {
        if self.item_handles.is_open_for_writing(node) {
            self.access_mode_switch.check_read_write()?;
            return Ok(FileToFill::Nothing);
        }
        self.write_operations.find_file_to_fill(node, Some(size))
    }

    /// The rest of a truncation: with a download, awaited on the runtime first; then
    /// [`queue_truncate`](Self::queue_truncate). With nothing to fill, the caller queues
    /// the job at once.
    pub(super) async fn fill_and_truncate(&self, node: LocalItemId, fh: Option<Fh>, size: u64, fill: FileToFill, reply: ReplyTo<FileAttr>) {
        let downloaded = match fill {
            FileToFill::Nothing => Ok(()),
            FileToFill::Download(file) => self.download_to_write(file).await,
        };
        match downloaded {
            Ok(()) => self.queue_truncate(node, fh, size, reply),
            Err(errno) => reply(Err(errno)),
        }
    }

    /// `ItemWriteOperations::truncate`, with the `fh` the request carries, as a job of the
    /// write-operation thread; the operation answers no attributes, and `reply` is
    /// answered as getattr answers, with the `fh` ([`WriteReplyAttributes`]).
    pub(super) fn queue_truncate(&self, node: LocalItemId, fh: Option<Fh>, size: u64, reply: ReplyTo<FileAttr>) {
        let (write_operations, reply_attributes) = (Arc::clone(&self.write_operations), Arc::clone(&self.reply_attributes));
        self.queue_change(reply, move || {
            write_operations.truncate(node, fh, size)?;
            reply_attributes.find_attributes(node, fh)
        });
    }

    /// setattr with a time and no size. `ItemWriteOperations::set_modified_time`, with the
    /// `fh` the request carries; the operation answers no attributes, and `reply` is
    /// answered as getattr answers, with the `fh` ([`WriteReplyAttributes`]). Mode, owner
    /// and the other times are ignored, as today.
    /// For the bridge's own thread: answered as getattr, on the mount's thread.
    pub(super) fn queue_set_modified_time(&self, node: LocalItemId, fh: Option<Fh>, mtime: i64, mtime_nsec: u32, thread_id: u32, reply: ReplyTo<FileAttr>) {
        if self.inotify_bridge.is_own_thread(thread_id) {
            return reply(self.answer_getattr(node, fh));
        }
        let (write_operations, reply_attributes) = (Arc::clone(&self.write_operations), Arc::clone(&self.reply_attributes));
        self.queue_change(reply, move || {
            write_operations.set_modified_time(node, fh, mtime, mtime_nsec)?;
            reply_attributes.find_attributes(node, fh)
        });
    }

    /// write. `ItemHandles::find_file_to_write`, `mark_written`, the bytes into the
    /// backing file, on the mount's own thread. No row is touched (design §8). `EBADF`
    /// for a handle that may not write.
    pub(super) fn answer_write(&self, fh: Fh, offset: u64, bytes: &[u8]) -> Result<u32, Errno> {
        let (item, backing_file) = self.item_handles.find_file_to_write(fh).ok_or(Errno::EBADF)?;
        self.item_handles.mark_written(item);
        backing_file.write_all_at(bytes, offset).map_err(|error| to_errno(&error))?;
        Ok(bytes.len() as u32)
    }

    /// fsync. The backing file of the handle synced, on the mount's own thread. Nothing
    /// for a handle that may not write: it wrote nothing.
    pub(super) fn answer_fsync(&self, fh: Fh) -> Result<(), Errno> {
        match self.item_handles.find_file_to_write(fh) {
            Some((_, backing_file)) => backing_file.sync_all().map_err(|error| to_errno(&error)),
            None => Ok(()),
        }
    }

    /// release. The kernel is answered at once; a reader's handle is closed on the
    /// mount's thread (`ItemHandles::close_reader`), a writer's as a job of the
    /// write-operation thread (`ItemWriteOperations::close_writer`), which writes
    /// `ClosedChanged` or `ClosedUnchanged` at the last close and only then takes the
    /// writer out. Queued whatever the access mode is: the close of a file that was open
    /// when the folder went read-only is written.
    pub(super) fn answer_release(&self, fh: Fh) {
        if self.item_handles.find_file_to_write(fh).is_none() {
            return self.item_handles.close_reader(fh);
        }
        let write_operations = Arc::clone(&self.write_operations);
        self.write_operation_thread.run(move || write_operations.close_writer(fh));
    }
}
