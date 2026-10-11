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
//! | read | mount, then runtime for a range | `decide_read`, `fetch_read_range` | — |
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
//! from the row read through the writer and the backing file — never through
//! `ItemLookup`, which reads on the connection of a thread of the mount.
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
// test: a_truncation_to_nothing_of_a_file_not_on_disk_downloads_nothing
// test: a_truncation_of_a_file_not_on_disk_downloads_it_first
// test: a_created_file_written_and_closed_is_one_unsent_item_with_its_bytes
// test: a_file_renamed_while_it_is_open_is_closed_into_the_row_at_its_new_place
// test: a_directory_read_during_renames_describes_each_listed_item_as_itself
// test: the_bridges_own_requests_are_answered_from_the_rows_and_make_nothing
// test: after_the_take_down_was_asked_every_change_is_refused_and_reading_goes_on
// test: each_refusal_of_a_write_operation_reaches_the_reply_with_its_errno
// test: a_request_by_a_handle_that_is_not_open_is_ebadf
// test: a_file_opened_to_be_overwritten_is_cut_and_nothing_is_downloaded
// test: the_time_set_on_a_file_is_what_its_reply_and_the_next_getattr_say
#![allow(dead_code)]

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use fuser::{Errno, FileAttr};
use konedrive_tree::{ContentPresence, Item, ItemKind, LocalItemId, Store};
use tokio::sync::oneshot;

use super::access_mode_switch::AccessModeSwitch;
use super::backing_directory::EntryKind;
use super::errno::to_errno;
use super::item_announcements::{find_announced_name, AnnouncedName, ItemAnnouncement};
use super::item_attributes::{to_item_attributes, to_open_file_attributes, BackingFileFacts, OpenFileFacts, SizeAndTimes};
use super::item_download_states::{DownloadAsker, DownloadKey, ItemDownloadStates};
use super::item_downloader::{ItemDownloader, ItemToDownload};
use super::item_free_up::ItemFreeUp;
use super::item_handles::{ItemHandles, OpenItemFile};
use super::item_inotify_bridge::ItemInotifyBridge;
use super::item_lookup::{ItemLookup, ShownEntry};
use super::item_range_fetcher::{ItemRangeFetcher, RangeRefusal};
use super::item_read_origin::{decide_not_on_disk_read_origin, decide_open_to_read, NotOnDiskReadOrigin, OpenToRead, RangeSource};
use super::item_write_operations::{FileToFill, ItemWriteOperations, MadeKind, RenameMode};
use super::open_handles::Fh;
use super::place_changes::{decide_open_errno, OpenAtRowError, OpenMode, PlaceChanges, RowPathStat};
use super::read_connections::ReadConnections;
use super::write_operation_thread::WriteOperationThread;

#[cfg(test)]
mod tests;

/// For how long the kernel may keep the attributes and the name of an entry the folder
/// shows, as today (`attributes::TTL`).
const ATTRIBUTE_LIFETIME: Duration = Duration::from_secs(1);

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
}

