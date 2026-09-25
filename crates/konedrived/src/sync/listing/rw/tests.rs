//! A read-write folder's cycle (`docs/design/writes.md` §9) against a fake
//! OneDrive on wiremock — the outbox worker's, now serving the delta feed too
//! — in a temporary folder: the tree lock, the stale-delta guard, what
//! waits, echoes of the outbox's own changes, the `410` variants, a
//! replacement under a write lease, the order of the cycle and the outbox,
//! and a folder removed in OneDrive that holds local work. No real network.

use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{self, State, XATTR_ITEM_ID, XATTR_ROOT};
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use nix::sys::socket::{accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::ResponseTemplate;

use super::super::{CycleReport, Listing, ListingContext, FULL_THRESHOLD};
use super::Writes;
use crate::sync::activity::Report;
use crate::sync::disk::Disk;
use crate::sync::graph_source::GraphSource;
use crate::sync::helper::HelperLink;
use crate::sync::local::{Batch, Examined, Examiner, FakeLiveness, IgnoreList};
use crate::sync::pin::Pins;
use crate::sync::root::SyncRoot;
use crate::sync::upload::fake::{FakeGraph, FakeItem, ROOT};
use crate::sync::upload::{Engine, Limits, NoHost, OutboxWorker, WorkerConfig};
use crate::sync::{InodeLocks, SyncSnapshot, SyncStateHandle};
use crate::tree::outbox::{Base, Committed, Detection, OutboxKind, OutboxState, Recorded};
use crate::tree::{classify, Change, Store, Table, TreeStore};

struct World {
    graph: FakeGraph,
    _dir: tempfile::TempDir,
    root: SyncRoot,
    store: Store,
    state: SyncStateHandle,
    report: Report,
    pins: Arc<Pins>,
    link: HelperLink,
    _helper: tempfile::TempDir,
    rescue: tempfile::TempDir,
    tree_lock: Arc<tokio::sync::Mutex<()>>,
    locks: InodeLocks,
    liveness: Arc<FakeLiveness>,
    /// What each cycle handed the watcher to examine.
    examined: Arc<Mutex<Vec<Batch>>>,
    /// Cycles that went through, as the outbox worker hears of them.
    cycles: Arc<AtomicUsize>,
    /// Rows `Writes::dropped_removed` heard were dropped: a held or pending
    /// removal whose item was already gone from OneDrive.
    dropped: Arc<Mutex<Vec<crate::tree::outbox::OutboxRow>>>,
}

/// A helper that acknowledges everything.
fn helper(socket_path: &Path) {
    let listener = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None).unwrap();
    bind(listener.as_raw_fd(), &UnixAddr::new(socket_path).unwrap()).unwrap();
    listen(&listener, Backlog::new(4).unwrap()).unwrap();
    std::thread::spawn(move || {
        let accepted = accept(listener.as_raw_fd()).unwrap();
        // SAFETY: a descriptor `accept` just returned, owned by nothing else.
        let mut channel = Channel::new(unsafe { UnixStream::from_raw_fd(accepted) }).unwrap();
        channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
        let _ = channel.recv::<ToHelper>().unwrap();
        channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
        while channel.recv::<ToHelper>().is_ok() {
            if channel.send(&ToDaemon::Ack { errno: 0 }, None).is_err() {
                break;
            }
        }
    });
}

