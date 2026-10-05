//! The one fixture of `remote/`'s tests: a folder in a temporary directory,
//! its tree store, the fake OneDrive and a fake helper.
//!
//! A test drives it in one of two ways:
//!
//! - [`World::cycle`] runs a real cycle of a [`Listing`] against the fake
//!   OneDrive — changed through [`FakeGraph::with`], or answered by hand
//!   ([`World::feed`], [`World::page`], [`World::held`]) where a test needs a
//!   page, a stop or a failure at an exact point;
//! - [`World::listed_as`], [`World::changed`] and [`World::step`] say what
//!   OneDrive says as tree rows, in place of the cycle's fetch, and hand
//!   them to the cycle's own staging ([`Listing::stage`]) and reconcile
//!   ([`Reconcile::run`]). A [`Step`] runs the reconcile in its two halves,
//!   [`Step::apply`] and [`Step::commit`], for what happens between the
//!   folder and the swap.
//!
//! From the staging on, everything is the daemon's own: the stale-delta
//! guard, what is staged again, the choice between a Changed and a Full
//! reconcile, the reconcile, the commit and what follows it. A step stands
//! for a cycle of a `Listing` of its own, whose turn it holds; a test says
//! whether that cycle was asked for a Full reconcile ([`Says`]).
//!
//! A folder made neither read-write nor locked ([`Options`]) is a read-only
//! one, reconciled under the lock as the daemon does, whose lock the
//! fixture takes off again after every step and cycle, so that a test can
//! put its own files into it.

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::{FileExt as _, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{self, State, XATTR_ITEM_ID, XATTR_ROOT};
use konedrive_graph::drive::DriveClient;
use konedrive_tree::outbox::{Base, Committed, Detection, OutboxKind, OutboxRow, OutboxState, Recorded};
use konedrive_tree::{Change, Row, Store, Table, TreeStore};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, Request, ResponseTemplate};

use crate::fake_onedrive::{FakeGraph, FakeItem, ROOT};
use crate::folder::classify::classify;
use crate::folder::disk::Disk;
use crate::folder::locks::InodeLocks;
use crate::folder::root::SyncRoot;
use crate::helper::testing::FakeHelper;
use crate::helper::HelperLink;
use crate::hydration::graph_source::GraphSource;
use crate::hydration::pin::Pins;
use crate::local::{Batch, Examined, FakeLiveness, IgnoreList};
use crate::remote::listing::reconcile::{Commit, Held, Prepared, Reconcile, Reconciled};
use crate::remote::listing::stage::{News, Staged};
use crate::remote::mode::Mode;
use crate::remote::listing::{CycleError, CycleReport, Lease, Listing, ListingContext, Neighbours, Turn, Writes, FULL_THRESHOLD};
use crate::remote::materialize::{Applied, Claimed, Materializer, Scope};
use crate::status::report::Report;
use crate::status::snapshot::{FolderStatus, SyncSnapshot, SyncStateHandle};
use crate::upload::{Engine, Limits, NoHost, WorkerConfig};

/// The longest a test waits for anything: a regression that would hang it
/// fails it in seconds instead.
pub(crate) const PATIENCE: Duration = Duration::from_secs(10);

/// `work`, or a failure once [`PATIENCE`] is out.
pub(crate) async fn within<T>(work: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(PATIENCE, work).await.expect("waited longer than a test should")
}

/// A cycle of `listing` in the background, stopped by `cancel`.
pub(crate) fn spawn_cycle(listing: &Arc<Listing>, cancel: &CancellationToken) -> tokio::task::JoinHandle<Result<CycleReport, CycleError>> {
    let (listing, cancel) = (Arc::clone(listing), cancel.clone());
    tokio::spawn(async move { listing.cycle(&cancel).await })
}

