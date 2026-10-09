//! What holds a mount together: its parts, and what is under way.
//!
//! The parts own their state and are shared by `Arc` with whoever needs them: the folder under
//! the mount ([`Under`]), the folder as the mount shows it ([`View`]), the kernel's nodes
//! ([`Nodes`]) and what resolves them ([`Resolve`]), the files open for writing ([`Writers`]).
//! What is not a part yet — the downloads, the reads, the pins, the announcer, the mode — is
//! still a field here with its code in `impl Shared` in the module of its topic.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicI32, AtomicU64};
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
use super::nodes::Nodes;
use super::policy::Policy;
use super::recorder::Job;
use super::writers::Writers;
use super::read::{Handle, Reader};
use super::resolve::Resolve;
use super::under::Under;
use super::view::View;

pub(super) struct Shared {
    pub(super) root: SyncRoot,
    /// The folder under the mount ([`super::under`]).
    pub(super) under: Arc<Under>,
    /// The folder as the mount shows it ([`super::view`]).
    pub(super) view: Arc<View>,
    /// The store's one writer, for the pins.
    pub(super) store: Store,
    /// The node ids the kernel knows ([`super::nodes`]): shared by what resolves them and what
    /// tells the kernel of changes.
    pub(super) nodes: Arc<Mutex<Nodes>>,
    /// Which entry of the folder a node is ([`super::resolve`]).
    pub(super) resolve: Arc<Resolve>,
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
    pub(super) handles: Mutex<HashMap<u64, Handle>>,
    pub(super) next_handle: AtomicU64,
    /// What each open directory held when it was first listed, by handle: a program that
    /// reads a directory in parts reads one listing, whatever changes meanwhile.
    pub(super) listings: Mutex<HashMap<u64, Arc<Vec<super::view::Entry>>>>,
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
    /// Whether programs may change the folder ([`super::mode`]).
    pub(super) writable: std::sync::atomic::AtomicBool,
    /// What of the folder is sent at all ([`super::policy`]).
    pub(super) policy: Arc<Policy>,
    /// What a folder that takes changes shares with its sync.
    pub(super) writing: Writing,
    /// Wakes the outbox's worker: set while a sync that uploads runs.
    pub(super) wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// The way to the recorder's thread ([`super::recorder`]).
    pub(super) jobs: Mutex<Option<mpsc::Sender<Job>>>,
    /// The files programs hold open for writing, by node ([`super::writers`]).
    pub(super) writers: Writers,
    pub(super) runtime: tokio::runtime::Handle,
}

/// What a folder that takes changes shares with its sync.
pub struct Writing {
    /// The folder's tree lock: a change is recorded under it, as a cycle of the listing
    /// holds it from staging to its swap.
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// The account's ignore list: what stays on this computer only.
    pub ignore: crate::local::SharedIgnore,
    /// For conflict copies: `name-<machine>.ext`.
    pub machine_name: String,
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
