//! The operations on a folder without a mount: the drive of the directory contents' tests in
//! a store on disk, which the recorder writes through the store's one writer and the
//! directory contents read through a connection of their own, over a backing directory that
//! holds nothing. The operations are called on the test's thread, as the write-operation
//! thread would.

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
use crate::fuse::unrecorded_entry_scan::UnrecordedEntryScan;
use crate::fuse::upload_rules::tests::rules;
use crate::fuse::upload_rules::UploadRules;

pub(in crate::fuse) struct Folder {
    _tmp: tempfile::TempDir,
    /// The backing directory, by its own path.
    pub(in crate::fuse) disk: PathBuf,
    pub(in crate::fuse) store: Store,
    pub(in crate::fuse) directory_contents: Arc<DirectoryContents>,
    pub(in crate::fuse) backing_directory: Arc<BackingDirectory>,
    pub(in crate::fuse) node_resolver: Arc<NodeResolver>,
    pub(in crate::fuse) open_handles: Arc<OpenHandles>,
    pub(in crate::fuse) access_mode_switch: Arc<AccessModeSwitch>,
    pub(in crate::fuse) outbox_recorder: Arc<OutboxRecorder>,
    pub(in crate::fuse) upload_rules: Arc<UploadRules>,
    pub(in crate::fuse) ops: WriteOperations,
}

/// The drive of the directory contents' tests (`docs` with `c.txt`; `a.txt` and `b.txt` in
/// the root), with `detections` waiting in the outbox, in a folder that takes changes and
/// ignores `*.tmp` and `cache`.
pub(in crate::fuse) fn folder(detections: Vec<Detection>) -> Folder {
    folder_recording_into(detections, |written, _| Store::new(written))
}

/// The [`folder`] whose every record fails: its recorder writes through a connection to the
/// store that is opened for reading alone.
pub(in crate::fuse) fn folder_that_records_nothing() -> Folder {
    folder_recording_into(Vec::new(), |_, path| Store::new(TreeStore::open_read_only(path).unwrap()))
}

/// The [`folder`], with the store its recorder writes into made by `open_store` from the
/// store the drive was written into and that store's path.
fn folder_recording_into(detections: Vec<Detection>, open_store: impl FnOnce(TreeStore, &Path) -> Store) -> Folder {
    let tmp = tempfile::tempdir().unwrap();
    let (disk, path) = (tmp.path().join("folder"), tmp.path().join("tree.db"));
    fs::create_dir(&disk).unwrap();
    let mut written = TreeStore::open(&path).unwrap();
    drive(&mut written, detections);
    let store = open_store(written, &path);
    let backing_directory = Arc::new(BackingDirectory::open(&disk).unwrap());
    let directory_contents = Arc::new(DirectoryContents::new(TreeStore::open_read_only(&path).unwrap(), Arc::clone(&backing_directory)));
    let node_resolver = Arc::new(NodeResolver::new(Arc::clone(&directory_contents), Arc::clone(&backing_directory)));
    let access_mode_switch = Arc::new(AccessModeSwitch::new(AccessMode::ReadWrite));
    let outbox_recorder = Arc::new(OutboxRecorder::new(store.clone()));
    let upload_rules = Arc::new(rules());
    let open_handles = Arc::new(OpenHandles::default());
    let ops = WriteOperations::new(Arc::clone(&directory_contents), Arc::clone(&backing_directory), Arc::clone(&node_resolver), Arc::clone(&open_handles), Arc::clone(&outbox_recorder), Arc::clone(&upload_rules), Arc::clone(&access_mode_switch));
    Folder { _tmp: tmp, disk, store, directory_contents, backing_directory, node_resolver, open_handles, access_mode_switch, outbox_recorder, upload_rules, ops }
}

impl Folder {
    /// The outbox's rows, oldest first.
    pub(in crate::fuse) fn rows(&self) -> Vec<OutboxRow> {
        self.store.call_blocking(|s| s.outbox_rows()).unwrap()
    }

    /// Each row's kind, OneDrive id and path.
    pub(in crate::fuse) fn said(&self) -> Vec<(OutboxKind, Option<String>, PathBuf)> {
        self.rows().into_iter().map(|row| (row.kind, row.item_id, row.rel)).collect()
    }

    /// The node id of what is `name` in the directory node `parent`.
    pub(in crate::fuse) fn node_id(&self, parent: NodeId, name: &str) -> NodeId {
        let directory = self.node_resolver.find_directory(parent).unwrap();
        self.node_resolver.assign_node_id(&self.directory_contents.find_entry(&directory, name).unwrap(), parent)
    }

    /// The node id of `docs`.
    pub(in crate::fuse) fn docs(&self) -> NodeId {
        self.node_id(NodeId::ROOT, "docs")
    }

    /// What is at `rel`: a OneDrive id, or `None` for a local-only entry; nothing at all as
    /// the outer `None`.
    pub(in crate::fuse) fn shown(&self, rel: &str) -> Option<Option<String>> {
        self.directory_contents.find_entry_at(Path::new(rel)).map(|entry| entry.onedrive_id().map(str::to_owned))
    }

    /// The sync state of the file at `rel`.
    pub(in crate::fuse) fn file_state(&self, rel: &str) -> FileSyncState {
        match self.directory_contents.find_entry_at(Path::new(rel)).unwrap().state {
            EntryState::File(file) => file,
            EntryState::Directory(directory) => panic!("{rel} is a directory: {directory:?}"),
        }
    }

    /// The scan for unrecorded entries, over the folder's parts.
    pub(in crate::fuse) fn unrecorded_entry_scan(&self) -> UnrecordedEntryScan {
        UnrecordedEntryScan::new(Arc::clone(&self.directory_contents), Arc::clone(&self.backing_directory), Arc::clone(&self.upload_rules), Arc::clone(&self.outbox_recorder), Arc::clone(&self.access_mode_switch))
    }

    /// The OneDrive file `id` downloaded at `rel`, with `bytes`.
    pub(in crate::fuse) fn download(&self, rel: &str, id: &str, bytes: &[u8]) {
        crate::fuse::directory_contents::tests::download(&self.disk, &self.directory_contents, rel, id, bytes);
    }
}

pub(in crate::fuse) fn at(rel: &str) -> PathBuf {
    PathBuf::from(rel)
}