/// Where the `size` bytes at `offset` of a content `content_size` long come from, when a
/// download of that content has written its file up to `written_up_to` in order (`None`:
/// none runs, or it has no file yet). The read ends at the content's end. Pure.
pub(super) fn decide_range_origin(content_size: u64, written_up_to: Option<u64>, offset: u64, size: u32) -> RangeOrigin {
    let end = offset.saturating_add(u64::from(size)).min(content_size);
    if offset >= end {
        return RangeOrigin::Nothing;
    }
    match written_up_to {
        Some(written_up_to) if end.saturating_add(WRITTEN_BEHIND) <= written_up_to => RangeOrigin::RunningDownload { from: offset, end },
        Some(_) | None => RangeOrigin::OneDrive { from: offset, end },
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

/// What `fstat` says of the file a handle holds.
fn read_open_file_facts(file: &File) -> Result<OpenFileFacts, Errno> {
    let metadata = file.metadata().map_err(|error| to_errno(&error))?;
    Ok(OpenFileFacts { size: metadata.size(), mtime: metadata.mtime(), mtime_nsec: metadata.mtime_nsec() as u32, blocks: metadata.blocks() })
}

/// The attributes a write operation's reply carries, made on the write-operation thread
/// once the operation wrote its row: of the item mkdir, mknod and create made, and of the
/// node a truncation or a time was set on.
///
/// **State owned:** none. It reads the row through the writer (`Store::call_blocking`),
/// as every job of that thread reads, and the backing file through the writer's handle
/// or `PlaceChanges::stat_at_row_path`; `ItemLookup` does the same for the reading
/// requests on the connection of a thread of the mount, which this thread has none of.
struct WriteReplyAttributes {
    store: Store,
    place_changes: Arc<PlaceChanges>,
    item_handles: Arc<ItemHandles>,
    access_mode_switch: Arc<AccessModeSwitch>,
    /// Unix seconds: the time of a directory that has none of its own.
    mount_started_at: i64,
}

impl WriteReplyAttributes {
    /// What the backing directory says of `item`, which the folder shows: as
    /// `ItemLookup::read_backing_file_facts` says it, with the path read through the
    /// writer.
    fn read_backing_file_facts(&self, item: &Item) -> BackingFileFacts {
        let no_facts = BackingFileFacts { size_and_times: SizeAndTimes::OfLocalContent, blocks: 0, directory_mtime: None };
        let node = item.local_item_id;
        if item.kind == ItemKind::File {
            if let Some(Ok(facts)) = self.item_handles.find_writer_file(node).map(|file| read_open_file_facts(&file)) {
                return BackingFileFacts { size_and_times: SizeAndTimes::OfWritersFile { size: facts.size, mtime: facts.mtime, mtime_nsec: facts.mtime_nsec }, blocks: facts.blocks, directory_mtime: None };
            }
            if item.content_presence == ContentPresence::NotOnDisk {
                return no_facts;
            }
        }
        let store = &self.store;
        let stat = self.place_changes.stat_at_row_path(|| store.call_blocking(move |store| Ok(store.item_store().reader().find_path(node)?.map(|path| (path, ())))));
        match (stat, item.kind) {
            (Ok(RowPathStat::Facts { facts, row: () }), ItemKind::File) if facts.kind == EntryKind::File => BackingFileFacts { blocks: facts.blocks, ..no_facts },
            (Ok(RowPathStat::Facts { facts, row: () }), ItemKind::Directory) if facts.kind == EntryKind::Directory => {
                let modified = facts.modified.duration_since(UNIX_EPOCH).unwrap_or_default();
                BackingFileFacts { directory_mtime: Some((modified.as_secs() as i64, modified.subsec_nanos())), ..no_facts }
            }
            (Ok(_), _) => no_facts,
            (Err(error), _) => {
                tracing::warn!("the path of item {node} could not be read for the attributes of a reply: {error}");
                no_facts
            }
        }
    }

    /// The attributes of `item`, a row the folder shows; `ESTALE` for one it does not.
    fn describe(&self, item: &Item) -> Result<FileAttr, Errno> {
        to_item_attributes(item, self.access_mode_switch.access_mode(), &self.read_backing_file_facts(item), self.mount_started_at).ok_or(Errno::ESTALE)
    }

    /// The attributes of `node` after a truncation or a time set on it, as getattr
    /// answers them (`ItemLookup::find_attributes`): of its row when the folder shows it;
    /// of the file `fh` holds when the row is gone or shows nothing and the handle is
    /// that node's (design §3); `ESTALE` otherwise.
    fn find_attributes(&self, node: LocalItemId, fh: Option<Fh>) -> Result<FileAttr, Errno> {
        let row = self.store.call_blocking(move |store| store.item_store().reader().find_item(node)).map_err(|error| {
            tracing::warn!("the row of item {node} could not be read for the attributes of a reply: {error}");
            Errno::EIO
        })?;
        if let Some(item) = row.filter(|row| row.local.is_some()) {
            return self.describe(&item);
        }
        match fh.and_then(|fh| self.item_handles.find_file(fh)) {
            Some(OpenItemFile::Writing { item, backing_file } | OpenItemFile::ReadingBackingFile { item, backing_file }) if item == node => Ok(to_open_file_attributes(node, self.access_mode_switch.access_mode(), &read_open_file_facts(&backing_file)?)),
            Some(_) | None => Err(Errno::ESTALE),
        }
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
    /// Made by the mount's build, last before the session. `store` and
    /// `mount_started_at` are for the attributes a write operation's reply carries
    /// ([`WriteReplyAttributes`]); `access_mode_switch` also refuses a change of a
    /// read-only folder before it is queued.
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
        mount_started_at: i64,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let reply_attributes = Arc::new(WriteReplyAttributes { store, place_changes: Arc::clone(&place_changes), item_handles: Arc::clone(&item_handles), access_mode_switch: Arc::clone(&access_mode_switch), mount_started_at });
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

    /// readdir and readdirplus: the items of the directory from `offset` on. A reading
    /// from the start (`offset` 0) calls `ItemLookup::list_entries` and keeps it
    /// (`ItemHandles::keep_listed_items`); a later part reads what was kept, as today:
    /// offsets are places in that one listing. readdirplus then calls
    /// [`describe_listed_item`](Self::describe_listed_item) for each item it puts into
    /// its reply.
    pub(super) fn answer_readdir(&self, node: LocalItemId, fh: Fh, offset: usize) -> Result<Vec<Item>, Errno> {
        let kept = self.item_handles.find_listed_items(fh).filter(|_| offset > 0);
        let listed = match kept {
            Some(kept) => kept,
            None => {
                let listed = Arc::new(self.lookup.list_entries(node)?);
                self.item_handles.keep_listed_items(fh, Arc::clone(&listed));
                listed
            }
        };
        Ok(listed.iter().skip(offset).cloned().collect())
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

    /// listxattr. `ItemLookup::list_xattr_names`: one read of the row, where today each of
    /// six names resolves the node again.
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
    /// (`PlaceChanges::open_at_row_path`, `ItemHandles::switch_to_backing_file`), or the
    /// file of the running download of that content
    /// (`ItemDownloadStates::find_running_download`), or a range to fetch
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
                        self.read_connections.read(|reader| match reader.find_item(item)? {
                            Some(row) if row.content_presence == ContentPresence::OnDisk && row.local.as_ref().and_then(|local| local.content.as_ref()) == Some(&content) => Ok(reader.find_path(item)?.map(|path| (path, ()))),
                            Some(_) | None => Ok(None),
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
            let running_download = self.download_states.find_running_download(&DownloadKey { item, content: content.clone() });
            return match decide_range_origin(content.size(), running_download.as_ref().map(|running| running.written_up_to), offset, size) {
                RangeOrigin::Nothing => Ok(ReadAnswer::Bytes(Vec::new())),
                RangeOrigin::RunningDownload { from, end } => match running_download {
                    Some(running) => read_file_range(&running.file, from, (end - from) as u32).map(ReadAnswer::Bytes),
                    None => Err(Errno::EIO),
                },
                RangeOrigin::OneDrive { from, end } => {
                    let download_at_threshold = ItemToDownload { item, onedrive_item: source.onedrive_item.clone(), content };
                    Ok(ReadAnswer::Range(RangeToFetch { item, source, from, end, download_at_threshold }))
                }
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
        let bytes = self.range_fetcher.fetch_range(range.item, &range.source, range.from, range.end).await.map_err(|refusal| match refusal {
            RangeRefusal::AnotherContent => Errno::ESTALE,
            RangeRefusal::Failed { errno } => Errno::from_i32(errno),
        })?;
        if self.download_states.count_bytes_read(range.item, bytes.len() as u64, range.source.content.size()) {
            self.downloader.download(range.download_at_threshold, DownloadAsker::Read, None);
        }
        Ok(bytes)
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
    /// ([`WriteReplyAttributes`]: `item_attributes::to_item_attributes`, with the facts of
    /// the backing file).
    /// For the bridge's own thread (mkdir, mknod): answered from the rows, on the mount's
    /// thread, nothing made.
    pub(super) fn queue_make(&self, directory: LocalItemId, name: &OsStr, kind: MadeKind, thread_id: u32, reply: ReplyTo<MadeEntry>) {
        if self.inotify_bridge.is_own_thread(thread_id) {
            return reply(self.lookup.find_entry(directory, name).map(|entry| MadeEntry { entry, fh: Fh::NOT_OPENED }));
        }
        let (write_operations, reply_attributes, name) = (Arc::clone(&self.write_operations), Arc::clone(&self.reply_attributes), name.to_owned());
        self.queue_change(reply, move || {
            let made = write_operations.make(directory, &name, kind)?;
            let attributes = reply_attributes.describe(&made.item)?;
            Ok(MadeEntry { entry: ShownEntry { item: made.item, attributes }, fh: made.fh })
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