/// How a [`World`] is made.
#[derive(Default)]
pub(crate) struct Options {
    /// A read-write folder: its cycles hold the tree lock and defer.
    pub writes: bool,
    /// A read-only folder is left under its lock, as the daemon keeps it.
    /// Otherwise the fixture takes the lock off after every step and cycle.
    pub locked: bool,
    /// The preferred rescue directory; a temporary one otherwise.
    pub rescue_dir: Option<PathBuf>,
    /// Where the folder's temporary directory is made; the default place otherwise.
    pub base: Option<PathBuf>,
    /// The item ids another account of the daemon claims; none otherwise.
    pub claimed: Option<Claimed>,
}

pub(crate) struct World {
    pub graph: FakeGraph,
    pub root: SyncRoot,
    pub store: Store,
    pub state: SyncStateHandle,
    /// Where every listing of this world reports, its activity kept in `store`.
    pub report: Report,
    /// What every listing of this world queues for pins, and never downloads.
    pub pins: Arc<Pins>,
    pub helper: FakeHelper,
    /// The link to [`helper`](Self::helper).
    pub link: HelperLink,
    /// The preferred rescue directory (`ListingContext::rescue_dir`).
    pub rescue_dir: PathBuf,
    pub lifecycle: Arc<tokio::sync::RwLock<()>>,
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
    pub locks: InodeLocks,
    pub liveness: Arc<FakeLiveness>,
    /// What each cycle handed the watcher to examine.
    pub examined: Arc<Mutex<Vec<Batch>>>,
    /// Cycles that went through, as the outbox worker hears of them.
    pub cycles: Arc<AtomicUsize>,
    /// Rows `Writes::dropped_removed` heard were dropped.
    pub dropped: Arc<Mutex<Vec<OutboxRow>>>,
    writes: bool,
    locked: bool,
    claimed: Option<Claimed>,
    /// The links the reconciles staged by hand committed so far.
    links: AtomicUsize,
    _dir: tempfile::TempDir,
    _rescue: Option<tempfile::TempDir>,
}

/// A locked tree cannot be deleted by the temporary directory's cleanup.
impl Drop for World {
    fn drop(&mut self) {
        if let Ok(disk) = Disk::open(&self.root, false) {
            let _ = disk.unlock_tree();
        }
    }
}

impl World {
    /// A read-only folder, kept locked; the fake OneDrive holds its root only.
    pub(crate) async fn read_only() -> World {
        World::new(Options { locked: true, ..Options::default() }).await
    }

    /// A read-write folder; OneDrive holds `docs/f.txt` ("one") and
    /// `top.txt` ("top").
    pub(crate) async fn read_write() -> World {
        World::read_write_in(None).await
    }

    /// [`read_write`](Self::read_write), with its folder in a temporary directory under `base`.
    pub(crate) async fn read_write_in(base: Option<&Path>) -> World {
        let world = World::new(Options { writes: true, base: base.map(Path::to_path_buf), ..Options::default() }).await;
        world.graph.with(|c| {
            c.add(FakeItem {
                id: "D".into(),
                parent: Some(ROOT.into()),
                name: "docs".into(),
                folder: true,
                content: Vec::new(),
                hash: None,
                size: 0,
                etag: "e-D".into(),
                ctag: "c-D".into(),
                mtime: 0,
            });
            c.add_file("F", "D", "f.txt", b"one");
            c.add_file("T", ROOT, "top.txt", b"top");
        });
        world
    }

