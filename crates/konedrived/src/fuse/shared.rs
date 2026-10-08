//! What every part of a mount works on: the folder under it, the store, and what is under way.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicU64};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::Errno;
use konedrive_graph::pool::TransferPool;
use konedrive_tree::{Kind, Row, Store, Table, TreeStore};

use crate::folder::root::SyncRoot;
use crate::hydration::source::{ContentSource, Share};
use crate::status::report::Report;

use super::announce::{Announce, Told};
use super::fill::{Running, Waiter};
use super::nodes::Nodes;
use super::outbox::Waiting;
use super::recorder::Job;
use super::writers::Writers;
use super::read::{Handle, Reader};

pub(super) struct Shared {
    pub(super) root: SyncRoot,
    /// The folder under the mount, opened before the mount.
    pub(super) folder: File,
    /// The store, for reading alone: what the mount's requests ask.
    pub(super) reader: Mutex<TreeStore>,
    /// The store's one writer, for the pins.
    pub(super) store: Store,
    pub(super) nodes: Mutex<Nodes>,
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
    /// What a folder that takes changes shares with its sync.
    pub(super) writing: Writing,
    /// Wakes the outbox's worker: set while a sync that uploads runs.
    pub(super) wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// The way to the recorder's thread ([`super::recorder`]).
    pub(super) jobs: Mutex<Option<mpsc::Sender<Job>>>,
    /// The files programs hold open for writing, by node ([`super::writers`]).
    pub(super) writers: Writers,
    /// What the outbox's rows say of the folder, as last read ([`super::outbox`]).
    pub(super) waiting: Mutex<Arc<Waiting>>,
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
    /// A path in the folder under the mount: through the descriptor, never through the mount.
    pub(super) fn under(&self, rel: &Path) -> PathBuf {
        konedrive_fs::proc_path(&self.folder).join(rel)
    }

    pub(super) fn read<T>(&self, ask: impl FnOnce(&TreeStore) -> Result<T, konedrive_tree::TreeError>) -> Option<T> {
        ask(&crate::panic::lock(&self.reader)).ok()
    }

    pub(super) fn row(&self, id: &str) -> Option<Row> {
        self.read(|s| s.get(Table::Items, id)).flatten()
    }

    /// The path a download is shown under: the file's place in the folder.
    pub(super) fn shown(&self, rel: &Path) -> String {
        self.root.path.join(rel).display().to_string()
    }

    /// The files at and below `row`, each with its place in the folder.
    pub(super) fn files_below(&self, row: &Row) -> Vec<(Row, PathBuf)> {
        let files: Vec<Row> = {
            let store = crate::panic::lock(&self.reader);
            let mut ids = store.descendants(Table::Items, &row.id).unwrap_or_default();
            ids.push(row.id.clone());
            ids.iter().filter_map(|id| store.get(Table::Items, id).ok().flatten().filter(|r| r.kind == Kind::File)).collect()
        };
        files.into_iter().filter_map(|file| self.place(&file.id).map(|at| (file, at))).collect()
    }

    /// The time the outbox's rows are dated by.
    pub(super) fn now(&self) -> i64 {
        crate::clock::unix_now()
    }
}