/// OneDrive holds `docs/f.txt` ("one") and `top.txt` ("top").
async fn world() -> World {
    let graph = FakeGraph::start().await;
    graph.with(|c| {
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
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().canonicalize().unwrap().join("OneDrive");
    std::fs::create_dir(&folder).unwrap();
    let root_id = "7e3a9c1d-2b4f-4a6e-8d0c-1f2e3d4c5b6a".to_owned();
    xattr::set(&folder, XATTR_ROOT, root_id.as_bytes()).unwrap();
    let store = Store::new(TreeStore::in_memory().unwrap());
    let state = SyncStateHandle::new(SyncSnapshot { root_path: folder.display().to_string(), ..SyncSnapshot::default() });
    let report = Report::new(state.clone());
    report.activity.attach(store.clone(), &folder);
    let pins = Pins::detached(state.clone());
    let helper_dir = tempfile::tempdir().unwrap();
    let socket_path = helper_dir.path().join("helper.sock");
    helper(&socket_path);
    let link = HelperLink::connect(&socket_path).await.unwrap().0;
    World {
        graph,
        _dir: dir,
        root: SyncRoot { path: folder, root_id },
        store,
        state,
        report,
        pins,
        link,
        _helper: helper_dir,
        rescue: tempfile::tempdir().unwrap(),
        tree_lock: Arc::new(tokio::sync::Mutex::new(())),
        locks: InodeLocks::new(),
        liveness: Arc::new(FakeLiveness::new()),
        examined: Arc::default(),
        cycles: Arc::default(),
        dropped: Arc::default(),
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

impl World {
    fn writes(&self, scanned: Option<tokio::sync::watch::Receiver<bool>>) -> Writes {
        let (examined, cycles, dropped) = (Arc::clone(&self.examined), Arc::clone(&self.cycles), Arc::clone(&self.dropped));
        Writes {
            tree_lock: Arc::clone(&self.tree_lock),
            machine_name: "fedora".into(),
            ignore: crate::sync::local::IgnoreList::default().shared(),
            scanned,
            examine: Arc::new(move |batch| examined.lock().unwrap().push(batch)),
            cycled: Arc::new(move || {
                cycles.fetch_add(1, Ordering::SeqCst);
            }),
            dropped_removed: Arc::new(move |rows| dropped.lock().unwrap().extend(rows)),
        }
    }

    fn listing_with(&self, scanned: Option<tokio::sync::watch::Receiver<bool>>) -> Arc<Listing> {
        Listing::new(ListingContext { writes: Some(self.writes(scanned)), ..self.context_parts() })
    }

    /// A read-write folder's context, but for its `writes`.
    fn context_parts(&self) -> ListingContext {
        let drive = self.graph.client();
        ListingContext {
            root: self.root.clone(),
            intercepted: true,
            store: self.store.clone(),
            drive: drive.clone(),
            drive_record: None,
            source: Arc::new(GraphSource::new(drive)),
            link: Arc::new(std::sync::Mutex::new(Some(self.link.clone()))),
            locks: self.locks.clone(),
            state: self.state.clone(),
            lifecycle: Arc::new(tokio::sync::RwLock::new(())),
            rescue_dir: self.rescue.path().to_path_buf(),
            full_threshold: FULL_THRESHOLD,
            after_cycle: None,
            report: self.report.clone(),
            pins: Arc::clone(&self.pins),
            locked: false,
            writes: None,
            neighbours: None,
        }
    }

    /// A read-write folder's listing, after its first cycle.
    async fn listed(&self) -> Arc<Listing> {
        let listing = self.listing_with(None);
        self.cycle(&listing).await;
        listing
    }

    /// One cycle, with the replacements it started.
    async fn cycle(&self, listing: &Arc<Listing>) -> CycleReport {
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        listing.join_replacements().await;
        report
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path.join(rel)
    }

    fn config(&self) -> WorkerConfig {
        WorkerConfig {
            root: self.root.clone(),
            store: self.store.clone(),
            drive: self.graph.client(),
            locks: self.locks.clone(),
            machine_name: "fedora".into(),
            tree_lock: Arc::clone(&self.tree_lock),
            host: Arc::new(NoHost),
            limits: Limits { small_slots: 4, large_slots: 2, small_max: 320 * 1024, chunk: 320 * 1024 },
            moved_out: None,
        }
    }

    /// The outbox worker, run until nothing more can run.
    async fn upload(&self) {
        Arc::new(Engine::new(self.config())).drain(&CancellationToken::new()).await;
    }

    /// The examination of `batch`, as the watcher's sink runs it.
    async fn examine(&self, batch: Batch) -> Examined {
        let (root, store, locks, liveness) = (self.root.clone(), self.store.clone(), self.locks.clone(), Arc::clone(&self.liveness));
        tokio::task::spawn_blocking(move || {
            let disk = Disk::open(&root, false).unwrap();
            Examiner { disk: &disk, store: &store, liveness: &*liveness, ignore: &IgnoreList::default(), locks: &locks, now: now() }
                .examine(&batch)
                .unwrap()
        })
        .await
        .unwrap()
    }

    fn base(&self, id: &str) -> Option<crate::tree::Row> {
        self.store.with(|s| s.get(Table::Items, id)).unwrap()
    }

    fn deferred(&self, id: &str) -> Option<Change> {
        self.store.with(|s| s.deferred(id)).unwrap()
    }

    fn cloud_ctag(&self, id: &str) -> String {
        self.graph.with(|c| c.item(id).unwrap().ctag.clone())
    }

    /// An outbox row of `kind` for item `id` at `rel`, as the examination
    /// records one; its `seq`.
    fn row(&self, kind: OutboxKind, id: &str, rel: &str) -> i64 {
        let base = self.base(id).map(|r| Base { etag: r.etag, ctag: r.ctag, parent: r.parent_id, name: Some(r.name) });
        let rel = PathBuf::from(rel);
        let detection = Detection {
            kind,
            item_id: Some(id.into()),
            inode: None,
            target_parent: base.as_ref().and_then(|b| b.parent.clone()),
            target_name: rel.file_name().map(|n| n.to_string_lossy().into_owned()),
            rel,
            base,
            same_content: false,
            state: OutboxState::Ready,
            reason: None,
            next_try: None,
        };
        match self.store.with(|s| s.outbox_record(&detection)).unwrap() {
            Recorded::Inserted(seq) | Recorded::Merged(seq) => seq,
            other => panic!("{other:?}"),
        }
    }

    /// The outbox uploads `content` as the new version of `id` at `rel` and
    /// commits it as the worker does: OneDrive takes it, the file gets its
    /// stamp and cTag, and the base Graph's answer, under the tree lock.
    async fn commit_upload(&self, id: &str, rel: &str, content: &[u8]) {
        self.graph.with(|c| c.edit(id, content));
        let item = self.graph.client().item(id).await.unwrap();
        let Change::Upsert(answer) = classify(&item) else { panic!("an upsert") };
        let _tree = self.tree_lock.lock().await;
        write_version(&self.path(rel), content, answer.ctag.as_deref().unwrap());
        let seq = self.row(OutboxKind::Update, id, rel);
        let handle = FileHandle::of(&File::open(self.path(rel)).unwrap()).unwrap();
        self.store.with(|s| s.outbox_commit(seq, Committed::Item { row: &answer, handle: Some(&handle) }, None)).unwrap();
    }
}

fn id_at(path: &Path) -> Option<String> {
    xattr::get(path, XATTR_ITEM_ID).unwrap().map(|v| String::from_utf8(v).unwrap())
}

fn state_at(path: &Path) -> Option<State> {
    placeholder::read_state(&File::open(path).unwrap()).unwrap()
}

/// `content` in the file at `at`, downloaded (or uploaded) as version `ctag`.
fn write_version(at: &Path, content: &[u8], ctag: &str) {
    use std::os::unix::fs::FileExt;
    let file = placeholder::reopen_writable(&File::open(at).unwrap()).unwrap();
    file.set_len(content.len() as u64).unwrap();
    file.write_all_at(content, 0).unwrap();
    placeholder::write_ctag(&file, ctag).unwrap();
    placeholder::write_state(&file, State::Hydrated).unwrap();
    placeholder::write_stamp(&file).unwrap();
}

fn names(batch: &Batch) -> String {
    format!("{batch:?}")
}

/// the read-write reconcile must, item 2: the cycle holds the outbox worker's tree lock from its
/// staging to its swap, so that no commit lands in between (and is
/// reverted by the swap).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cycle_holds_the_tree_lock_from_staging_to_the_swap() {
    let w = world().await;
    let listing = w.listed().await;
    w.graph.with(|c| c.edit("F", b"two"));
    let held = Arc::clone(&w.tree_lock).lock_owned().await;
    let cycle = {
        let listing = Arc::clone(&listing);
        tokio::spawn(async move { listing.cycle(&CancellationToken::new()).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!cycle.is_finished(), "the cycle staged while the outbox held the tree");
    assert_ne!(w.base("F").unwrap().ctag.as_deref(), Some(w.cloud_ctag("F").as_str()));
    drop(held);
    cycle.await.unwrap().unwrap();
    assert_eq!(w.base("F").unwrap().ctag.as_deref(), Some(w.cloud_ctag("F").as_str()));
    assert_eq!(w.cycles.load(Ordering::SeqCst), 2, "each cycle tells the outbox");
}

/// §3.7's stale-delta guard, and the read-write reconcile must, items 1 and 4: a delta fetched
/// before an upload's commit does not take the item back to the version
/// before it; a change OneDrive made after the commit is taken — read again
/// — and the file is replaced, its base kept at the version it holds until
/// the new one is in place, and its new inode recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delta_fetched_before_a_commit_does_not_undo_it() {
    let w = world().await;
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));

    // The next delta is answered late, as the drive was before the upload.
    let stale = w.graph.with(|c| c.delta_body());
    w.graph.with(|c| c.script("GET", "root/delta", ResponseTemplate::new(200).set_body_json(stale).set_delay(Duration::from_millis(800)), 1));
    let cycle = {
        let listing = Arc::clone(&listing);
        tokio::spawn(async move { listing.cycle(&CancellationToken::new()).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    w.commit_upload("F", "docs/f.txt", b"mine").await;
    let committed = w.base("F").unwrap();
    let report = cycle.await.unwrap().unwrap();
    listing.join_replacements().await;
    assert_eq!(w.base("F").unwrap().etag, committed.etag, "the stale delta did not undo the commit");
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"mine");
    assert!(report.applied.replacements.is_empty(), "{:?}", report.applied.replacements);
    assert!(w.graph.with(|c| c.count("GET", "items/F")) >= 1, "read again from OneDrive");

    // Now OneDrive changes it again right after an upload, within one fetch.
    let stale = w.graph.with(|c| c.delta_body());
    w.graph.with(|c| c.script("GET", "root/delta", ResponseTemplate::new(200).set_body_json(stale).set_delay(Duration::from_millis(800)), 1));
    let cycle = {
        let listing = Arc::clone(&listing);
        tokio::spawn(async move { listing.cycle(&CancellationToken::new()).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    w.commit_upload("F", "docs/f.txt", b"mine again").await;
    w.graph.with(|c| c.edit("F", b"theirs"));
    let report = cycle.await.unwrap().unwrap();
    assert_eq!(report.applied.replacements.len(), 1, "OneDrive's newer version is fetched");
    listing.join_replacements().await;
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"theirs");
    assert_eq!(w.base("F").unwrap().ctag.as_deref(), Some(w.cloud_ctag("F").as_str()), "the base took it as it landed");
    assert!(w.deferred("F").is_none());
    let handle = FileHandle::of(&File::open(w.path("docs/f.txt")).unwrap()).unwrap();
    assert_eq!(w.store.with(|s| s.local_handle("F")).unwrap(), Some(handle), "the new version's inode is the item's");
}

/// WR6, §3.7 echo: the outbox's own create, edit and delete come back in the
/// delta and change nothing here — no download, no replacement, no removal,
/// nothing said.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_outbox_own_changes_coming_back_in_the_delta_change_nothing() {
    let w = world().await;
    let listing = w.listed().await;
    let new = w.path("docs/new.txt");
    std::fs::write(&new, b"hello").unwrap();
    let mut batch = Batch::new();
    batch.name(Path::new("docs"), OsStr::new("new.txt"));
    assert_eq!(w.examine(batch).await.applied.queued.len(), 1);
    w.upload().await;
    let id = id_at(&new).expect("uploaded");
    let inode = std::fs::metadata(&new).unwrap().ino();

    let report = w.cycle(&listing).await;
    assert_eq!((report.applied.created, report.applied.updated, report.applied.deleted), (0, 0, 0));
    assert!(report.applied.replacements.is_empty() && report.applied.changes.is_empty() && report.applied.unsettled.is_empty());
    assert_eq!(std::fs::metadata(&new).unwrap().ino(), inode);
    assert_eq!(state_at(&new), Some(State::Hydrated));

    // An edit, uploaded, comes back.
    std::thread::sleep(Duration::from_millis(10));
    std::fs::OpenOptions::new().append(true).open(&new).unwrap().write_all(b" again").unwrap();
    let mut batch = Batch::new();
    batch.written(Path::new("docs"), OsStr::new("new.txt"), None);
    assert_eq!(w.examine(batch).await.applied.queued.len(), 1);
    w.upload().await;
    let report = w.cycle(&listing).await;
    assert!(report.applied.replacements.is_empty() && report.applied.changes.is_empty());
    assert_eq!(std::fs::read(&new).unwrap(), b"hello again");
    assert_eq!(std::fs::metadata(&new).unwrap().ino(), inode);

    // A delete, done in OneDrive, comes back.
    std::fs::remove_file(&new).unwrap();
    let mut batch = Batch::new();
    batch.name(Path::new("docs"), OsStr::new("new.txt"));
    assert_eq!(w.examine(batch).await.applied.queued.len(), 1, "gone, on the (fake) helper's word");
    w.upload().await;
    assert!(w.graph.with(|c| c.bin.contains_key(&id)));
    let report = w.cycle(&listing).await;
    assert!(report.applied.changes.is_empty() && !new.exists() && w.base(&id).is_none());
    let said: Vec<String> = w.report.activity.recent(100).unwrap().into_iter().filter(|e| e.path.ends_with("new.txt")).map(|e| e.kind).collect();
    assert!(said.iter().all(|kind| kind != "added" && kind != "updated" && kind != "removed"), "{said:?}");
}

/// §3.7 `410`: `resyncChangesUploadDifferences` keeps what the new listing
/// left out and was downloaded here — its attributes off, for the outbox to
/// upload again — keeps a download whose version differs from the listing's
/// beside it, as a copy, and removes placeholders, which hold nothing;
/// `resyncChangesApplyDifferences` removes what is gone and replaces what
/// changed, keeping only local work.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_two_resyncs_differ_in_what_the_listing_left_out() {
    for (code, upload) in [("resyncChangesUploadDifferences", true), ("resyncChangesApplyDifferences", false)] {
        let w = world().await;
        let listing = w.listed().await;
        w.graph.with(|c| c.add_file("P", "D", "p.txt", b"p"));
        w.cycle(&listing).await;
        assert_eq!(state_at(&w.path("docs/p.txt")), Some(State::OnlineOnly));
        write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
        write_version(&w.path("top.txt"), b"top", &w.cloud_ctag("T"));
        w.graph.with(|c| {
            c.trash("F");
            c.trash("P");
            c.edit("T", b"top, changed");
            c.script("GET", "root/delta", ResponseTemplate::new(410).set_body_json(json!({"error": {"code": "resyncRequired", "innerError": {"code": code}}})), 1);
        });
        let report = w.cycle(&listing).await;
        assert!(report.full, "{code}");
        assert!(!w.path("docs/p.txt").exists(), "{code}: a placeholder holds nothing here");
        assert_eq!(w.path("docs/f.txt").exists(), upload, "{code}");
        assert_eq!(w.path("top-fedora.txt").exists(), upload, "{code}");
        if upload {
            assert_eq!(id_at(&w.path("docs/f.txt")), None, "uploaded again as new");
            assert_eq!(std::fs::read(w.path("top-fedora.txt")).unwrap(), b"top", "the version OneDrive may have lost is kept");
            assert_eq!(id_at(&w.path("top-fedora.txt")), None);
            assert_eq!(state_at(&w.path("top.txt")), Some(State::OnlineOnly), "OneDrive's version takes the name");
            let examined = w.examined.lock().unwrap().iter().map(names).collect::<String>();
            assert!(examined.contains("f.txt") && examined.contains("top-fedora.txt"), "{examined}");
        } else {
            assert_eq!(std::fs::read(w.path("top.txt")).unwrap(), b"top, changed", "a clean download takes the new version");
        }
    }
}

/// §3.7 replacement under a lease, and the read-write reconcile must, item 4: a file open for
/// writing is not replaced — nothing is downloaded — and its base keeps the
/// version it holds; once closed, the next cycle replaces it and the base
/// follows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replacement_waits_for_a_file_open_for_writing() {
    let w = world().await;
    let listing = w.listed().await;
    let old = w.cloud_ctag("F");
    write_version(&w.path("docs/f.txt"), b"one", &old);
    w.graph.with(|c| c.edit("F", b"two"));
    let writer = std::fs::OpenOptions::new().write(true).open(w.path("docs/f.txt")).unwrap();
    let report = w.cycle(&listing).await;
    assert_eq!(report.applied.replacements.len(), 1);
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"one");
    assert_eq!(w.graph.with(|c| c.count("GET", "dl/F")), 0, "nothing downloaded for nothing");
    assert_eq!(w.base("F").unwrap().ctag.as_deref(), Some(old.as_str()), "the base keeps the version on disk");
    assert!(w.deferred("F").is_some());

    drop(writer);
    w.cycle(&listing).await;
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"two");
    assert_eq!(w.base("F").unwrap().ctag.as_deref(), Some(w.cloud_ctag("F").as_str()));
    assert!(w.deferred("F").is_none());
}

/// F82 (7): an outbox commit that adopted OneDrive's answer with a newer
/// cTag than the file holds (a move whose earlier PATCH landed, or whose
/// place OneDrive won) — the delta then brings nothing new — is looked at
/// again by the next cycle, and the unchanged file is replaced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_older_than_what_the_outbox_committed_is_replaced() {
    let w = world().await;
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    w.cycle(&listing).await;
    w.graph.with(|c| c.edit("F", b"two"));
    let item = w.graph.client().item("F").await.unwrap();
    let Change::Upsert(answer) = classify(&item) else { panic!("an upsert") };
    {
        let _tree = w.tree_lock.lock().await;
        let seq = w.row(OutboxKind::Update, "F", "docs/f.txt");
        w.store.with(|s| s.outbox_commit(seq, Committed::Item { row: &answer, handle: None }, None)).unwrap();
    }
    let report = w.cycle(&listing).await;
    assert_eq!(report.applied.replacements.len(), 1, "{report:?}");
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"two");
}

