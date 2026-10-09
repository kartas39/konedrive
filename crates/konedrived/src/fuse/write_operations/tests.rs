//! The operations on a folder without a mount: the drive of the directory contents' tests in
//! a store on disk, which the recorder writes through the store's one writer and the
//! directory contents read through a connection of their own, over a backing directory that
//! holds nothing. The operations are called on the test's thread, as the recorder's thread
//! would.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_tree::outbox::{Detection, OutboxKind, OutboxRow};
use konedrive_tree::{Store, TreeStore};

use super::WriteOperations;
use crate::fuse::node_table::NodeId;
use crate::fuse::access_mode_switch::{AccessMode, AccessModeSwitch};
use crate::fuse::backing_directory::BackingDirectory;
use crate::fuse::directory_contents::tests::drive;
use crate::fuse::directory_contents::{DirectoryContents, EntryState, FileSyncState};
use crate::fuse::node_resolver::NodeResolver;
use crate::fuse::open_handles::OpenHandles;
use crate::fuse::outbox_recorder::OutboxRecorder;
use crate::fuse::upload_rules::tests::rules;

pub(super) struct Folder {
    _tmp: tempfile::TempDir,
    /// The backing directory, by its own path.
    pub(super) disk: PathBuf,
    pub(super) store: Store,
    pub(super) directory_contents: Arc<DirectoryContents>,
    pub(super) backing_directory: Arc<BackingDirectory>,
    pub(super) node_resolver: Arc<NodeResolver>,
    pub(super) open_handles: Arc<OpenHandles>,
    pub(super) access_mode_switch: Arc<AccessModeSwitch>,
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
    let node_resolver = Arc::new(NodeResolver::new(Arc::clone(&directory_contents)));
    let access_mode_switch = Arc::new(AccessModeSwitch::new(AccessMode::ReadWrite));
    let outbox_recorder = Arc::new(OutboxRecorder::start(store.clone(), Arc::default()).unwrap());
    let open_handles = Arc::new(OpenHandles::default());
    let ops = WriteOperations::new(Arc::clone(&directory_contents), Arc::clone(&backing_directory), Arc::clone(&node_resolver), Arc::clone(&open_handles), outbox_recorder, Arc::new(rules()), Arc::clone(&access_mode_switch));
    Folder { _tmp: tmp, disk, store, directory_contents, backing_directory, node_resolver, open_handles, access_mode_switch, ops }
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
    pub(super) fn node_id(&self, parent: NodeId, name: &str) -> NodeId {
        let directory = self.node_resolver.find_directory(parent).unwrap();
        self.node_resolver.assign_node_id(&self.directory_contents.find_entry(&directory, name).unwrap(), parent)
    }

    /// The node id of `docs`.
    pub(super) fn docs(&self) -> NodeId {
        self.node_id(NodeId::ROOT, "docs")
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