    /// The folder is `OneDrive` inside a temporary directory, so that a
    /// rescue directory made beside it is cleaned up with it.
    pub(crate) async fn new(options: Options) -> World {
        let graph = FakeGraph::start().await;
        let dir = match &options.base {
            Some(base) => tempfile::tempdir_in(base).unwrap(),
            None => tempfile::tempdir().unwrap(),
        };
        let folder = dir.path().canonicalize().unwrap().join("OneDrive");
        std::fs::create_dir(&folder).unwrap();
        let root_id = "8f6c0a3e-3b0e-4d7a-9c1e-5b2d7e4f1a90".to_owned();
        xattr::set(&folder, XATTR_ROOT, root_id.as_bytes()).unwrap();
        let store = Store::new(TreeStore::in_memory().unwrap());
        // The folder is what `SyncService` has registered: what its events are about.
        let state = SyncStateHandle::new(SyncSnapshot { folder: FolderStatus { root_path: folder.display().to_string(), ..FolderStatus::default() }, ..SyncSnapshot::default() });
        let report = Report::new(state.clone());
        konedrive_tree::off_runtime(|| report.activity.attach(store.clone(), &folder));
        let pins = Pins::detached(state.clone());
        let helper = FakeHelper::standalone();
        let link = helper.connect().await;
        let (rescue_dir, rescue) = match options.rescue_dir {
            Some(dir) => (dir, None),
            None => {
                let rescue = tempfile::tempdir().unwrap();
                (rescue.path().to_path_buf(), Some(rescue))
            }
        };
        World {
            graph,
            root: SyncRoot { path: folder, root_id },
            store,
            state,
            report,
            pins,
            link,
            helper,
            rescue_dir,
            lifecycle: Arc::new(tokio::sync::RwLock::new(())),
            tree_lock: Arc::new(tokio::sync::Mutex::new(())),
            locks: InodeLocks::new(),
            liveness: Arc::new(FakeLiveness::new()),
            examined: Arc::default(),
            cycles: Arc::default(),
            dropped: Arc::default(),
            writes: options.writes,
            locked: options.locked,
            claimed: options.claimed,
            links: AtomicUsize::new(0),
            _dir: dir,
            _rescue: rescue,
        }
    }

    // ---- The listing and its cycles ----

    /// A read-write folder's part of the context: what its cycles tell is kept here.
    pub(crate) fn writes(&self, scanned: Option<tokio::sync::watch::Receiver<bool>>) -> Writes {
        let (examined, cycles, dropped) = (Arc::clone(&self.examined), Arc::clone(&self.cycles), Arc::clone(&self.dropped));
        Writes {
            tree_lock: Arc::clone(&self.tree_lock),
            machine_name: "fedora".into(),
            ignore: IgnoreList::default().shared(),
            scanned,
            examine: Arc::new(move |batch| examined.lock().unwrap().push(batch)),
            cycled: Arc::new(move || {
                cycles.fetch_add(1, Ordering::SeqCst);
            }),
            reopened: Arc::new(|| {}),
            dropped_removed: Arc::new(move |rows| dropped.lock().unwrap().extend(rows)),
        }
    }

    /// The context of this world's listing; a test changes a part of it with
    /// `ListingContext { part, ..w.context() }`.
    pub(crate) fn context(&self) -> ListingContext {
        let drive = self.drive();
        ListingContext {
            root: self.root.clone(),
            intercepted: true,
            store: self.store.clone(),
            drive: drive.clone(),
            drive_record: None,
            source: Arc::new(GraphSource::new(drive)),
            link: crate::helper::LinkCell::holding(Some(self.link.clone())),
            locks: self.locks.clone(),
            state: self.state.clone(),
            lease: Lease::on(&self.lifecycle),
            rescue_dir: self.rescue_dir.clone(),
            full_threshold: FULL_THRESHOLD,
            after_cycle: None,
            report: self.report.clone(),
            pins: Arc::clone(&self.pins),
            mode: match self.writes {
                true => Mode::ReadWrite(self.writes(None)),
                false => Mode::ReadOnly,
            },
            neighbours: self.claimed.clone().map(|claimed| Neighbours { claimed, drive_seen: Arc::new(|_| {}) }),
            running: Arc::default(),
        }
    }

    pub(crate) fn drive(&self) -> DriveClient {
        self.graph.client()
    }

    pub(crate) fn listing(&self) -> Arc<Listing> {
        Listing::new(self.context())
    }