/// the read-write reconcile must, item 1 (the examination's hunk in `replace_through`, read-only as before): a
/// replacement records the new version's inode as the item's, so that a
/// move of it out of the folder is never taken for a delete.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replacement_records_its_new_inode_in_a_read_only_folder_too() {
    let w = world().await;
    let read_only = || Listing::new(ListingContext { locked: true, writes: None, ..w.context_parts() });
    let listing = read_only();
    w.cycle(&listing).await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    w.graph.with(|c| c.edit("F", b"two"));
    w.cycle(&listing).await;
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"two");
    let handle = FileHandle::of(&File::open(w.path("docs/f.txt")).unwrap()).unwrap();
    assert_eq!(w.store.with(|s| s.local_handle("F")).unwrap(), Some(handle));
    Disk::open(&w.root, false).unwrap().unlock_tree().unwrap();
}

/// §3.3, §4.9: the folder's first cycle waits for the watcher's Full local
/// scan, and the outbox for a cycle: nothing is sent before one went through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_cycle_waits_for_the_scan_and_the_outbox_for_the_cycle() {
    let w = world().await;
    let (scanned, first_scan) = tokio::sync::watch::channel(false);
    let listing = w.listing_with(Some(first_scan));
    let cycle = {
        let listing = Arc::clone(&listing);
        tokio::spawn(async move { listing.cycle(&CancellationToken::new()).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(w.graph.with(|c| c.count("GET", "root/delta")), 0, "OneDrive asked before the local scan");
    scanned.send(true).unwrap();
    cycle.await.unwrap().unwrap();
    assert_eq!(w.cycles.load(Ordering::SeqCst), 1);

    let worker = OutboxWorker::new(w.config());
    worker.wait_for_cycle(false);
    std::fs::write(w.path("docs/new.txt"), b"hello").unwrap();
    let mut batch = Batch::new();
    batch.name(Path::new("docs"), OsStr::new("new.txt"));
    w.examine(batch).await;
    worker.start();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(w.graph.with(|c| c.at("docs/new.txt").is_none()), "sent before a cycle");
    worker.cycle_done();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while w.graph.with(|c| c.at("docs/new.txt").is_none()) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(w.graph.with(|c| c.at("docs/new.txt").is_some()), "sent once the cycle went through");
    worker.stop().await;
}

/// §3.7: an item with a live row keeps its base at the swap, and OneDrive's
/// change waits; once the row is gone, the next cycle applies it — the delta
/// cursor never sends it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_that_waited_for_a_row_is_applied_once_the_row_is_gone() {
    let w = world().await;
    let listing = w.listed().await;
    let seq = w.row(OutboxKind::Update, "F", "docs/f.txt");
    w.graph.with(|c| c.rename("F", "D", "renamed.txt"));
    w.cycle(&listing).await;
    assert!(w.path("docs/f.txt").exists() && !w.path("docs/renamed.txt").exists());
    assert_eq!(w.base("F").unwrap().name, "f.txt");
    assert!(w.deferred("F").is_some());

    w.store.with(|s| s.outbox_drop(seq, None, None, None)).unwrap();
    w.cycle(&listing).await;
    assert_eq!(id_at(&w.path("docs/renamed.txt")).as_deref(), Some("F"));
    assert!(!w.path("docs/f.txt").exists());
    assert_eq!(w.base("F").unwrap().name, "renamed.txt");
    assert!(w.deferred("F").is_none());
}

/// §6 folders, F82 (4): a folder removed in OneDrive while a new file waits
/// in it to be uploaded stays, as a new folder, without its clean
/// placeholders; the file's row waits for the folder's `mkdir`; the
/// examination makes that `mkdir`, and the outbox makes the folder again in
/// OneDrive with the file in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_removed_in_onedrive_with_local_work_in_it_is_made_again() {
    let w = world().await;
    let listing = w.listed().await;
    std::fs::write(w.path("docs/mine.txt"), b"mine").unwrap();
    let mut batch = Batch::new();
    batch.name(Path::new("docs"), OsStr::new("mine.txt"));
    w.examine(batch).await;
    w.graph.with(|c| c.trash("D"));
    let report = w.cycle(&listing).await;
    assert_eq!(report.applied.recreated, vec!["D".to_owned()]);
    assert_eq!(id_at(&w.path("docs")), None);
    assert!(!w.path("docs/f.txt").exists() && w.path("docs/mine.txt").exists());
    let rows = w.store.with(|s| s.outbox_rows()).unwrap();
    assert!(rows.iter().all(|row| row.target_parent.is_none()), "{rows:?}");

    let hinted = std::mem::take(&mut *w.examined.lock().unwrap());
    for batch in hinted {
        w.examine(batch).await;
    }
    w.upload().await;
    assert!(w.graph.with(|c| c.at("docs/mine.txt").is_some_and(|f| f.content == b"mine")), "{:?}", w.graph.with(|c| c.paths()));
    assert!(id_at(&w.path("docs")).is_some());
}

