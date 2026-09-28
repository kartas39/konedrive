//! The outbox at scale (issue #38): a bench, never run by `cargo test`. Each
//! operation is an ignored test on a temporary store and a temporary folder;
//! it prints its time and fails when its budget is exceeded:
//!
//! ```text
//! cargo test -p konedrived --release --lib bench:: -- --ignored --nocapture --test-threads 1
//! ```
//!
//! The sizes are the issue's: 30 000 queued changes, 100 000 items, a
//! directory of 30 000 entries of which 27 000 are new files.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::XATTR_ROOT;
use tokio_util::sync::CancellationToken;

use crate::sync::disk::Disk;
use crate::sync::local::{Batch, Examined, Examiner, FakeLiveness, IgnoreList};
use crate::sync::materialize::{Materializer, Scope};
use crate::sync::root::SyncRoot;
use crate::sync::upload::fake::Harness;
use crate::sync::InodeLocks;
use crate::tree::outbox::{Committed, Detection, Inode, OutboxKind, OutboxOp, OutboxRow, OutboxState};
use crate::tree::{Change, Kind, Placement, Row, Store, TreeStore};

const TIME: i64 = 1_700_000_000;

/// Refuses to run unless the user's own places are out of reach: `HOME` and
/// the XDG state, config, data and cache directories inside the temporary
/// directory, and no session bus but a private one — so that nothing here,
/// nor a wrong binary run in its place, can touch the user's data.
fn guard() {
    let temp = std::env::temp_dir().canonicalize().unwrap();
    for var in ["HOME", "XDG_STATE_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME"] {
        let value = std::env::var_os(var).unwrap_or_else(|| panic!("the bench runs only with {var} set inside {}", temp.display()));
        let path = Path::new(&value).canonicalize().unwrap_or_else(|e| panic!("{var}: {e}"));
        assert!(path.starts_with(&temp) && path != temp, "the bench runs only with {var} inside {}, not {}", temp.display(), path.display());
    }
    let bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_default();
    assert!(!bus.contains("/run/user/"), "the bench runs only without the user's session bus ({bus})");
}

fn timed<T>(what: &str, f: impl FnOnce() -> T) -> (T, Duration) {
    guard();
    let start = Instant::now();
    let out = f();
    let took = start.elapsed();
    println!("bench: {what}: {:.1} ms", took.as_secs_f64() * 1000.0);
    (out, took)
}

fn within(what: &str, took: Duration, budget: Duration) {
    assert!(took <= budget, "{what} took {took:?}, over its budget of {budget:?}");
}

fn item(id: &str, parent: Option<&str>, name: &str, kind: Kind) -> Row {
    Row {
        id: id.into(),
        parent_id: parent.map(str::to_owned),
        name: name.into(),
        kind,
        size: 0,
        mtime: TIME,
        etag: Some(format!("e-{id}")),
        ctag: Some(format!("c-{id}")),
        quickxor: None,
        mime: None,
        placement: Placement::Placed,
    }
}

fn object(n: u64) -> Inode {
    let mut bytes = n.to_le_bytes().to_vec();
    bytes.extend_from_slice(b"bench");
    Inode { dev: 1, ino: n, handle: Some(FileHandle { kind: 1, bytes }) }
}

/// A row as an examination writes it for a new file or directory.
fn new_row(kind: OutboxKind, rel: &str, parent: Option<&str>, inode: Inode, state: OutboxState, reason: Option<&str>) -> OutboxRow {
    OutboxRow {
        seq: 0,
        kind,
        item_id: None,
        inode: Some(inode),
        rel: rel.into(),
        base: None,
        target_parent: parent.map(str::to_owned),
        target_name: Path::new(rel).file_name().map(|n| n.to_string_lossy().into_owned()),
        state,
        reason: reason.map(str::to_owned),
        attempts: 0,
        next_try: None,
        snapshot: None,
        session_url: None,
        session_expires: None,
        session_next: None,
        confirmed: false,
        size: None,
    }
}

