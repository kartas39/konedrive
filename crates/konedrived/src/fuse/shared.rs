//! What holds a mount together: its parts, and what is under way.
//!
//! The parts own their state and are shared by `Arc` with whoever needs them: the disk
//! ([`BackingDirectory`]), what each directory holds ([`DirectoryContents`]), the node ids
//! given to the kernel ([`NodeTable`]) and what looks them up ([`NodeLookup`]), the files open
//! for writing ([`OpenWriters`]), the thread that records ([`OutboxRecorder`]), the operations
//! that change the folder ([`WriteOperations`]) and what moves the user's own out of the way
//! of a change from OneDrive ([`ConflictCopies`]). What is not a part yet — the downloads, the
//! reads, the pins, the announcer, the mode — is still a field here with its code in
//! `impl Shared` in the module of its topic.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::Errno;
use konedrive_graph::pool::TransferPool;
use konedrive_tree::Store;

use crate::folder::root::SyncRoot;
use crate::hydration::source::{ContentSource, Share};
use crate::status::report::Report;

use super::announce::{Announce, Told};
use super::fill::{Running, Waiter};
use super::conflict_copies::ConflictCopies;
use super::node_table::NodeTable;
use super::outbox_recorder::OutboxRecorder;
use super::write_operations::WriteOperations;
use super::open_writers::OpenWriters;
use super::read::{OpenFile, Reader};
use super::node_lookup::NodeLookup;
use super::backing_directory::BackingDirectory;
use super::directory_contents::DirectoryContents;

pub(super) struct Shared {
    pub(super) root: SyncRoot,
    /// The directory on disk the mount lies over ([`super::backing_directory`]).
    pub(super) backing_directory: Arc<BackingDirectory>,
    /// What each directory of the mount holds ([`super::directory_contents`]).
    pub(super) directory_contents: Arc<DirectoryContents>,
    /// The store's one writer, for the pins.
    pub(super) store: Store,
    /// The node ids given to the kernel ([`super::node_table`]): shared by what looks them up
    /// and what tells the kernel of changes.
    pub(super) node_table: Arc<Mutex<NodeTable>>,
    /// The entry a node id stands for, and the node id of an entry ([`super::node_lookup`]).
    pub(super) node_lookup: Arc<NodeLookup>,
    pub(super) source: Arc<dyn ContentSource>,
    pub(super) report: Report,
    pub(super) pool: Arc<TransferPool>,
    /// What the daemon's large downloads in parts share.
    pub(super) parts: Arc<Share>,
    /// The downloads in progress, by item, and who waits for each.
    pub(super) fills: Mutex<HashMap<String, Vec<Waiter>>>,
    /// Of those, the ones in one stream: what of each is on disk already.
    pub(super) running: Mutex<HashMap<String, Running>>,
    /// The downloads that failed, by item, oldest first: each one's file, which has no name
    /// and goes with the daemon. The next download of the item continues from its checkpoint.
    pub(super) unfinished: Mutex<Vec<(String, Arc<File>)>>,
    /// What programs hold open, by the handle the kernel was given.
    pub(super) handles: Mutex<HashMap<u64, OpenFile>>,
    pub(super) next_handle: AtomicU64,
    /// What each open directory held when it was first listed, by handle: a program that
    /// reads a directory in parts reads one listing, whatever changes meanwhile.
    pub(super) listings: Mutex<HashMap<u64, Arc<Vec<super::directory_contents::DirectoryEntry>>>>,
    /// How many bytes of each item that is not downloaded were read as ranges.
    pub(super) asked: Mutex<HashMap<String, u64>>,
    /// Once that many bytes of a file were read as ranges, the file is downloaded whole
    /// (`[transfers] download_after` in `config.toml`).
    pub(super) download_after: u64,
    /// The files being read as ranges, by item: each holds the account's pool as a file being
    /// opened does, until `READ_LINGERS` after its last read. While one is held no pinned
    /// or other background download starts, so what the user reads goes first.
    pub(super) readers: Mutex<HashMap<String, Reader>>,
    /// The reads waiting for a range from OneDrive, by request: a program that is interrupted
    /// has its read given up.
    pub(super) reads: Mutex<HashMap<u64, tokio::task::AbortHandle>>,
    /// The items that carry a pin of their own, as the store has them.
    pub(super) pins: Mutex<HashSet<String>>,
    /// The announcer: its thread, what it is telling, and the way to it.
    pub(super) announcer: AtomicI32,
    pub(super) told: Mutex<Option<Told>>,
    pub(super) tell: Mutex<Option<mpsc::Sender<Vec<Announce>>>>,
    /// What the cycle now committing changed: announced, and the pins of what went taken off,
    /// once it is committed.
    pub(super) pending: Mutex<(Vec<Announce>, Vec<String>)>,
    /// The mount is being taken off: the announcer says nothing more through it.
    pub(super) stopping: std::sync::atomic::AtomicBool,
    /// Whether programs may change the folder ([`super::mode`]): shared with the recorder.
    pub(super) writable: Arc<AtomicBool>,
    /// What moves the user's own out of the way of a change from OneDrive
    /// ([`super::conflict_copies`]).
    pub(super) conflict_copies: ConflictCopies,
    /// The thread that makes and records each change ([`super::outbox_recorder`]).
    pub(super) outbox_recorder: Arc<OutboxRecorder>,
    /// The files programs hold open for writing, by node id ([`super::open_writers`]).
    pub(super) open_writers: Arc<OpenWriters>,
    /// The operations that change the folder ([`super::write_operations`]).
    pub(super) write_operations: WriteOperations,
    pub(super) runtime: tokio::runtime::Handle,
}

pub(super) fn errno(e: &io::Error) -> Errno {
    Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))
}

pub(super) fn seconds(t: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(t.max(0) as u64)
}

pub(super) fn other(e: impl ToString) -> io::Error {
    io::Error::other(e.to_string())
}

impl Shared {
    /// The path a download is shown under: the file's place in the folder.
    pub(super) fn shown(&self, rel: &Path) -> String {
        self.root.path.join(rel).display().to_string()
    }

    /// The time the outbox's rows are dated by.
    pub(super) fn now(&self) -> i64 {
        crate::clock::unix_now()
    }
}
