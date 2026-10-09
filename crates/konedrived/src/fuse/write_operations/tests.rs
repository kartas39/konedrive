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
use crate::fuse::nodes::Nodes;
use crate::fuse::recorder::Recorder;
use crate::fuse::resolve::Resolve;
use crate::fuse::under::Under;
use crate::fuse::upload_rules::tests::rules;
use crate::fuse::view::tests::drive;
use crate::fuse::view::{Dir, Entry, View};
use crate::fuse::writers::Writers;

pub(super) struct Folder {
    _tmp: tempfile::TempDir,
    /// The backing directory, by its own path.
    pub(super) disk: PathBuf,
    pub(super) store: Store,
    pub(super) view: Arc<View>,
    pub(super) under: Arc<Under>,
    pub(super) resolve: Arc<Resolve>,
    pub(super) writers: Arc<Writers>,
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
    let under = Arc::new(Under::open(&disk).unwrap());
    let view = Arc::new(View::new(TreeStore::open_read_only(&path).unwrap(), Arc::clone(&under)));
    let nodes = Arc::new(Mutex::new(Nodes { next: 1, ..Nodes::default() }));
    let resolve = Arc::new(Resolve::new(Arc::clone(&view), nodes));
    let writable = Arc::new(AtomicBool::new(true));
    let recorder = Arc::new(Recorder::start(store.clone(), Arc::default(), Arc::clone(&writable)).unwrap());
    let writers = Arc::new(Writers::default());
    let ops = WriteOperations::new(Arc::clone(&view), Arc::clone(&under), Arc::clone(&resolve), Arc::clone(&writers), recorder, Arc::new(rules()));
    Folder { _tmp: tmp, disk, store, view, under, resolve, writers, writable, ops }
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
        self.resolve.ino_of(&Entry::Item(self.view.row(id).unwrap()), parent)
    }

    /// The node of `docs`.
    pub(super) fn docs(&self) -> u64 {
        self.item("D", 1)
    }

    /// The node of what stands as `name` in the directory node `parent`.
    pub(super) fn node(&self, parent: u64, name: &str) -> u64 {
        let dir = self.resolve.dir(parent).unwrap();
        self.resolve.ino_of(&self.view.entry(&dir, name).unwrap(), parent)
    }

    /// What stands at `rel`: an item's id, or `None` for a local object; nothing at all as
    /// the outer `None`.
    pub(super) fn shown(&self, rel: &str) -> Option<Option<String>> {
        self.view.resolve(Path::new(rel)).map(|entry| entry.id().map(str::to_owned))
    }

    /// Item `id` downloaded at `rel`, with `bytes`.
    pub(super) fn download(&self, rel: &str, id: &str, bytes: &[u8]) {
        let rel = Path::new(rel);
        self.view.backing(rel.parent().unwrap()).unwrap();
        fs::write(self.disk.join(rel), bytes).unwrap();
        self.under.mark_copy(rel, id).unwrap();
    }

    pub(super) fn root(&self) -> Dir {
        self.view.root_dir()
    }
}

pub(super) fn at(rel: &str) -> PathBuf {
    PathBuf::from(rel)
}
