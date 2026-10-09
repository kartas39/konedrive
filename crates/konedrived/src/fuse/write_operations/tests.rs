//! The operations on a folder without a mount: the view's drive in a store on disk, which
//! the recorder writes through the store's one writer and the view reads through a
//! connection of its own, over a backing directory that holds nothing. The operations are
//! called on the test's thread, as the recorder's thread would.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use konedrive_tree::outbox::{Detection, OutboxKind, OutboxRow};
use konedrive_tree::{Store, TreeStore};

use super::WriteOperations;
use crate::fuse::node_table::NodeTable;
use crate::fuse::outbox_recorder::OutboxRecorder;
use crate::fuse::node_lookup::NodeLookup;
use crate::fuse::backing_directory::BackingDirectory;
use crate::fuse::upload_rules::tests::rules;
use crate::fuse::directory_contents::tests::drive;
use crate::fuse::directory_contents::{Directory, Entry, DirectoryContents};
use crate::fuse::open_writers::OpenWriters;

pub(super) struct Folder {
    _tmp: tempfile::TempDir,
    /// The backing directory, by its own path.
    pub(super) disk: PathBuf,
    pub(super) store: Store,
    pub(super) directory_contents: Arc<DirectoryContents>,
    pub(super) backing_directory: Arc<BackingDirectory>,
    pub(super) node_lookup: Arc<NodeLookup>,
    pub(super) open_writers: Arc<OpenWriters>,
    pub(super) writable: Arc<AtomicBool>,
    pub(super) ops: WriteOperations,
}

/// The view's drive (`docs` with `c.txt`; `a.txt` and `b.txt` in the root), with
/// `detections` waiting in the outbox, in a folder that takes changes and ignores `*.tmp`
/// and `cache`.
pub(super) fn folder(detections: Vec<Detection>) -> Folder {
    let tmp = tempfile::tempdir().unwrap();
    let (disk, path) = (tmp.path().join("folder"), tmp.path().join("tree.db"));
    fs::create_dir(&disk).unwrap();
    let mut written = TreeStore::open(&path).unwrap();
    drive(&mut written, detections);
    let store = Store::new(written);
    let backing_directory = Arc::new(BackingDirectory::open(&disk).unwrap());
    let directory_contents = Arc::new(DirectoryContents::new(TreeStore::open_read_only(&path).unwrap(), Arc::clone(&backing_directory)));
    let node_table = Arc::new(Mutex::new(NodeTable { next: 1, ..NodeTable::default() }));
    let node_lookup = Arc::new(NodeLookup::new(Arc::clone(&directory_contents), node_table));
    let writable = Arc::new(AtomicBool::new(true));
    let outbox_recorder = Arc::new(OutboxRecorder::start(store.clone(), Arc::default(), Arc::clone(&writable)).unwrap());
    let open_writers = Arc::new(OpenWriters::default());
    let ops = WriteOperations::new(Arc::clone(&directory_contents), Arc::clone(&backing_directory), Arc::clone(&node_lookup), Arc::clone(&open_writers), outbox_recorder, Arc::new(rules()));
    Folder { _tmp: tmp, disk, store, directory_contents, backing_directory, node_lookup, open_writers, writable, ops }
}

impl Folder {
    /// The outbox's rows, oldest first.
    pub(super) fn rows(&self) -> Vec<OutboxRow> {
        self.store.call_blocking(|s| s.outbox_rows()).unwrap()
    }

    /// Each row's kind, item and place.
    pub(super) fn said(&self) -> Vec<(OutboxKind, Option<String>, PathBuf)> {
        self.rows().into_iter().map(|row| (row.kind, row.item_id, row.rel)).collect()
    }

    /// The node of item `id`, which stands in the directory node `parent`.
    pub(super) fn item(&self, id: &str, parent: u64) -> u64 {
        self.node_lookup.assign_node_id(&Entry::Item(self.directory_contents.row(id).unwrap()), parent)
    }

    /// The node of `docs`.
    pub(super) fn docs(&self) -> u64 {
        self.item("D", 1)
    }

    /// The node of what stands as `name` in the directory node `parent`.
    pub(super) fn node(&self, parent: u64, name: &str) -> u64 {
        let dir = self.node_lookup.find_directory(parent).unwrap();
        self.node_lookup.assign_node_id(&self.directory_contents.find_entry(&dir, name).unwrap(), parent)
    }

    /// What stands at `rel`: an item's id, or `None` for a local object; nothing at all as
    /// the outer `None`.
    pub(super) fn shown(&self, rel: &str) -> Option<Option<String>> {
        self.directory_contents.find_entry_at(Path::new(rel)).map(|entry| entry.id().map(str::to_owned))
    }

    /// Item `id` downloaded at `rel`, with `bytes`.
    pub(super) fn download(&self, rel: &str, id: &str, bytes: &[u8]) {
        let rel = Path::new(rel);
        self.directory_contents.make_marked_directories(rel.parent().unwrap()).unwrap();
        fs::write(self.disk.join(rel), bytes).unwrap();
        self.backing_directory.mark_downloaded_copy(rel, id).unwrap();
    }

    pub(super) fn root(&self) -> Directory {
        self.directory_contents.root_directory()
    }
}

pub(super) fn at(rel: &str) -> PathBuf {
    PathBuf::from(rel)
}