/// Where the object `handle` names is, found by walking `bases` as the
/// helper's `OpenByHandle` would find it: an object renamed anywhere under
/// them, inside the folder or out of it, is still found.
fn find_by_handle(bases: &[PathBuf], handle: &FileHandle) -> Option<PathBuf> {
    let mut stack: Vec<PathBuf> = bases.to_vec();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let opened = File::open(&dir).ok()?;
        for entry in entries.flatten() {
            if FileHandle::at(&opened, &entry.file_name()).ok().as_ref() == Some(handle) {
                return Some(entry.path());
            }
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(entry.path());
            }
        }
    }
    None
}

/// "Is this object alive, and where?", answered as the helper answers it,
/// from where the objects really are.
struct Scanning(Vec<PathBuf>);

impl crate::sync::local::Liveness for Scanning {
    fn whereabouts(&self, handle: &FileHandle) -> std::io::Result<crate::sync::local::Whereabouts> {
        Ok(match find_by_handle(&self.0, handle) {
            Some(path) => crate::sync::local::Whereabouts::At(path),
            None => crate::sync::local::Whereabouts::Gone,
        })
    }
}

/// The helper for the worker's `move-out` rows, opening objects where
/// they really are.
struct ScanningHelper(Vec<PathBuf>);

#[async_trait::async_trait]
impl crate::sync::upload::move_out::Helper for ScanningHelper {
    async fn open_by_handle(&self, _dir: &File, handle: &FileHandle) -> Result<std::os::fd::OwnedFd, crate::sync::helper::HelperError> {
        let stale = || crate::sync::helper::HelperError::Refused(libc::ESTALE);
        let path = find_by_handle(&self.0, handle).ok_or_else(stale)?;
        let file = if path.is_dir() {
            File::open(&path)
        } else {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW).open(&path)
        };
        Ok(file.map_err(|_| stale())?.into())
    }

    async fn mark_file(&self, _file: &File) -> Result<(), crate::sync::helper::HelperError> {
        Ok(())
    }

    async fn mark_dir(&self, _dir: &File) -> Result<(), crate::sync::helper::HelperError> {
        Ok(())
    }

    async fn unmark_dir(&self, _dir: &File) -> Result<(), crate::sync::helper::HelperError> {
        Ok(())
    }

    fn clearance(&self) -> Option<crate::sync::helper::Clearance> {
        Some(crate::sync::helper::Clearance::NoLink(self.0[0].join("no-helper.sock")))
    }
}

