//! The one fixture of `local/`'s tests, and of the other areas' tests that run an
//! examination: [`Folder`], a folder in a temporary directory with its store, the listing
//! placed in it by the real materializer, and [`FakeLiveness`] for "is this object alive?".
//!
//! A test makes one of four folders: listed and committed ([`Folder::new`]), the same with
//! its store on disk ([`Folder::on_disk`]), listed and placed but not committed yet
//! ([`Folder::unfinished`], until [`Folder::finish`]), or with nothing listed at all
//! ([`Folder::unlisted`]). It examines batches of its own ([`Folder::examine`]) or runs the
//! real watcher on the folder ([`Folder::watched`], with [`Folder::with_helper`] for the
//! daemon's fake helper), and reads the results: [`Folder::rows`], [`Folder::summary`],
//! [`Folder::skipped`], [`Folder::activity`].

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{self, State, XATTR_ROOT};
use konedrive_tree::outbox::{OutboxKind, OutboxRow};
use konedrive_tree::{Change, Kind, Placement, Row, Store, TreeStore};

use super::liveness::{Liveness, NoLiveness, Whereabouts};
use super::watcher::service::ExamineSink;
use super::watcher::{Handled, Sink, Timing, WatchConfig, Watcher};
use super::{Batch, ExamineError, Examined, Examiner, IgnoreList, ScanProgress};
use crate::folder::disk::Disk;
use crate::folder::locks::InodeLocks;
use crate::folder::root::SyncRoot;
use crate::helper::testing::FakeHelper;
use crate::helper::LinkCell;
use crate::remote::materialize::Scope;

/// The `mtime` of every listed item.
pub(crate) const TIME: i64 = 1_700_000_000;

/// The longest a test waits for the watcher.
pub(crate) const WAIT: Duration = Duration::from_secs(10);

/// An item as OneDrive lists it: a file holds `content`.
pub(crate) fn row(id: &str, parent: Option<&str>, name: &str, kind: Kind, content: &[u8]) -> Row {
    let mut hasher = konedrive_graph::quickxor::QuickXor::new();
    hasher.update(content);
    Row {
        id: id.into(),
        parent_id: parent.map(str::to_owned),
        name: name.into(),
        kind,
        size: if kind == Kind::File { content.len() as u64 } else { 0 },
        mtime: TIME,
        etag: Some(format!("e-{id}")),
        ctag: Some(format!("c-{id}")),
        quickxor: (kind == Kind::File).then(|| hasher.finish_base64()),
        mime: None,
        placement: Placement::Placed,
    }
}

pub(crate) fn folder(id: &str, parent: &str, name: &str) -> Change {
    Change::Upsert(row(id, Some(parent), name, Kind::Folder, b""))
}

pub(crate) fn file(id: &str, parent: &str, name: &str, content: &[u8]) -> Change {
    Change::Upsert(row(id, Some(parent), name, Kind::File, content))
}

/// A batch naming these (directory, name) pairs.
pub(crate) fn names(pairs: &[(&str, &str)]) -> Batch {
    let mut batch = Batch::new();
    for (dir, name) in pairs {
        batch.name(Path::new(dir), OsStr::new(name));
    }
    batch
}

/// One examination of `batch`, at `now`.
pub(crate) fn examine(disk: &Disk, store: &Store, liveness: &dyn Liveness, ignore: &IgnoreList, locks: &InodeLocks, now: i64, batch: &Batch) -> Result<Examined, ExamineError> {
    Examiner { disk, store, liveness, ignore, locks, now }.examine(batch)
}

/// A folder in a temporary directory, its store, and what an examination of it needs. The
/// folder is `OneDrive`; [`outside`](Self::outside) is a directory beside it, on its
/// filesystem. The drive's root is item `R`.
pub(crate) struct Folder {
    pub dir: tempfile::TempDir,
    pub root: SyncRoot,
    pub outside: PathBuf,
    pub store: Store,
    pub liveness: FakeLiveness,
    pub ignore: IgnoreList,
    pub locks: InodeLocks,
    /// For the watcher and the daemon's sink, and for what a test runs beside them.
    pub runtime: tokio::runtime::Runtime,
}

impl Folder {
    /// A folder whose listing is `changes` (the root comes first by itself), placed and
    /// committed.
    pub(crate) fn new(changes: &[Change]) -> Self {
        let folder = Self::unfinished(changes);
        folder.finish();
        folder
    }

