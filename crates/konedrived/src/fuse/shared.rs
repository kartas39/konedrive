//! What every part of a mount works on: the folder under it, the store, and what is under way.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicU64};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{Errno, INodeNo};
use konedrive_graph::pool::TransferPool;
use konedrive_tree::{Kind, Placement, Row, Store, Table, TreeStore};

use crate::folder::root::SyncRoot;
use crate::hydration::source::{ContentSource, Share};
use crate::status::report::Report;

use super::announce::{Announce, Told};
use super::fill::{Running, Waiter};
use super::nodes::Nodes;
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
    /// opened does, until [`READ_LINGERS`] after its last read. While one is held no pinned
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
    /// A path in the folder under the mount: through the descriptor, never through the mount.
    pub(super) fn under(&self, rel: &Path) -> PathBuf {
        konedrive_fs::proc_path(&self.folder).join(rel)
    }

    pub(super) fn read<T>(&self, ask: impl FnOnce(&TreeStore) -> Result<T, konedrive_tree::TreeError>) -> Option<T> {
        ask(&crate::panic::lock(&self.reader)).ok()
    }

    pub(super) fn id(&self, ino: INodeNo) -> Option<String> {
        if ino.0 == 1 {
            return self.read(|s| s.root_item_id()).flatten();
        }
        crate::panic::lock(&self.nodes).items.get(&ino.0).map(|n| n.id.clone())
    }

    pub(super) fn row(&self, id: &str) -> Option<Row> {
        self.read(|s| s.get(Table::Items, id)).flatten()
    }

    pub(super) fn rel_of_ino(&self, ino: u64) -> Option<PathBuf> {
        crate::panic::lock(&self.nodes).rel(ino)
    }

    pub(super) fn children(&self, id: &str) -> Vec<Row> {
        let rows = self.read(|s| s.children(Table::Items, id)).unwrap_or_default();
        rows.into_iter().filter(|r| r.placement == Placement::Placed).collect()
    }

    pub(super) fn child(&self, parent: INodeNo, name: &OsStr) -> Option<Row> {
        let (dir, name) = self.id(parent).zip(name.to_str())?;
        self.read(|s| s.child(Table::Items, &dir, name)).flatten().filter(|r| r.placement == Placement::Placed)
    }

    /// The path a download is shown under: the file's place in the folder.
    pub(super) fn shown(&self, rel: &Path) -> String {
        self.root.path.join(rel).display().to_string()
    }

    /// The row at `rel`, name by name from the root.
    pub(super) fn resolve(&self, rel: &Path) -> Option<Row> {
        let store = crate::panic::lock(&self.reader);
        let mut row = store.get(Table::Items, &store.root_item_id().ok()??).ok()??;
        for name in rel.components() {
            row = store.child(Table::Items, &row.id, name.as_os_str().to_str()?).ok()??;
            if row.placement != Placement::Placed {
                return None;
            }
        }
        Some(row)
    }

    /// The files at and below `row`, each with its place in the folder.
    pub(super) fn files_below(&self, row: &Row) -> Vec<(Row, PathBuf)> {
        let store = crate::panic::lock(&self.reader);
        let mut ids = store.descendants(Table::Items, &row.id).unwrap_or_default();
        ids.push(row.id.clone());
        let file = |id: &String| {
            let row = store.get(Table::Items, id).ok().flatten().filter(|r| r.kind == Kind::File)?;
            let at = store.locate(Table::Items, id).ok().flatten().filter(|at| at.placed)?;
            Some((row, at.rel))
        };
        ids.iter().filter_map(file).collect()
    }
}
