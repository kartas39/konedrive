//! The operations on a folder without a mount: the drive of the directory contents' tests in
//! a store on disk, which the recorder writes through the store's one writer and the
//! directory contents read through a connection of their own, over a backing directory that
//! holds nothing. The operations are called on the test's thread, as the recorder's thread
//! would.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use konedrive_tree::outbox::{Detection, OutboxKind, OutboxRow};
use konedrive_tree::{Store, TreeStore};

use super::WriteOperations;
use crate::fuse::access_mode::AccessMode;
use crate::fuse::backing_directory::BackingDirectory;
use crate::fuse::directory_contents::tests::drive;
use crate::fuse::directory_contents::{DirectoryContents, EntryState, FileSyncState};
use crate::fuse::node_lookup::NodeLookup;
use crate::fuse::node_table::NodeTable;
use crate::fuse::open_writers::OpenWriters;
use crate::fuse::outbox_recorder::OutboxRecorder;
use crate::fuse::upload_rules::tests::rules;

pub(super) struct Folder {
    _tmp: tempfile::TempDir,
    /// The backing directory, by its own path.
    pub(super) disk: PathBuf,
    pub(super) store: Store,
    pub(super) directory_contents: Arc<DirectoryContents>,
    pub(super) backing_directory: Arc<BackingDirectory>,
    pub(super) node_lookup: Arc<NodeLookup>,
    pub(super) open_writers: Arc<OpenWriters>,
    pub(super) access_mode: Arc<AccessMode>,
    pub(super) ops: WriteOperations,
}

/// The drive of the directory contents' tests (`docs` with `c.txt`; `a.txt` and `b.txt` in
/// the root), with `detections` waiting in the outbox, in a folder that takes changes and
/// ignores `*.tmp` and `cache`.
pub(super) fn folder(detections: Vec<Detection>) -> Folder {
    let tmp = tempfile::tempdir().unwrap();
    let (disk, path) = (tmp.path().join("folder"), tmp.path().join("tree.db"));
    fs::create_dir(&disk).unwrap();
    let mut written = TreeStore::open(&path).unwrap();
    drive(&mut written, detections);
    let store = Store::new(written);
    let backing_directory = Arc::new(BackingDirectory::open(&disk).unwrap());
    let directory_contents = Arc::new(DirectoryContents::new(TreeStore::open_read_only(&path).unwrap(), Arc::clone(&backing_directory)));
    let node_lookup = Arc::new(NodeLookup::new(Arc::clone(&directory_contents), Arc::new(Mutex::new(NodeTable::new()))));
    let access_mode = Arc::new(AccessMode::read_write());
    let outbox_recorder = Arc::new(OutboxRecorder::start(store.clone(), Arc::default(), Arc::clone(&access_mode)).unwrap());
    let open_writers = Arc::new(OpenWriters::default());
    let ops = WriteOperations::new(Arc::clone(&directory_contents), Arc::clone(&backing_directory), Arc::clone(&node_lookup), Arc::clone(&open_writers), outbox_recorder, Arc::new(rules()), Arc::clone(&access_mode));
    Folder { _tmp: tmp, disk, store, directory_contents, backing_directory, node_lookup, open_writers, access_mode, ops }
}

impl Folder {
    /// The outbox's rows, oldest first.
    pub(super) fn rows(&self) -> Vec<OutboxRow> {
        self.store.call_blocking(|s| s.outbox_rows()).unwrap()
    }

    /// Each row's kind, OneDrive id and path.
    pub(super) fn said(&self) -> Vec<(OutboxKind, Option<String>, PathBuf)> {
        self.rows().into_iter().map(|row| (row.kind, row.item_id, row.rel)).collect()
    }

    /// The node id of what is `name` in the directory node `parent`.
    pub(super) fn node_id(&self, parent: u64, name: &str) -> u64 {
        let directory = self.node_lookup.find_directory(parent).unwrap();
        self.node_lookup.assign_node_id(&self.directory_contents.find_entry(&directory, name).unwrap(), parent)
    }

    /// The node id of `docs`.
    pub(super) fn docs(&self) -> u64 {
        self.node_id(1, "docs")
    }

    /// What is at `rel`: a OneDrive id, or `None` for a local-only entry; nothing at all as
    /// the outer `None`.
    pub(super) fn shown(&self, rel: &str) -> Option<Option<String>> {
        self.directory_contents.find_entry_at(Path::new(rel)).map(|entry| entry.onedrive_id().map(str::to_owned))
    }

    /// The sync state of the file at `rel`.
    pub(super) fn file_state(&self, rel: &str) -> FileSyncState {
        match self.directory_contents.find_entry_at(Path::new(rel)).unwrap().state {
            EntryState::File(file) => file,
            EntryState::Directory(directory) => panic!("{rel} is a directory: {directory:?}"),
        }
    }

    /// The OneDrive file `id` downloaded at `rel`, with `bytes`.
    pub(super) fn download(&self, rel: &str, id: &str, bytes: &[u8]) {
        crate::fuse::directory_contents::tests::download(&self.disk, &self.directory_contents, rel, id, bytes);
    }
}

pub(super) fn at(rel: &str) -> PathBuf {
    PathBuf::from(rel)
}