    /// One cycle that must go through, with the replacements it started.
    pub(crate) async fn cycle(&self, listing: &Arc<Listing>) -> CycleReport {
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        listing.join_replacements().await;
        self.hands_free();
        report
    }

    /// A listing after its first cycle against the fake OneDrive as it is.
    pub(crate) async fn listed(&self) -> Arc<Listing> {
        let listing = self.listing();
        self.cycle(&listing).await;
        listing
    }

    // ---- What OneDrive says, staged by hand, through the real reconcile ----

    /// OneDrive lists `changes`, whole: reconciled in full and committed.
    pub(crate) async fn listed_as(&self, changes: &[Change]) -> Applied {
        self.step(Says::Whole(changes)).await.run().await.unwrap().applied
    }

    /// A delta of `changes` on top of what is committed: reconciled over what
    /// it changes, and committed.
    pub(crate) async fn changed(&self, changes: &[Change]) -> Result<Reconciled, CycleError> {
        self.step(Says::Delta(changes)).await.run().await
    }

    /// [`changed`](Self::changed), as a cycle that was asked for a Full reconcile.
    pub(crate) async fn changed_full(&self, changes: &[Change]) -> Result<Reconciled, CycleError> {
        self.step(Says::DeltaInFull(changes)).await.run().await
    }

    /// What OneDrive `says`, staged as a cycle stages it, and the reconcile
    /// of it ready to run: whole ([`Step::run`]) or in its two halves.
    pub(crate) async fn step(&self, says: Says<'_>) -> Step {
        let listing = self.listing();
        let turn: Turn = Arc::new(Arc::new(tokio::sync::Mutex::new(())).lock_owned().await);
        let cancel = CancellationToken::new();
        let mode = listing.begin(&turn, &cancel).await.unwrap();
        let link = format!("link-{}", self.links.fetch_add(1, Ordering::SeqCst) + 1);
        let (news, full_requested) = match says {
            Says::Whole(changes) => {
                listing.begin_listing(&turn).await.unwrap();
                listing.stage_page(&turn, changes.to_vec()).await.unwrap();
                (News::Listed { link, upload_differences: false }, false)
            }
            Says::Delta(changes) => (News::Changes { changes: changes.to_vec(), link }, false),
            Says::DeltaInFull(changes) => (News::Changes { changes: changes.to_vec(), link }, true),
            Says::Fetched => {
                let stored = self.store.call(|s| s.delta_link()).await.unwrap().expect("a folder listed once");
                (listing.fetch_changes(&turn, stored, &cancel).await.unwrap(), false)
            }
        };
        let unlock = (!self.writes && !self.locked).then(|| self.root.clone());
        let (staged, _) = listing.stage(&turn, mode, news, full_requested, &cancel).await.unwrap();
        // Nothing changed and nothing to look at again: the cycle ends here.
        let Some(Staged { mode, scope, commit }) = staged else { return Step { reconcile: None, _held: None, scope: None, commit: None, passed: None, unlock } };
        let (reconcile, held) = listing.begin_reconcile(&turn, mode, &cancel).await.unwrap();
        Step { reconcile: Some(reconcile), _held: Some(held), scope: Some(scope), commit: Some(commit), passed: None, unlock }
    }

    /// The lock taken off a read-only folder that the test did not ask to
    /// be left locked ([`Options::locked`]).
    fn hands_free(&self) {
        if !self.writes && !self.locked {
            unlock(&self.root);
        }
    }

    // ---- The delta feed answered by hand ----

    pub(crate) fn link_to(&self, token: &str) -> String {
        format!("{}/me/drive/root/delta?token={token}", self.graph.server.uri())
    }

    /// The delta feed from `from` (None: the start) answers `items` and
    /// ends with the link `next`, once.
    pub(crate) async fn feed(&self, from: Option<&str>, items: Value, next: &str) {
        self.feed_after(from, items, next, Duration::ZERO).await;
    }