    /// [`new`](Self::new), with its store in a file beside the folder, in WAL mode as the
    /// daemon keeps it.
    pub(crate) fn on_disk(changes: &[Change]) -> Self {
        let folder = Self::bare(|dir| TreeStore::open(&dir.join("tree.sqlite")).unwrap());
        folder.place(changes);
        folder.finish();
        folder
    }

    /// A folder whose first listing, `changes`, is placed in it and not committed: there is
    /// no base to examine against until [`finish`](Self::finish).
    pub(crate) fn unfinished(changes: &[Change]) -> Self {
        let folder = Self::unlisted();
        folder.place(changes);
        folder
    }

    /// A folder nothing was listed in: what the watcher's tests watch.
    pub(crate) fn unlisted() -> Self {
        Self::bare(|_| TreeStore::in_memory().unwrap())
    }

    fn bare(store: impl FnOnce(&Path) -> TreeStore) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let path = base.join("OneDrive");
        let outside = base.join("elsewhere");
        std::fs::create_dir(&path).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let root_id = "5b0e2c7a-1d3f-4e8a-9b6c-0f1e2d3c4b5a".to_owned();
        xattr::set(&path, XATTR_ROOT, root_id.as_bytes()).unwrap();
        Folder {
            store: Store::new(store(&base)),
            dir,
            root: SyncRoot { path, root_id },
            outside,
            liveness: FakeLiveness::new(),
            ignore: IgnoreList::default(),
            locks: InodeLocks::new(),
            runtime: tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap(),
        }
    }

    /// Stages the root and `changes` as a whole listing and makes the folder match it.
    fn place(&self, changes: &[Change]) {
        let mut all = vec![Change::Root(row("R", None, "", Kind::Folder, b""))];
        all.extend_from_slice(changes);
        self.store
            .call_blocking(move |s| {
                s.begin_staging(konedrive_tree::NewTree::Whole)?;
                s.stage(&all)
            })
            .unwrap();
        let rescue_into = self.outside.join("rescued");
        crate::remote::testing::materializer(self.disk(), &self.store, "R", rescue_into, None, self.runtime.handle()).apply(Scope::Full).unwrap();
    }

    /// The listing completes: what was placed is the base.
    pub(crate) fn finish(&self) {
        self.store.call_blocking(move |s| s.commit_staging("link-1")).unwrap();
    }

    pub(crate) fn path(&self, rel: &str) -> PathBuf {
        self.root.path.join(rel)
    }

    pub(crate) fn disk(&self) -> Disk {
        Disk::open(&self.root, false).unwrap()
    }

    // ---- Examinations ----

    pub(crate) fn examine(&self, batch: &Batch) -> Examined {
        self.examine_with(batch, &self.liveness)
    }

    /// [`examine`](Self::examine), at the time `now`.
    pub(crate) fn examine_at(&self, batch: &Batch, now: i64) -> Examined {
        examine(&self.disk(), &self.store, &self.liveness, &self.ignore, &self.locks, now, batch).unwrap()
    }

    pub(crate) fn examine_with(&self, batch: &Batch, liveness: &dyn Liveness) -> Examined {
        self.try_examine(&self.disk(), batch, liveness).unwrap()
    }

    pub(crate) fn try_examine(&self, disk: &Disk, batch: &Batch, liveness: &dyn Liveness) -> Result<Examined, ExamineError> {
        examine(disk, &self.store, liveness, &self.ignore, &self.locks, 1000, batch)
    }

    /// A Full local scan that tells `progress` how it goes.
    pub(crate) fn scan_reporting(&self, progress: &dyn ScanProgress) -> Result<Examined, ExamineError> {
        let disk = self.disk();
        let examiner = Examiner { disk: &disk, store: &self.store, liveness: &self.liveness, ignore: &self.ignore, locks: &self.locks, now: 1000 };
        examiner.examine_reporting(&Batch::full(), Some(progress))
    }

    // ---- What an examination left ----

    pub(crate) fn rows(&self) -> Vec<OutboxRow> {
        self.store.call_blocking(move |s| s.outbox_rows()).unwrap()
    }

    /// (kind, where, item) of every row, in `seq` order.
    pub(crate) fn summary(&self) -> Vec<(OutboxKind, String, Option<String>)> {
        self.rows().into_iter().map(|r| (r.kind, r.rel.display().to_string(), r.item_id)).collect()
    }

    pub(crate) fn row_at(&self, rel: &str) -> OutboxRow {
        self.rows().into_iter().find(|r| r.rel == Path::new(rel)).unwrap_or_else(|| panic!("no row at {rel}: {:?}", self.summary()))
    }

    /// The "Not uploaded" list, as (where, why).
    pub(crate) fn skipped(&self) -> Vec<(String, String)> {
        let skipped = self.store.call_blocking(move |s| s.local_skipped()).unwrap();
        skipped.into_iter().map(|s| (s.rel.display().to_string(), s.reason.to_string())).collect()
    }

    /// What Activity holds, newest first.
    pub(crate) fn activity(&self) -> Vec<konedrive_tree::ActivityRow> {
        self.store.call_blocking(move |s| s.recent_activity(1000)).unwrap()
    }

    // ---- What the user does in the folder ----

    /// Downloaded, as a fill leaves it: the content, `hydrated`, a stamp.
    pub(crate) fn hydrate(&self, rel: &str, content: &[u8]) {
        let file = File::options().write(true).open(self.path(rel)).unwrap();
        (&file).write_all(content).unwrap();
        placeholder::write_state(&file, State::Hydrated).unwrap();
        placeholder::write_stamp(&file).unwrap();
    }

    /// The handle of `rel`, by name.
    pub(crate) fn handle(&self, rel: &str) -> FileHandle {
        let path = self.path(rel);
        FileHandle::at(&File::open(path.parent().unwrap()).unwrap(), path.file_name().unwrap()).unwrap()
    }

    pub(crate) fn ino(&self, rel: &str) -> u64 {
        std::fs::metadata(self.path(rel)).unwrap().ino()
    }

    pub(crate) fn write(&self, rel: &str, content: &[u8]) {
        std::fs::write(self.path(rel), content).unwrap();
    }

    pub(crate) fn rename(&self, from: &str, to: &str) {
        std::fs::rename(self.path(from), self.path(to)).unwrap();
    }

    /// `script`, run by `sh` in the folder: another process, whose events
    /// carry no pid of ours.
    pub(crate) fn shell(&self, script: &str) {
        let status = Command::new("sh").arg("-c").arg(script).current_dir(&self.root.path).status().unwrap();
        assert!(status.success(), "{script}");
    }

    // ---- The watcher on the folder ----

    /// The watcher's configuration for this folder: no helper, the tests' clocks, and
    /// nothing dropped by pid, so the test process itself can stand in for the user.
    pub(crate) fn config(&self) -> WatchConfig {
        let mut config = WatchConfig::new(self.root.clone(), LinkCell::default(), self.runtime.handle().clone());
        config.own_pid = None;
        config.timing = timing();
        config
    }

    /// `config` with a link to a helper of its own, which marks whatever it
    /// is asked to until the test tells it otherwise.
    pub(crate) fn with_helper(&self, mut config: WatchConfig) -> (WatchConfig, FakeHelper) {
        let helper = FakeHelper::standalone();
        config.link = LinkCell::holding(Some(self.runtime.block_on(helper.connect())));
        (config, helper)
    }

    /// The real watcher on the folder, with a real notification group. Its batches come
    /// out of the receiver, the bring-up's Full local scan already taken off it.
    pub(crate) fn watched(&self, config: WatchConfig) -> (Watcher, mpsc::Receiver<Batch>) {
        let (tx, rx) = mpsc::channel();
        let watcher = Watcher::start(config, Box::new(Recorder(tx))).unwrap();
        let first = next(&rx);
        assert!(first.is_full(), "the bring-up hands over a Full local scan: {first:?}");
        (watcher, rx)
    }

    /// The daemon's sink over the folder and its store, with nobody to ask
    /// where an object went.
    pub(crate) fn sink(&self) -> ExamineSink {
        ExamineSink {
            root: self.root.clone(),
            store: self.store.clone(),
            locks: self.locks.clone(),
            ignore: IgnoreList::default().shared(),
            liveness: Box::new(NoLiveness),
            link: LinkCell::default(),
            runtime: self.runtime.handle().clone(),
            on_rows: None,
            on_handles: None,
            tree_lock: None,
            scan: None,
        }
    }

    /// Makes the directory `name` in the folder and returns once the watcher's reader is
    /// held in its `MarkDir`: it reads nothing until the returned sender is told
    /// to let the helper answer.
    pub(crate) fn reader_held_at(&self, helper: &FakeHelper, name: &str) -> mpsc::Sender<()> {
        let (reached, release) = helper.stall_on(&format!("/{name}"));
        std::fs::create_dir(self.path(name)).unwrap();
        reached.recv_timeout(WAIT).expect("the reader asks the helper to mark the new directory");
        release
    }
}