fn create_detection(rel: &str, n: u64) -> Detection {
    Detection {
        kind: OutboxKind::Create,
        item_id: None,
        inode: Some(object(n)),
        rel: rel.into(),
        base: None,
        target_parent: Some("R".into()),
        target_name: Path::new(rel).file_name().map(|n| n.to_string_lossy().into_owned()),
        same_content: false,
        state: OutboxState::Ready,
        reason: None,
        next_try: None,
        size: None,
    }
}

/// A store on disk, in WAL mode as the daemon keeps it, whose base is the
/// root and `changes`.
fn store_at(dir: &Path, changes: &[Change]) -> Store {
    let mut all = vec![Change::Root(item("R", None, "", Kind::Folder))];
    all.extend_from_slice(changes);
    let mut store = TreeStore::open(&dir.join("tree.sqlite")).unwrap();
    store.begin_staging(false).unwrap();
    store.stage(&all).unwrap();
    store.commit_staging("link-1").unwrap();
    Store::new(store)
}

/// A OneDrive folder placed from `changes` by the real materializer, with its store on disk.
struct Folder {
    dir: tempfile::TempDir,
    root: SyncRoot,
    store: Store,
    liveness: FakeLiveness,
    locks: InodeLocks,
}

impl Folder {
    fn new(changes: &[Change]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("OneDrive");
        std::fs::create_dir(&path).unwrap();
        let root_id = "5b0e2c7a-1d3f-4e8a-9b6c-0f1e2d3c4b5a".to_owned();
        xattr::set(&path, XATTR_ROOT, root_id.as_bytes()).unwrap();
        let root = SyncRoot { path, root_id };
        let mut all = vec![Change::Root(item("R", None, "", Kind::Folder))];
        all.extend_from_slice(changes);
        let store = Store::new(TreeStore::open(&dir.path().join("tree.sqlite")).unwrap());
        store
            .with(|s| {
                s.begin_staging(false)?;
                s.stage(&all)
            })
            .unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let materializer = Materializer {
            disk: Disk::open(&root, false).unwrap(),
            store: store.clone(),
            link: None,
            runtime: runtime.handle().clone(),
            locks: InodeLocks::new(),
            root_item_id: "R".into(),
            rescue_into: dir.path().join("rescued"),
            cancel: CancellationToken::new(),
            rw: None,
            claimed: None,
        };
        materializer.apply(Scope::Full).unwrap();
        store.with(|s| s.commit_staging("link-1")).unwrap();
        Folder { dir, root, store, liveness: FakeLiveness::new(), locks: InodeLocks::new() }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path.join(rel)
    }

    fn examine(&self, batch: &Batch) -> Examined {
        let disk = Disk::open(&self.root, false).unwrap();
        let ignore = IgnoreList::default();
        Examiner { disk: &disk, store: &self.store, liveness: &self.liveness, ignore: &ignore, locks: &self.locks, now: TIME }.examine(batch).unwrap()
    }

    fn write_files(&self, dir: &str, prefix: &str, n: usize) {
        std::fs::create_dir_all(self.path(dir)).unwrap();
        for i in 0..n {
            std::fs::write(self.path(&format!("{dir}/{prefix}{i:05}")), b"x").unwrap();
        }
    }

    fn rows(&self) -> usize {
        self.store.with(|s| s.outbox_rows()).unwrap().len()
    }
}