    /// [`Self::feed`], answering only after `delay`.
    pub(crate) async fn feed_after(&self, from: Option<&str>, items: Value, next: &str, delay: Duration) {
        let body = json!({"value": items, "@odata.deltaLink": self.link_to(next)});
        self.answer(from, ResponseTemplate::new(200).set_body_json(body).set_delay(delay)).await;
    }

    /// Page `from` of the delta feed (None: the first) holds `items`,
    /// and the page after it is at the link `next`, once.
    pub(crate) async fn page(&self, from: Option<&str>, items: Value, next: &str) {
        let body = json!({"value": items, "@odata.nextLink": self.link_to(next)});
        self.answer(from, ResponseTemplate::new(200).set_body_json(body)).await;
    }

    /// The delta request from `from` (None: the start) is answered by
    /// `respond`, once, instead of by the fake OneDrive.
    pub(crate) async fn answer(&self, from: Option<&str>, respond: impl wiremock::Respond + 'static) {
        let mock = Mock::given(method("GET")).and(path("/me/drive/root/delta"));
        let mock = match from {
            Some(token) => mock.and(query_param("token", token)),
            None => mock,
        };
        mock.respond_with(respond).up_to_n_times(1).with_priority(if from.is_some() { 1 } else { 5 }).mount(&self.graph.server).await;
    }

    /// The delta request from `from`, held: the channel says when it is
    /// asked, and the answer — no page at all — comes only after twice
    /// [`PATIENCE`], longer than any test step waits (see [`within`]),
    /// so a test sees the request still open for as long as it looks.
    pub(crate) async fn held(&self, from: Option<&str>) -> tokio::sync::mpsc::UnboundedReceiver<()> {
        let (asked, heard) = tokio::sync::mpsc::unbounded_channel();
        self.answer(from, move |_: &Request| {
            let _ = asked.send(());
            ResponseTemplate::new(200).set_delay(2 * PATIENCE)
        })
        .await;
        heard
    }

    /// The `token` of every delta request so far, in order; `None` for
    /// one from the start.
    pub(crate) async fn delta_tokens(&self) -> Vec<Option<String>> {
        let requests = self.graph.server.received_requests().await.unwrap();
        let deltas = requests.iter().filter(|r| r.url.path() == "/me/drive/root/delta");
        deltas.map(|r| r.url.query_pairs().find(|(k, _)| k == "token").map(|(_, v)| v.into_owned())).collect()
    }

    pub(crate) async fn delta_requests(&self) -> usize {
        self.delta_tokens().await.len()
    }