impl World {
    /// Everywhere an object of the folder can be: the folder, beside it, and
    /// the rescue directory.
    fn everywhere(&self) -> Vec<PathBuf> {
        vec![self.root.path.parent().unwrap().to_path_buf(), self.rescue.path().canonicalize().unwrap()]
    }

    /// the move-out step in the loop: a Full local scan whose "where is it now?" is
    /// answered from where objects really are, then the worker, with
    /// `move-out` rows downloaded and deleted in OneDrive as the move-out step does.
    async fn scan_and_upload(&self) -> Examined {
        let (root, store, locks, bases) = (self.root.clone(), self.store.clone(), self.locks.clone(), self.everywhere());
        let examined = tokio::task::spawn_blocking(move || {
            let disk = Disk::open(&root, false).unwrap();
            let liveness = Scanning(bases);
            Examiner { disk: &disk, store: &store, liveness: &liveness, ignore: &IgnoreList::default(), locks: &locks, now: now() }
                .full_scan()
                .unwrap()
        })
        .await
        .unwrap();
        let root = self.root.path.clone();
        let mut config = self.config();
        config.moved_out = Some(crate::sync::upload::move_out::MoveOuts {
            helper: Arc::new(ScanningHelper(self.everywhere())),
            filler: Arc::new(crate::sync::upload::move_out::SourceFill(Arc::new(GraphSource::new(self.graph.client())))),
            route: None,
            home_trash: None,
            roots: Arc::new(move || vec![root.clone()]),
        });
        Arc::new(Engine::new(config)).drain(&CancellationToken::new()).await;
        examined
    }