/// An examination batch of one directory with 30 000 entries, 27 000 of
/// them new files, recorded: 30 000 rows after it.
#[test]
#[ignore]
fn examination_of_a_directory_with_27000_new_files() {
    guard();
    let mut changes = vec![Change::Upsert(item("D", Some("R"), "big", Kind::Folder))];
    changes.extend((0..3000).map(|i| Change::Upsert(item(&format!("K{i}"), Some("D"), &format!("k{i:05}"), Kind::File))));
    let folder = Folder::new(&changes);
    // 3 001 rows already queued: a new directory of 3 000 files.
    folder.write_files("old", "o", 3000);
    let mut first = Batch::new();
    first.name(Path::new(""), std::ffi::OsStr::new("old"));
    timed("examination of a new directory of 3 000 files (setup)", || folder.examine(&first));
    folder.write_files("big", "n", 27000);
    let mut batch = Batch::new();
    batch.dir(Path::new("big"));
    let (examined, took) = timed("examination of big/: 30 000 entries, 27 000 new", || folder.examine(&batch));
    assert_eq!(examined.applied.queued.len(), 27000);
    assert_eq!(folder.rows(), 30001);
    within("the examination batch", took, Duration::from_secs(3));
}

/// A Full local scan: 100 000 items, 30 000 rows, on tmpfs.
#[test]
#[ignore]
fn full_scan_of_100000_items_and_30000_rows() {
    guard();
    let mut changes = Vec::new();
    for d in 0..100 {
        changes.push(Change::Upsert(item(&format!("D{d}"), Some("R"), &format!("d{d:03}"), Kind::Folder)));
        changes.extend((0..1000).map(|i| Change::Upsert(item(&format!("F{d}-{i}"), Some(&format!("D{d}")), &format!("f{i:04}"), Kind::File))));
    }
    let (folder, _) = timed("placing 100 000 items (setup)", || Folder::new(&changes));
    folder.write_files("new", "n", 30000);
    let dir = std::fs::File::open(folder.path("new")).unwrap();
    let meta = |rel: &str| {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(folder.path(rel)).unwrap();
        (m.dev(), m.ino())
    };
    let (dev, ino) = meta("new");
    let top = std::fs::File::open(&folder.root.path).unwrap();
    let mut rows = vec![new_row(
        OutboxKind::Mkdir,
        "new",
        Some("R"),
        Inode { dev, ino, handle: FileHandle::at(&top, std::ffi::OsStr::new("new")).ok() },
        OutboxState::Ready,
        None,
    )];
    for i in 0..30000 {
        let name = format!("n{i:05}");
        let (dev, ino) = meta(&format!("new/{name}"));
        let handle = FileHandle::at(&dir, std::ffi::OsStr::new(&name)).ok();
        rows.push(new_row(OutboxKind::Create, &format!("new/{name}"), None, Inode { dev, ino, handle }, OutboxState::Ready, None));
    }
    folder.store.with(|s| s.bench_insert(&rows)).unwrap();
    let (_, took) = timed("Full local scan: 100 000 items, 30 000 rows", || folder.examine(&Batch::full()));
    assert_eq!(folder.rows(), 30001);
    drop(folder.dir);
    within("the Full local scan", took, Duration::from_secs(10));
}

/// A worker on a store whose outbox is `rows`, in `seq` order.
fn engine_with(rows: &[OutboxRow]) -> (tempfile::TempDir, Harness, Arc<crate::sync::upload::Engine>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("OneDrive");
    std::fs::create_dir(&path).unwrap();
    let root = SyncRoot { path, root_id: "5b0e2c7a-1d3f-4e8a-9b6c-0f1e2d3c4b5a".into() };
    let store = store_at(dir.path(), &[]);
    store.with(|s| s.bench_insert(rows)).unwrap();
    let harness = Harness::new(&root, &store, &InodeLocks::new());
    let engine = harness.engine();
    (dir, harness, engine)
}

/// What the worker would take next.
fn pick(h: &Harness, engine: &crate::sync::upload::Engine) -> usize {
    h.block_on(engine.candidates()).unwrap().len()
}

/// Picking the next rows to run: 100 000 rows.
#[test]
#[ignore]
fn picking_among_100000_rows() {
    guard();
    let rows: Vec<OutboxRow> =
        (0..100_000).map(|i| new_row(OutboxKind::Create, &format!("f{i:06}"), Some("R"), object(i), OutboxState::Ready, None)).collect();
    let (_dir, h, engine) = engine_with(&rows);
    let (_, took) = timed("picking the next rows among 100 000", || pick(&h, &engine));
    within("picking", took, Duration::from_millis(50));
}