    /// Graph's metadata for F at version `ctag`, holding `content`.
    pub(crate) fn version(&self, ctag: &str, content: &[u8]) -> ResponseTemplate {
        let mut hash = konedrive_graph::quickxor::QuickXor::new();
        hash.update(content);
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "F", "name": "f.txt", "size": content.len(), "cTag": ctag,
            "file": {"hashes": {"quickXorHash": hash.finish_base64()}},
            "@microsoft.graph.downloadUrl": format!("{}/dl/F/{ctag}", self.graph.server.uri())
        }))
    }

    /// The bytes of F's version `ctag`.
    pub(crate) async fn serve_download(&self, ctag: &str, content: &[u8]) {
        let bytes = ResponseTemplate::new(200).set_body_bytes(content.to_vec());
        Mock::given(method("GET")).and(path(format!("/dl/F/{ctag}"))).respond_with(bytes).with_priority(2).mount(&self.graph.server).await;
    }

    /// Serves `content` as F's version c2, its metadata answered by `metadata`.
    pub(crate) async fn serve_new_version(&self, content: &[u8], metadata: impl wiremock::Respond + 'static) {
        Mock::given(method("GET")).and(path("/me/drive/items/F")).respond_with(metadata).with_priority(2).mount(&self.graph.server).await;
        self.serve_download("c2", content).await;
    }

    // ---- The folder, the store and the activity ----

    pub(crate) fn path(&self, rel: &str) -> PathBuf {
        self.root.path.join(rel)
    }

    /// A full path in the folder, as events name it.
    pub(crate) fn full(&self, rel: &str) -> String {
        self.path(rel).display().to_string()
    }

    /// Every event recorded so far, oldest first, as (kind, path, detail).
    pub(crate) fn activity(&self) -> Vec<(String, String, String)> {
        let mut events = konedrive_tree::off_runtime(|| self.report.activity.recent(1000)).unwrap();
        events.reverse();
        events.into_iter().map(|e| (e.kind.as_str().to_owned(), e.path, e.detail)).collect()
    }

    pub(crate) fn base(&self, id: &str) -> Option<Row> {
        let id = id.to_owned();
        konedrive_tree::off_runtime(|| self.store.call_blocking(move |s| s.get(Table::Items, &id))).unwrap()
    }

    pub(crate) fn deferred(&self, id: &str) -> Option<Change> {
        let id = id.to_owned();
        konedrive_tree::off_runtime(|| self.store.call_blocking(move |s| s.deferred(&id))).unwrap()
    }

    pub(crate) fn cloud_ctag(&self, id: &str) -> String {
        self.graph.with(|c| c.item(id).unwrap().ctag.clone())
    }

    // ---- The watcher's and the outbox worker's side of a read-write folder ----

    /// A live outbox row of `kind` for item `id` (None: something new) at
    /// `rel`, as the examination records one; its `seq`.
    pub(crate) fn row(&self, kind: OutboxKind, id: Option<&str>, rel: &str) -> i64 {
        let base = id.and_then(|id| self.base(id)).map(|r| Base { etag: r.etag, ctag: r.ctag, parent: r.parent_id, name: Some(r.name) });
        let rel = PathBuf::from(rel);
        let detection = Detection {
            kind,
            item_id: id.map(str::to_owned),
            inode: None,
            target_parent: base.as_ref().and_then(|b| b.parent.clone()),
            target_name: rel.file_name().map(|n| n.to_string_lossy().into_owned()),
            rel,
            base,
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
            size: None,
        };
        match konedrive_tree::off_runtime(|| self.store.call_blocking(move |s| s.outbox_record(&detection))).unwrap() {
            Recorded::Inserted(seq) | Recorded::Merged(seq) => seq,
            other => panic!("{other:?}"),
        }
    }

    /// The examination of `batch`, as the watcher's sink runs it.
    pub(crate) async fn examine(&self, batch: Batch) -> Examined {
        let (root, store, locks, liveness) = (self.root.clone(), self.store.clone(), self.locks.clone(), Arc::clone(&self.liveness));
        tokio::task::spawn_blocking(move || {
            let disk = Disk::open(&root, false).unwrap();
            crate::local::testing::examine(&disk, &store, &*liveness, &IgnoreList::default(), &locks, now(), &batch).unwrap()
        })
        .await
        .unwrap()
    }

    /// Examines what the cycles handed to the watcher, and nothing else,
    /// then lets the outbox worker run.
    pub(crate) async fn examine_handed_and_upload(&self) {
        let handed = std::mem::take(&mut *self.examined.lock().unwrap());
        for batch in handed {
            self.examine(batch).await;
        }
        self.upload().await;
    }

    pub(crate) fn config(&self) -> WorkerConfig {
        WorkerConfig {
            root: self.root.clone(),
            store: self.store.clone(),
            drive: self.drive(),
            locks: self.locks.clone(),
            machine_name: "fedora".into(),
            tree_lock: Arc::clone(&self.tree_lock),
            host: Arc::new(NoHost),
            limits: Limits { chunk: 320 * 1024 },
            moved_out: None,
            quota: crate::account::quota::Quota::detached(),
        }
    }

    /// The outbox worker, run until nothing more can run.
    pub(crate) async fn upload(&self) {
        Arc::new(Engine::new(self.config())).drain(&CancellationToken::new()).await;
    }

    /// The outbox uploads `content` as the new version of `id` at `rel` and
    /// commits it as the worker does: OneDrive takes it, the file gets its
    /// stamp and cTag, and the base Graph's answer, under the tree lock.
    pub(crate) async fn commit_upload(&self, id: &str, rel: &str, content: &[u8]) {
        self.graph.with(|c| c.edit(id, content));
        let item = self.drive().item(id).await.unwrap();
        let Change::Upsert(answer) = classify(&item) else { panic!("an upsert") };
        let _tree = self.tree_lock.lock().await;
        write_version(&self.path(rel), content, answer.ctag.as_deref().unwrap());
        let seq = self.row(OutboxKind::Update, Some(id), rel);
        let handle = FileHandle::of(&File::open(self.path(rel)).unwrap()).unwrap();
        self.store.call(move |s| s.outbox_commit(seq, Committed::Item { row: &answer, handle: Some(&handle) }, None)).await.unwrap();
    }
}