    fn deletes(&self) -> usize {
        self.graph.with(|c| c.count("DELETE", "items/"))
    }
}

/// C1: one delta renames `top.txt` in OneDrive and adds a file to `docs`,
/// which was renamed here and not examined yet. The Changed pass moves
/// `top.txt` to the holding directory, then hands over to the Full scan (the
/// new file's folder is not where the tree has it), which must take it from
/// there to its new name — never out of the folder, where the move-out step would take it
/// for a move out and delete it in OneDrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_pass_handing_over_with_something_in_holding_keeps_it_in_the_folder() {
    let w = world().await;
    let listing = w.listed().await;
    std::fs::rename(w.path("docs"), w.path("papers")).unwrap();
    w.graph.with(|c| {
        c.rename("T", ROOT, "top2.txt");
        c.add_file("N", "D", "new.txt", b"new");
    });
    let report = w.cycle(&listing).await;
    assert!(report.full, "handed over to the Full scan");
    assert!(report.applied.rescued.is_empty(), "moved out of the folder: {:?}", report.applied.rescued);
    assert_eq!(id_at(&w.path("top2.txt")).as_deref(), Some("T"), "placed from the holding directory");
    assert!(!w.path(".konedrive-holding").exists());
    assert_eq!(id_at(&w.path("papers")).as_deref(), Some("D"), "the local rename is left to the examination");

    let examined = w.scan_and_upload().await;
    let rows = w.store.with(|s| s.outbox_rows()).unwrap();
    assert!(!rows.iter().any(|r| r.kind.removes()), "{rows:?}: {:?}", examined.applied);
    assert_eq!(w.deletes(), 0, "nothing deleted in OneDrive");
    assert!(w.graph.with(|c| c.bin.is_empty()));
}