/// The same, worst case: every row waits on one row at the end of the queue.
#[test]
#[ignore]
fn picking_when_every_row_waits_on_the_last() {
    guard();
    let mut rows: Vec<OutboxRow> =
        (0..99_999).map(|i| new_row(OutboxKind::Create, &format!("big/f{i:06}"), None, object(i), OutboxState::Ready, None)).collect();
    rows.push(new_row(OutboxKind::Mkdir, "big", Some("R"), object(1_000_000), OutboxState::Ready, None));
    let (_dir, h, engine) = engine_with(&rows);
    let (picked, took) = timed("picking when 99 999 rows wait on the last", || pick(&h, &engine));
    assert!(picked >= 1, "the mkdir runs");
    within("picking, worst case", took, Duration::from_millis(200));
}

/// One upload step's own work — claim, locate, commit — not the transfer, as
/// the rows grow.
#[test]
#[ignore]
fn one_steps_own_work_is_flat() {
    guard();
    let mut worst = Duration::ZERO;
    for n in [3_000u64, 30_000] {
        let dir = tempfile::tempdir().unwrap();
        let mut store = store_at(dir.path(), &[]);
        let rows: Vec<OutboxRow> =
            (0..n).map(|i| new_row(OutboxKind::Create, &format!("f{i:06}"), Some("R"), object(i), OutboxState::Ready, None)).collect();
        store.with(|s| s.bench_insert(&rows)).unwrap();
        let _ = &mut store;
        for pick in [n / 2, n / 2 + 1, n / 2 + 2] {
            let seq = pick as i64 + 1;
            let inode = object(pick);
            let answer = item(&format!("N{pick}"), Some("R"), &format!("f{pick:06}"), Kind::File);
            let (_, took) = timed(&format!("claim, locate and commit one row of {n}"), || {
                let claimed = store.with(|s| s.outbox_claim(seq, OutboxState::Ready)).unwrap().unwrap();
                let located = store.with(|s| s.outbox_for_inode(claimed.inode.as_ref().unwrap())).unwrap();
                assert_eq!(located.len(), 1);
                store.with(|s| s.outbox_commit(seq, Committed::Item { row: &answer, handle: inode.handle.as_ref() }, None)).unwrap();
            });
            worst = worst.max(took);
        }
    }
    within("one step's own work", worst, Duration::from_millis(10));
}

/// 30 000 rows: 25 000 queued, 4 000 waiting for space, 1 000 names refused.
fn mixed_rows() -> Vec<OutboxRow> {
    (0..30_000u64)
        .map(|i| {
            let (state, reason) = match i % 30 {
                0 => (OutboxState::Blocked, Some("name-characters")),
                1..=4 => (OutboxState::Ready, Some(crate::sync::upload::space::WAITING)),
                _ => (OutboxState::Ready, None),
            };
            new_row(OutboxKind::Create, &format!("d/f{i:06}"), Some("R"), object(i), state, reason)
        })
        .collect()
}

/// `NotUploadedSummary()` as the daemon answers it.
fn summary_now(store: &Store, _root: &Path) -> Vec<crate::sync::kept_back::SummaryRow> {
    let (skipped, groups) = store.with(|s| Ok((s.skipped_groups()?, s.outbox_groups()?))).unwrap();
    crate::sync::kept_back::summary(&skipped, &groups, false)
}