/// What OneDrive says to the cycle a [`World::step`] stands for.
pub(crate) enum Says<'a> {
    /// Its whole listing: reconciled in full.
    Whole(&'a [Change]),
    /// A delta on top of what is committed: reconciled over what it
    /// changes, or not at all when it changes nothing and nothing waits.
    Delta(&'a [Change]),
    /// A delta, in a cycle asked for a Full reconcile.
    DeltaInFull(&'a [Change]),
    /// What the fake OneDrive's delta feed says since the stored link.
    Fetched,
}

fn unlock(root: &SyncRoot) {
    Disk::open(root, false).unwrap().unlock_tree().unwrap();
}

/// One reconcile over what a cycle staged, holding its locks: run whole, or
/// its folder half and its commit half one after the other, with whatever
/// the test does in between — what an examination or a crash at that moment
/// meets. Where the staging found nothing to reconcile, every half does
/// nothing.
pub(crate) struct Step {
    reconcile: Option<Reconcile>,
    _held: Option<Held>,
    scope: Option<Scope>,
    commit: Option<Commit>,
    passed: Option<(Prepared, Reconciled)>,
    /// The folder whose lock is taken off after each half ([`Options::locked`]).
    unlock: Option<SyncRoot>,
}

impl Step {
    /// The whole reconcile, as a cycle runs it.
    pub(crate) async fn run(mut self) -> Result<Reconciled, CycleError> {
        let Some(reconcile) = self.reconcile.take() else { return Ok(Reconciled::default()) };
        let (scope, commit) = (self.scope.take().unwrap(), self.commit.take().unwrap());
        let done = tokio::task::spawn_blocking(move || reconcile.run(scope, commit)).await.unwrap();
        self.unlock.iter().for_each(unlock);
        done
    }

    /// The folder made to match `staging`; nothing is committed.
    pub(crate) async fn apply(&mut self) -> Result<(), CycleError> {
        let Some(reconcile) = self.reconcile.take() else { return Ok(()) };
        let scope = self.scope.take().unwrap();
        let (reconcile, passed) = tokio::task::spawn_blocking(move || {
            let passed = reconcile.prepare().and_then(|prepared| {
                let prepared = prepared.expect("the drive's root is listed");
                let done = reconcile.apply_with_handover(&prepared, scope)?;
                Ok((prepared, done))
            });
            (reconcile, passed)
        })
        .await
        .unwrap();
        self.reconcile = Some(reconcile);
        self.unlock.iter().for_each(unlock);
        self.passed = Some(passed?);
        Ok(())
    }

    /// The commit of what [`apply`](Self::apply) did, and what follows it.
    pub(crate) async fn commit(mut self) -> Result<Reconciled, CycleError> {
        let Some(reconcile) = self.reconcile.take() else { return Ok(Reconciled::default()) };
        let (prepared, done) = self.passed.take().expect("applied first");
        let commit = self.commit.take().unwrap();
        tokio::task::spawn_blocking(move || reconcile.commit(&prepared, done, commit)).await.unwrap()
    }
}

/// The materializer of a read-only reconcile with no helper, for a fixture that places a
/// staged listing by itself (`local::testing::Folder`): the one place a test names the
/// materializer's parts.
pub(crate) fn materializer(disk: Disk, store: &Store, root_item_id: &str, rescue_into: PathBuf, claimed: Option<Claimed>, runtime: &tokio::runtime::Handle) -> Materializer {
    Materializer {
        disk,
        store: store.clone(),
        link: None,
        runtime: runtime.clone(),
        locks: InodeLocks::new(),
        root_item_id: root_item_id.into(),
        rescue_into,
        cancel: CancellationToken::new(),
        mode: Mode::ReadOnly,
        claimed,
    }
}

/// What a delta page holds, as Graph's JSON, for the feed answered by hand.
pub(crate) mod feed {
    use serde_json::{json, Value};

    pub(crate) fn root_item() -> Value {
        json!({"id": "R", "root": {}, "folder": {}})
    }

    pub(crate) fn folder(id: &str, parent: &str, name: &str) -> Value {
        json!({"id": id, "name": name, "folder": {}, "parentReference": {"id": parent}})
    }

    pub(crate) fn file(id: &str, parent: &str, name: &str, ctag: &str) -> Value {
        json!({"id": id, "name": name, "size": 10, "cTag": ctag, "file": {}, "parentReference": {"id": parent},
               "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}})
    }

    pub(crate) fn vault() -> Value {
        json!({"id": "V", "name": "Personal Vault", "folder": {}, "specialFolder": {"name": "vault"}, "parentReference": {"id": "R"}})
    }
}

pub(crate) fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

pub(crate) fn id_at(path: &Path) -> Option<String> {
    xattr::get(path, XATTR_ITEM_ID).unwrap().map(|v| String::from_utf8(v).unwrap())
}

pub(crate) fn state_at(path: &Path) -> Option<State> {
    placeholder::read_state(&File::open(path).unwrap()).unwrap()
}

pub(crate) fn ino(at: &Path) -> u64 {
    std::fs::symlink_metadata(at).unwrap().ino()
}

pub(crate) fn mode(at: &Path) -> u32 {
    std::fs::symlink_metadata(at).unwrap().permissions().mode() & 0o7777
}

/// Everything beneath `root`, hidden names too, as sorted relative paths.
pub(crate) fn tree_of(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(rel) = pending.pop() {
        for entry in std::fs::read_dir(root.join(&rel)).unwrap() {
            let entry = entry.unwrap();
            let child = rel.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                pending.push(child.clone());
            }
            out.push(child.display().to_string());
        }
    }
    out.sort();
    out
}

/// `content` in the file at `at`, as a finished download (or upload) of
/// version `ctag` leaves it.
pub(crate) fn write_version(at: &Path, content: &[u8], ctag: &str) {
    let file = placeholder::reopen_writable(&File::open(at).unwrap()).unwrap();
    file.set_len(content.len() as u64).unwrap();
    file.write_all_at(content, 0).unwrap();
    placeholder::write_ctag(&file, ctag).unwrap();
    placeholder::write_state(&file, State::Hydrated).unwrap();
    placeholder::write_stamp(&file).unwrap();
}

/// An edit made here: the stamp no longer matches.
pub(crate) fn edit(at: &Path, more: &[u8]) {
    std::thread::sleep(Duration::from_millis(10));
    std::fs::OpenOptions::new().append(true).open(at).unwrap().write_all(more).unwrap();
}

/// Renames `docs` to `papers` in the (locked) folder, as a user with
/// their own chmod might while a replacement downloads.
pub(crate) fn move_docs_away(root: &Path) {
    let dir = File::open(root).unwrap();
    placeholder::with_owner_write(&dir, || std::fs::rename(root.join("docs"), root.join("papers"))).unwrap();
}