/// A placeholder moved out of the folder, changed in
/// OneDrive before the examination saw the move, is not placed again by the
/// reconcile — placed again, the examination would find the item at its place
/// and never make the `move-out` row, and the object outside would read zeros
/// for good. With the move-out step in the loop the object is downloaded where it went, its
/// delete meets OneDrive's change and is dropped, and only then is the item
/// placed again in the folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_placeholder_moved_out_and_changed_in_onedrive_is_downloaded_where_it_went() {
    let w = world().await;
    let listing = w.listed().await;
    let outside = w.root.path.parent().unwrap().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::rename(w.path("top.txt"), outside.join("top.txt")).unwrap();
    w.graph.with(|c| c.edit("T", b"top, changed"));
    let report = w.cycle(&listing).await;
    assert!(!w.path("top.txt").exists(), "not placed while its object is alive outside");
    assert!(report.applied.unsettled.contains("T"));
    assert!(w.examined.lock().unwrap().iter().map(names).collect::<String>().contains("top.txt"), "handed to the examination");

    w.scan_and_upload().await;
    assert_eq!(std::fs::read(outside.join("top.txt")).unwrap(), b"top, changed", "downloaded where it went, never zeros");
    assert!(xattr::get(outside.join("top.txt"), XATTR_ITEM_ID).unwrap().is_none(), "the user's own file now");
    assert_eq!(w.deletes(), 1, "one DELETE, answered 412: OneDrive's change wins");
    assert!(w.graph.with(|c| c.item("T").is_some() && c.bin.is_empty()), "nothing deleted in OneDrive");
    assert!(w.store.with(|s| s.outbox_rows()).unwrap().is_empty());

    w.cycle(&listing).await;
    assert_eq!(id_at(&w.path("top.txt")).as_deref(), Some("T"), "placed again once the examination decided");
    assert_eq!(state_at(&w.path("top.txt")), Some(State::OnlineOnly));
}

/// C1: what a stop or a crash left in the holding directory goes back into
/// the folder at the next Full reconcile: where the tree has it, or — held by
/// a local change — where the base has it. Nothing is rescued out of the
/// folder, and nothing is deleted in OneDrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_a_stop_left_in_the_holding_directory_goes_back_into_the_folder() {
    let w = world().await;
    w.listed().await;
    std::fs::create_dir(w.path(".konedrive-holding")).unwrap();
    std::fs::rename(w.path("docs/f.txt"), w.path(".konedrive-holding/F")).unwrap();
    std::fs::rename(w.path("top.txt"), w.path(".konedrive-holding/T")).unwrap();
    w.row(OutboxKind::Update, "T", "top.txt");
    // A restart: the first cycle is Full.
    let report = w.cycle(&w.listing_with(None)).await;
    assert!(report.full);
    assert!(report.applied.rescued.is_empty(), "moved out of the folder: {:?}", report.applied.rescued);
    assert_eq!(id_at(&w.path("docs/f.txt")).as_deref(), Some("F"), "placed from the holding directory");
    assert_eq!(id_at(&w.path("top.txt")).as_deref(), Some("T"), "put back where the base has it");
    assert!(!w.path(".konedrive-holding").exists());

    w.store.with(|s| s.outbox_drop_all()).unwrap();
    let examined = w.scan_and_upload().await;
    assert!(examined.applied.queued.is_empty(), "{:?}", w.store.with(|s| s.outbox_rows()).unwrap());
    assert_eq!(w.deletes(), 0, "nothing deleted in OneDrive");
}

/// I1: OneDrive changes a file right after an upload's commit, within one
/// fetch; the guard reads it again, but the file is open, so the replacement
/// waits. The change waits too — the cursor never sends it again — and lands
/// once the file is closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_read_again_survives_a_replacement_that_waits() {
    let w = world().await;
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    w.cycle(&listing).await;
    let stale = w.graph.with(|c| c.delta_body());
    w.graph.with(|c| c.script("GET", "root/delta", ResponseTemplate::new(200).set_body_json(stale).set_delay(Duration::from_millis(800)), 1));
    let cycle = {
        let listing = Arc::clone(&listing);
        tokio::spawn(async move { listing.cycle(&CancellationToken::new()).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    w.commit_upload("F", "docs/f.txt", b"mine").await;
    w.graph.with(|c| c.edit("F", b"theirs"));
    let writer = std::fs::OpenOptions::new().write(true).open(w.path("docs/f.txt")).unwrap();
    cycle.await.unwrap().unwrap();
    listing.join_replacements().await;
    assert!(w.deferred("F").is_some(), "OneDrive's change waits");
    w.cycle(&listing).await;
    assert!(w.deferred("F").is_some(), "and keeps waiting while the file is open");
    drop(writer);
    w.cycle(&listing).await;
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"theirs");
    assert_eq!(w.base("F").unwrap().ctag.as_deref(), Some(w.cloud_ctag("F").as_str()));
    assert!(w.deferred("F").is_none());
}

/// I3: `docs` is deleted here (a live `delete` row), and OneDrive moves
/// `top.txt` into it. The move is not the folder's: the file stays where it
/// is and its move waits, and no row takes OneDrive's item back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_item_moved_in_onedrive_into_a_folder_deleted_here_is_not_moved_back() {
    let w = world().await;
    let listing = w.listed().await;
    std::fs::remove_dir_all(w.path("docs")).unwrap();
    let mut batch = Batch::new();
    batch.name(Path::new(""), OsStr::new("docs"));
    w.examine(batch).await;
    assert!(w.store.with(|s| s.outbox_rows()).unwrap().iter().any(|r| r.kind == OutboxKind::Delete && r.item_id.as_deref() == Some("D")));
    w.graph.with(|c| c.rename("T", "D", "top.txt"));
    w.cycle(&listing).await;
    assert!(w.path("top.txt").exists());
    assert_eq!(w.base("T").unwrap().parent_id.as_deref(), Some(ROOT), "the base keeps it where the disk has it");
    assert!(w.deferred("T").is_some(), "OneDrive's move waits");
    let mut batch = Batch::new();
    batch.name(Path::new(""), OsStr::new("top.txt"));
    w.examine(batch).await;
    let rows = w.store.with(|s| s.outbox_rows()).unwrap();
    assert!(!rows.iter().any(|r| r.item_id.as_deref() == Some("T")), "a row that moves OneDrive's item back: {rows:?}");
}