/// A D-Bus count or the Not Uploaded summary while an `outbox_apply` of 30 000 runs.
#[test]
#[ignore]
fn the_summary_answers_while_an_apply_holds_the_store() {
    guard();
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(dir.path(), &[]);
    store.with(|s| s.bench_insert(&mixed_rows()[..3000])).unwrap();
    let ops: Vec<OutboxOp> = (0..30_000u64).map(|i| OutboxOp::Record(create_detection(&format!("new/f{i:06}"), 1_000_000 + i))).collect();
    let applying = store.clone();
    let apply = std::thread::spawn(move || timed("outbox_apply of 30 000 new files", || applying.with(|s| s.outbox_apply(&ops, TIME)).unwrap()));
    std::thread::sleep(Duration::from_millis(100));
    let root = Path::new("/nowhere/OneDrive");
    let (summary, took) = timed("the Not Uploaded summary during the apply", || summary_now(&store, root));
    assert!(!summary.is_empty());
    let (applied, _) = apply.join().unwrap();
    assert_eq!(applied.queued.len(), 30_000);
    within("the summary during an apply", took, Duration::from_millis(100));
}

/// `Outbox(21)` and `NotUploadedFiles(reason, 20)` with 30 000 rows.
#[test]
#[ignore]
fn the_first_rows_and_files_of_a_reason() {
    guard();
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(dir.path(), &[]);
    store.with(|s| s.bench_insert(&mixed_rows())).unwrap();
    let root = Path::new("/nowhere/OneDrive");
    let (entries, first) = timed("Outbox(21) of 30 000", || {
        let rows = store.with(|s| s.outbox_first(21)).unwrap();
        crate::sync::outbox_api::entries(rows, root, &[], false, false)
    });
    assert_eq!(entries.len(), 21);
    let ((files, total), second) = timed("NotUploadedFiles(name-characters, 20) of 30 000", || {
        store.with(|s| crate::sync::kept_back::files(s, root, false, "name-characters", 20)).unwrap()
    });
    assert_eq!((files.len(), total), (20, 1000));
    within("Outbox(21)", first, Duration::from_millis(50));
    within("NotUploadedFiles", second, Duration::from_millis(50));
}

/// 100 000 rows: 1 000 inside `moving/`, the rest elsewhere.
fn many_rows() -> Vec<OutboxRow> {
    (0..100_000u64)
        .map(|i| {
            let rel = if i % 100 == 0 { format!("moving/f{i:06}") } else { format!("d{}/f{i:06}", i % 7) };
            new_row(OutboxKind::Create, &rel, None, object(i), OutboxState::Ready, None)
        })
        .collect()
}

/// A watcher lookup by handle, among 100 000 rows.
#[test]
#[ignore]
fn a_lookup_by_handle() {
    guard();
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(dir.path(), &[]);
    store.with(|s| s.bench_insert(&many_rows())).unwrap();
    let handle = object(77_777).handle.unwrap();
    let (found, took) = timed("outbox_by_handle among 100 000", || store.with(|s| s.outbox_by_handle(&handle)).unwrap());
    assert!(found.is_some());
    within("a lookup by handle", took, Duration::from_millis(1));
}

/// A folder renamed (`rebase`), among 100 000 rows.
#[test]
#[ignore]
fn a_folder_renamed() {
    guard();
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(dir.path(), &[]);
    store.with(|s| s.bench_insert(&many_rows())).unwrap();
    let rebase = [OutboxOp::Rebase { from: "moving".into(), to: "moved/here".into() }];
    let (_, took) = timed("rebase of 1 000 rows among 100 000", || store.with(|s| s.outbox_apply(&rebase, TIME)).unwrap());
    let under = store.with(|s| s.outbox_under(Path::new("moved/here"))).unwrap().len();
    assert_eq!(under, 1000);
    within("a rebase", took, Duration::from_millis(100));
}

/// What the rarely run whole-table reads cost at 30 000 rows (no budget: the
/// limitations log says when they run).
#[test]
#[ignore]
fn a_whole_table_read() {
    guard();
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(dir.path(), &[]);
    store.with(|s| s.bench_insert(&mixed_rows())).unwrap();
    let (rows, _) = timed("every row of 30 000", || store.with(|s| s.outbox_rows()).unwrap());
    assert_eq!(rows.len(), 30_000);
}