/// The watcher's clocks in a test.
fn timing() -> Timing {
    Timing {
        quiet: Duration::from_millis(200),
        ceiling: Duration::from_secs(3),
        recheck: Duration::from_millis(300),
        retry: Duration::from_millis(200),
        degraded_scan: Duration::from_millis(300),
        mark_retry: Duration::from_secs(3600),
    }
}

/// The next batch a watcher handed over.
pub(crate) fn next(rx: &mpsc::Receiver<Batch>) -> Batch {
    rx.recv_timeout(WAIT).expect("a batch")
}

/// The next Full local scan, whatever is handed over before it.
pub(crate) fn next_full(rx: &mpsc::Receiver<Batch>) -> Batch {
    loop {
        let batch = next(rx);
        if batch.is_full() {
            return batch;
        }
    }
}

/// Waits until `helper` has been asked to mark every one of `inodes`.
pub(crate) fn marked_all(helper: &FakeHelper, inodes: Vec<u64>) {
    let deadline = Instant::now() + WAIT;
    loop {
        let asked: Vec<u64> = helper.marks().iter().map(|mark| mark.ino).collect();
        let missing: Vec<u64> = inodes.iter().copied().filter(|ino| !asked.contains(ino)).collect();
        if missing.is_empty() {
            return;
        }
        assert!(Instant::now() < deadline, "never marked for interception: {missing:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A sink that sends on every batch it is handed, and examines nothing.
pub(crate) struct Recorder(pub mpsc::Sender<Batch>);

impl Sink for Recorder {
    fn handle(&mut self, batch: &Batch) -> Handled {
        let _ = self.0.send(batch.clone());
        Handled::Done { recheck: Batch::new(), passed: Box::default() }
    }
}

/// A sink that fails a number of batches, and records what comes after.
pub(crate) struct Failing(pub u32, pub mpsc::Sender<Batch>);

impl Sink for Failing {
    fn handle(&mut self, batch: &Batch) -> Handled {
        if self.0 > 0 {
            self.0 -= 1;
            return Handled::Failed("the store is closed (this test's own failure)".into());
        }
        let _ = self.1.send(batch.clone());
        Handled::Done { recheck: Batch::new(), passed: Box::default() }
    }
}

/// A sink that says it was handed a batch, and panics on it.
pub(crate) struct Panicking(pub mpsc::Sender<()>);

impl Sink for Panicking {
    fn handle(&mut self, _batch: &Batch) -> Handled {
        let _ = self.0.send(());
        panic!("the examination panicked (this test's own panic)");
    }
}

/// A table of where objects went, for tests: a handle it knows is alive
/// there, any other is gone. Never for the daemon itself: "gone" by default
/// would delete in OneDrive whatever it was not told about — so it is in a
/// module only test builds have.
#[derive(Debug, Default)]
pub struct FakeLiveness {
    alive: Mutex<HashMap<FileHandle, PathBuf>>,
    asked: Mutex<Vec<FileHandle>>,
}

impl FakeLiveness {
    pub fn new() -> Self {
        Self::default()
    }

    /// The object `handle` names now lives at `path`.
    pub fn alive(&self, handle: FileHandle, path: impl Into<PathBuf>) {
        self.alive.lock().unwrap().insert(handle, path.into());
    }

    /// The object at `path` is alive there, and so is everything below it —
    /// as the helper answers for what a moved folder took along.
    pub fn alive_tree(&self, path: &Path) {
        let mut stack = vec![path.to_path_buf()];
        while let Some(p) = stack.pop() {
            let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else { continue };
            if let Ok(handle) = File::open(parent).and_then(|dir| FileHandle::at(&dir, name)) {
                self.alive(handle, p.clone());
            }
            if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
                stack.extend(std::fs::read_dir(&p).into_iter().flatten().flatten().map(|e| e.path()));
            }
        }
    }

    /// Every handle asked about, in order.
    pub fn asked(&self) -> Vec<FileHandle> {
        self.asked.lock().unwrap().clone()
    }
}

impl Liveness for FakeLiveness {
    fn whereabouts(&self, handle: &FileHandle) -> io::Result<Whereabouts> {
        self.asked.lock().unwrap().push(handle.clone());
        Ok(match self.alive.lock().unwrap().get(handle) {
            Some(path) => Whereabouts::At(path.clone()),
            None => Whereabouts::Gone,
        })
    }
}