/// A held delete outliving the item's own removal in OneDrive: `docs` is
/// deleted here, the mass-delete guard holds its row, and before it is
/// confirmed or restored, `docs` is deleted in OneDrive too (another
/// device). The next cycle's delta reports it gone: the held row has
/// nothing left to delete, so it is dropped without a request, `HeldCount`
/// goes back to 0, and the worker is woken to say so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_delete_of_an_item_already_deleted_in_onedrive_is_dropped() {
    let w = world().await;
    let listing = w.listed().await;
    std::fs::remove_dir_all(w.path("docs")).unwrap();
    let mut batch = Batch::new();
    batch.name(Path::new(""), OsStr::new("docs"));
    w.examine(batch).await;
    let seq = w.store.with(|s| s.outbox_rows()).unwrap().into_iter().find(|r| r.item_id.as_deref() == Some("D")).unwrap().seq;
    // The mass-delete guard's decision, without tripping its threshold.
    w.store.with(|s| s.outbox_set_state(seq, OutboxState::Held, Some("mass-delete"), None)).unwrap();
    assert_eq!(OutboxWorker::new(w.config()).counts().unwrap().held, 1);

    // `docs` is deleted in OneDrive too, from another device.
    w.graph.with(|c| c.trash("D"));
    w.cycle(&listing).await;

    let rows = w.store.with(|s| s.outbox_rows()).unwrap();
    assert!(rows.is_empty(), "the held delete has nothing left to delete: {rows:?}");
    assert_eq!(OutboxWorker::new(w.config()).counts().unwrap().held, 0);
    assert_eq!(w.deletes(), 0, "never sent to OneDrive");
    let dropped = w.dropped.lock().unwrap().clone();
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].item_id.as_deref(), Some("D"));
}

/// §5, §6 echo, with the delta ahead of the commit: an upload landed, and the
/// worker stopped before its commit (its row stays `running`). The cycle
/// meanwhile brings the new version: it is not downloaded over the file, nor
/// is the new file taken for a create/create conflict. The replay adopts it,
/// and the next cycle changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delta_that_brings_an_upload_before_its_commit_changes_nothing() {
    let w = world().await;
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    std::thread::sleep(Duration::from_millis(10));
    std::fs::OpenOptions::new().append(true).open(w.path("docs/f.txt")).unwrap().write_all(b" and mine").unwrap();
    std::fs::write(w.path("docs/new.txt"), b"new").unwrap();
    let mut batch = Batch::new();
    batch.written(Path::new("docs"), OsStr::new("f.txt"), None);
    batch.name(Path::new("docs"), OsStr::new("new.txt"));
    assert_eq!(w.examine(batch).await.applied.queued.len(), 2);
    let (f_ino, new_ino) = (std::fs::metadata(w.path("docs/f.txt")).unwrap().ino(), std::fs::metadata(w.path("docs/new.txt")).unwrap().ino());

    // Both rows are sent (at once: they are content rows), and the worker
    // stops before committing either.
    let engine = Arc::new(Engine::new(w.config()));
    engine.arm(crate::sync::upload::Fault::AfterSend);
    engine.arm(crate::sync::upload::Fault::AfterSend);
    engine.drain(&CancellationToken::new()).await;
    assert!(w.graph.with(|c| c.at("docs/new.txt").is_some()) && w.graph.with(|c| c.item("F").unwrap().content == b"one and mine"));
    let rows = w.store.with(|s| s.outbox_rows()).unwrap();
    assert!(rows.iter().all(|r| r.state == OutboxState::Running), "{rows:?}");

    let report = w.cycle(&listing).await;
    assert!(report.applied.replacements.is_empty() && report.applied.copies.is_empty(), "{report:?}");
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"one and mine");
    assert_eq!(id_at(&w.path("docs/new.txt")), None, "still the outbox's to commit");

    w.upload().await;
    assert!(w.store.with(|s| s.outbox_rows()).unwrap().is_empty());
    let report = w.cycle(&listing).await;
    assert!(report.applied.replacements.is_empty() && report.applied.copies.is_empty() && report.applied.changes.is_empty(), "{report:?}");
    assert_eq!(std::fs::metadata(w.path("docs/f.txt")).unwrap().ino(), f_ino);
    assert_eq!(std::fs::metadata(w.path("docs/new.txt")).unwrap().ino(), new_ino);
    assert!(id_at(&w.path("docs/new.txt")).is_some());
    assert!(w.store.with(|s| s.deferred_ids()).unwrap().is_empty());
    assert_eq!(w.graph.with(|c| c.paths()).len(), 4, "no copy in OneDrive: {:?}", w.graph.with(|c| c.paths()));
}
