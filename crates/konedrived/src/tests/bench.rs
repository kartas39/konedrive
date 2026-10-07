//! The outbox and the cloud side at scale: a bench,
//! never run by `cargo test`. Each
//! operation is an ignored test on a temporary store and a temporary folder;
//! it prints its time and fails when its budget is exceeded:
//!
//! ```text
//! cargo test -p konedrived --release --lib bench:: -- --ignored --nocapture --test-threads 1
//! ```
//!
//! The sizes are the issues': 30 000 queued changes, 100 000 items, a
//! directory of 30 000 entries of which 27 000 are new files; a delta of
//! 30 000 changed files, 5 000 skipped files, 2 000 conflicts, 20 000 images
//! without thumbnails and 30 000 transfers waiting for a slot.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use konedrive_fs::handle::FileHandle;

use crate::local::testing::Folder;
use crate::local::Batch;
use crate::folder::root::SyncRoot;
use crate::upload::tests::harness::Harness;
use crate::folder::locks::InodeLocks;
use konedrive_tree::outbox::{Committed, Detection, Inode, OutboxKind, OutboxOp, OutboxRow, OutboxState, Reason};
use konedrive_tree::{Change, Kind, Placement, Row, Store, TreeStore};

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
        reason: reason.map(Reason::parse),
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
    store.begin_staging(konedrive_tree::NewTree::Whole).unwrap();
    store.stage(&all).unwrap();
    store.commit_staging("link-1").unwrap();
    Store::new(store)
}

/// `n` new files named `prefix` and a number, in `dir` of `folder`.
fn write_files(folder: &Folder, dir: &str, prefix: &str, n: usize) {
    std::fs::create_dir_all(folder.path(dir)).unwrap();
    for i in 0..n {
        std::fs::write(folder.path(&format!("{dir}/{prefix}{i:05}")), b"x").unwrap();
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
    let folder = Folder::on_disk(&changes);
    // 3 001 rows already queued: a new directory of 3 000 files.
    write_files(&folder, "old", "o", 3000);
    let mut first = Batch::new();
    first.name(Path::new(""), std::ffi::OsStr::new("old"));
    timed("examination of a new directory of 3 000 files (setup)", || folder.examine(&first));
    write_files(&folder, "big", "n", 27000);
    let mut batch = Batch::new();
    batch.dir(Path::new("big"));
    let (examined, took) = timed("examination of big/: 30 000 entries, 27 000 new", || folder.examine(&batch));
    assert_eq!(examined.applied.queued.len(), 27000);
    assert_eq!(folder.rows().len(), 30001);
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
    let (folder, _) = timed("placing 100 000 items (setup)", || Folder::on_disk(&changes));
    write_files(&folder, "new", "n", 30000);
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
    folder.store.call_blocking(move |s| s.bench_insert(&rows)).unwrap();
    let (_, took) = timed("Full local scan: 100 000 items, 30 000 rows", || folder.examine(&Batch::full()));
    assert_eq!(folder.rows().len(), 30001);
    drop(folder.dir);
    within("the Full local scan", took, Duration::from_secs(10));
}

/// A worker on a store whose outbox is `rows`, in `seq` order.
fn engine_with(rows: &[OutboxRow]) -> (tempfile::TempDir, Harness, Arc<crate::upload::Engine>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("OneDrive");
    std::fs::create_dir(&path).unwrap();
    let root = SyncRoot { path, root_id: "5b0e2c7a-1d3f-4e8a-9b6c-0f1e2d3c4b5a".into() };
    let store = store_at(dir.path(), &[]);
    let rows = rows.to_vec();
    store.call_blocking(move |s| s.bench_insert(&rows)).unwrap();
    let harness = Harness::new(&root, &store, &InodeLocks::new());
    let engine = harness.engine();
    (dir, harness, engine)
}

/// What the worker would take next.
fn pick(h: &Harness, engine: &crate::upload::Engine) -> usize {
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
        store.call_blocking(move |s| s.bench_insert(&rows)).unwrap();
        let _ = &mut store;
        for pick in [n / 2, n / 2 + 1, n / 2 + 2] {
            let seq = pick as i64 + 1;
            let inode = object(pick);
            let answer = item(&format!("N{pick}"), Some("R"), &format!("f{pick:06}"), Kind::File);
            let (_, took) = timed(&format!("claim, locate and commit one row of {n}"), || {
                let claimed = store.call_blocking(move |s| s.outbox_claim(seq, OutboxState::Ready)).unwrap().unwrap();
                let located = store.call_blocking(move |s| s.outbox_for_inode(claimed.inode.as_ref().unwrap())).unwrap();
                assert_eq!(located.len(), 1);
                store.call_blocking(move |s| s.outbox_commit(seq, Committed::Item { row: &answer, handle: inode.handle.as_ref() }, None)).unwrap();
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
                1..=4 => (OutboxState::Ready, Some(Reason::WaitingForSpace.key())),
                _ => (OutboxState::Ready, None),
            };
            new_row(OutboxKind::Create, &format!("d/f{i:06}"), Some("R"), object(i), state, reason)
        })
        .collect()
}

/// `NotUploadedSummary()` as the daemon sums it when it has no sum in
/// memory: through the store's read-only connection.
fn summary_now(store: &Store, _root: &Path) -> Vec<crate::upload::kept_back::SummaryRow> {
    let (skipped, groups) = store.read_blocking(|s| Ok((s.skipped_groups()?, s.outbox_groups()?))).unwrap();
    crate::upload::kept_back::summary(&skipped, &groups, false)
}

/// A D-Bus count or the Not Uploaded summary while an `outbox_apply` of 30 000 runs.
#[test]
#[ignore]
fn the_summary_answers_while_an_apply_holds_the_store() {
    guard();
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(dir.path(), &[]);
    store.call_blocking(move |s| s.bench_insert(&mixed_rows()[..3000])).unwrap();
    let ops: Vec<OutboxOp> = (0..30_000u64).map(|i| OutboxOp::Record(create_detection(&format!("new/f{i:06}"), 1_000_000 + i))).collect();
    let applying = store.clone();
    let apply = std::thread::spawn(move || timed("outbox_apply of 30 000 new files", || applying.call_blocking(move |s| s.outbox_apply(&ops, TIME)).unwrap()));
    std::thread::sleep(Duration::from_millis(100));
    let root = Path::new("/nowhere/OneDrive");
    let (summary, took) = timed("the Not Uploaded summary during the apply", || summary_now(&store, root));
    assert!(!summary.is_empty());
    let (applied, _) = apply.join().unwrap();
    assert_eq!(applied.queued.len(), 30_000);
    within("the summary during an apply", took, Duration::from_millis(100));
}

/// `Changes(21)` and `NotUploadedFiles(reason, 20)` with 30 000 rows.
#[test]
#[ignore]
fn the_first_rows_and_files_of_a_reason() {
    guard();
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(dir.path(), &[]);
    store.call_blocking(move |s| s.bench_insert(&mixed_rows())).unwrap();
    let root = Path::new("/nowhere/OneDrive");
    let (entries, first) = timed("Changes(21) of 30 000", || {
        let rows = store.read_blocking(|s| s.outbox_first(21)).unwrap();
        crate::sync::outbox::entries(rows, root, &[], false, false)
    });
    assert_eq!(entries.len(), 21);
    let ((files, total), second) = timed("NotUploadedFiles(name-characters, 20) of 30 000", || {
        store.read_blocking(|s| crate::upload::kept_back::files(s, root, false, "name-characters", 20)).unwrap()
    });
    assert_eq!((files.len(), total), (20, 1000));
    within("Changes(21)", first, Duration::from_millis(50));
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
    store.call_blocking(move |s| s.bench_insert(&many_rows())).unwrap();
    let handle = object(77_777).handle.unwrap();
    let (found, took) = timed("outbox_by_handle among 100 000", || store.call_blocking(move |s| s.outbox_by_handle(&handle)).unwrap());
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
    store.call_blocking(move |s| s.bench_insert(&many_rows())).unwrap();
    let rebase = [OutboxOp::Rebase { from: "moving".into(), to: "moved/here".into() }];
    let (_, took) = timed("rebase of 1 000 rows among 100 000", || store.call_blocking(move |s| s.outbox_apply(&rebase, TIME)).unwrap());
    let under = store.call_blocking(move |s| s.outbox_under(Path::new("moved/here"))).unwrap().len();
    assert_eq!(under, 1000);
    within("a rebase", took, Duration::from_millis(100));
}

/// What the rarely run whole-table reads cost at 30 000 rows (no budget).
#[test]
#[ignore]
fn a_whole_table_read() {
    guard();
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(dir.path(), &[]);
    store.call_blocking(move |s| s.bench_insert(&mixed_rows())).unwrap();
    let (rows, _) = timed("every row of 30 000", || store.call_blocking(move |s| s.outbox_rows()).unwrap());
    assert_eq!(rows.len(), 30_000);
}

// The cloud side.

/// 100 000 items: 100 folders of 1 000 files, every fifth file an image —
/// 20 000 images — and every twentieth skipped for its name — 5 000.
fn big_tree() -> Vec<Change> {
    let mut changes = Vec::with_capacity(100_100);
    for d in 0..100 {
        changes.push(Change::Upsert(item(&format!("D{d:02}"), Some("R"), &format!("d{d:02}"), Kind::Folder)));
        for i in 0..1000 {
            let mut row = item(&format!("F{d:02}-{i:03}"), Some(&format!("D{d:02}")), &format!("f{i:03}.jpg"), Kind::File);
            if i % 5 == 0 {
                row.mime = Some("image/jpeg".into());
            }
            if i % 20 == 1 {
                row.placement = Placement::Skipped(konedrive_tree::SkipReason::NameTooLong);
            }
            changes.push(Change::Upsert(row));
        }
    }
    changes
}

/// A store on disk holding [`big_tree`], every placed item with its local
/// object on record, as a folder placed in full has them.
fn big_store(dir: &Path) -> Store {
    let store = store_at(dir, &big_tree());
    store
        .call_blocking(|s| s.bench_sql("UPDATE items SET local_handle = CAST(id AS BLOB) WHERE placement = 'placed' AND id != 'R'"))
        .unwrap();
    store
}

/// `n` files of the tree, changed in OneDrive: a new version each.
fn changed_files(n: usize) -> Vec<Change> {
    (0..n)
        .map(|k| {
            let (d, i) = (k % 100, (k / 100) % 1000);
            let mut row = item(&format!("F{d:02}-{i:03}"), Some(&format!("D{d:02}")), &format!("f{i:03}.jpg"), Kind::File);
            row.ctag = Some(format!("c2-{d}-{i}"));
            row.etag = Some(format!("e2-{d}-{i}"));
            row.size = 7;
            if i % 5 == 0 {
                row.mime = Some("image/jpeg".into());
            }
            if i % 20 == 1 {
                row.placement = Placement::Skipped(konedrive_tree::SkipReason::NameTooLong);
            }
            Change::Upsert(row)
        })
        .collect()
}

/// What the reconcile asks the store of each changed id (`Materializer::changed`):
/// where it is and was, and its row in both trees.
fn materializer_reads(store: &Store, ids: &[String]) {
    use konedrive_tree::Table;
    for id in ids {
        let id = id.clone();
        let (a, b, c) = (id.clone(), id.clone(), id.clone());
        store.call_blocking(move |s| s.locate(Table::Staging, &a)).unwrap();
        store.call_blocking(move |s| s.locate(Table::Items, &b)).unwrap();
        store.call_blocking(move |s| s.get(Table::Items, &c)).unwrap();
        store.call_blocking(move |s| s.get(Table::Staging, &id)).unwrap();
    }
}

/// A read-only folder's delta cycle, its store work only (`Listing::sync_once`
/// and `reconcile`): stage, what changed, the reconcile's reads, the swap,
/// the counts.
fn read_only_cycle(store: &Store, changes: Vec<Change>) {
    store
        .call_blocking(move |s| {
            s.begin_staging(konedrive_tree::NewTree::Delta)?;
            s.stage(&changes)
        })
        .unwrap();
    let ids = store.call_blocking(|s| s.changed_ids()).unwrap();
    materializer_reads(store, &ids);
    store.call_blocking(|s| s.commit_staging("link-2")).unwrap();
    store.call_blocking(|s| s.counts()).unwrap();
}

/// A read-write folder's delta cycle, its store work only (`remote::listing::rw`).
fn read_write_cycle(store: &Store, changes: Vec<Change>) {
    let staged = store.call_blocking(move |s| s.stage_rw(&changes, 0, false)).unwrap();
    let Some(konedrive_tree::reconcile::RwStaged { ids, consumed }) = staged else { return };
    store
        .call_blocking(|s| crate::remote::materialize::Rw::read(s, "bench".into(), false, crate::local::IgnoreList::default()))
        .unwrap();
    materializer_reads(store, &ids);
    let changed = store.call_blocking(|s| s.changed_ids()).unwrap();
    assert!(changed.len() <= ids.len());
    store.call_blocking(move |s| s.commit_staging_deferring("link-2", &konedrive_tree::reconcile::Deferrals { consumed: &consumed, whole: &[], content: &[], fetched_at: 0, waits: &[] })).unwrap();
    store.call_blocking(|s| s.outbox_drop_removed()).unwrap();
    store.call_blocking(|s| s.counts()).unwrap();
}

/// A delta changing 10 files of 100 000 items, both kinds of folder.
#[test]
#[ignore]
fn a_delta_cycle_changing_10_files() {
    guard();
    let dir = tempfile::tempdir().unwrap();
    let store = big_store(dir.path());
    let (_, read_only) = timed("delta cycle, 10 of 100 000 changed, read-only folder", || read_only_cycle(&store, changed_files(10)));
    let dir = tempfile::tempdir().unwrap();
    let store = big_store(dir.path());
    let (_, read_write) = timed("delta cycle, 10 of 100 000 changed, read-write folder", || read_write_cycle(&store, changed_files(10)));
    within("a delta of 10, read-only", read_only, Duration::from_millis(200));
    within("a delta of 10, read-write", read_write, Duration::from_millis(200));
}

/// A delta changing 30 000 files of 100 000 items: the store's work, not the
/// downloads.
#[test]
#[ignore]
fn a_delta_cycle_changing_30000_files() {
    guard();
    let dir = tempfile::tempdir().unwrap();
    let store = big_store(dir.path());
    let (_, read_only) = timed("delta cycle, 30 000 of 100 000 changed, read-only folder", || read_only_cycle(&store, changed_files(30_000)));
    let dir = tempfile::tempdir().unwrap();
    let store = big_store(dir.path());
    let (_, read_write) = timed("delta cycle, 30 000 of 100 000 changed, read-write folder", || read_write_cycle(&store, changed_files(30_000)));
    within("a delta of 30 000, read-only", read_only, Duration::from_secs(5));
    within("a delta of 30 000, read-write", read_write, Duration::from_secs(5));
}

/// A read-write cycle with nothing new: 100 000 items, 30 000 changes queued.
#[test]
#[ignore]
fn an_idle_read_write_cycle() {
    guard();
    let dir = tempfile::tempdir().unwrap();
    let store = big_store(dir.path());
    store.call_blocking(move |s| s.bench_insert(&mixed_rows())).unwrap();
    let (staged, took) = timed("idle read-write cycle, 100 000 items, 30 000 queued", || store.call_blocking(|s| s.stage_rw(&[], 0, false)).unwrap());
    assert!(staged.is_none(), "nothing to do");
    within("an idle read-write cycle", took, Duration::from_millis(50));
}

/// A batch of 200 thumbnails to make, half-way through 20 000 images.
#[test]
#[ignore]
fn a_thumbnail_batch() {
    guard();
    let dir = tempfile::tempdir().unwrap();
    let store = big_store(dir.path());
    // The first 10 000 images (by id) have their thumbnails.
    store
        .call_blocking(|s| {
            s.bench_sql(
                "UPDATE items SET thumb_key = ctag || '|' || (SELECT p.name FROM items p WHERE p.id = items.parent_id) || '/' || name || '|' || mtime
                  WHERE mime = 'image/jpeg' AND id < 'F50'",
            )
        })
        .unwrap();
    // A drain half-way: its next batch goes on from where the last stopped.
    let (konedrive_tree::ThumbnailBatch { wanted: batch, next }, took) = timed("one thumbnail batch of 200, 10 000 of 20 000 made, going on", || {
        store.call_blocking(|s| s.thumbnail_candidates("F49-999", 200)).unwrap()
    });
    assert_eq!(batch.len(), 200);
    assert!(next.is_some());
    assert!(batch.iter().all(|wanted| wanted.row.id.as_str() >= "F50"), "those made are not made again");
    // A new drain starts from the beginning: one call looks at so many.
    let (konedrive_tree::ThumbnailBatch { wanted: batch, next }, from_start) =
        timed("one thumbnail batch, 10 000 of 20 000 made, from the start", || store.call_blocking(|s| s.thumbnail_candidates("", 200)).unwrap());
    assert!(batch.is_empty() && next.is_some(), "those made are passed over, a call at a time");
    within("a thumbnail batch", took.max(from_start), Duration::from_millis(100));
}

/// `Skipped()` with 5 000 skipped files among 100 000 items.
#[test]
#[ignore]
fn the_skipped_list() {
    guard();
    let dir = tempfile::tempdir().unwrap();
    let store = big_store(dir.path());
    let (skipped, took) = timed("Skipped() of 5 000", || store.call_blocking(|s| s.skipped()).unwrap());
    assert_eq!(skipped.len(), 5000);
    within("Skipped()", took, Duration::from_millis(100));
}

/// The conflicts looked over at the end of a cycle: 2 000, every file there.
#[test]
#[ignore]
fn the_conflicts_at_the_end_of_a_cycle() {
    guard();
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(dir.path(), &[]);
    let rescued = dir.path().join("rescued");
    std::fs::create_dir(&rescued).unwrap();
    let rows: Vec<konedrive_tree::ConflictRow> = (0..2000)
        .map(|i| {
            let file = rescued.join(format!("c{i:04}.txt"));
            std::fs::write(&file, b"x").unwrap();
            konedrive_tree::ConflictRow {
                at: TIME + i,
                original: format!("/nowhere/OneDrive/c{i:04}.txt"),
                rescued: file.display().to_string(),
                kind: konedrive_tree::ConflictKind::Rescued,
            }
        })
        .collect();
    store.call_blocking(move |s| s.add_conflicts(&rows)).unwrap();
    let state = crate::status::snapshot::SyncStateHandle::new(crate::status::snapshot::SyncSnapshot::default());
    let activity = crate::status::activity::Activity::new(state.clone());
    activity.attach(store.clone(), Path::new("/nowhere/OneDrive"));
    let (_, took) = timed("the conflicts looked over at the end of a cycle, 2 000", || activity.prune());
    assert_eq!(state.get().local.conflict_count, 2000);
    // No budget: `Conflicts.List()` on the bus looks at every one.
    let (all, _) = timed("Conflicts(), 2 000", || activity.conflicts().unwrap());
    assert_eq!(all.len(), 2000);
    within("the conflicts at the end of a cycle", took, Duration::from_millis(50));
}

/// A grant, a release, a waiter polled again and one given up, with 30 000
/// downloads and one upload waiting for a slot.
#[test]
#[ignore]
fn the_pool_with_30000_waiters() {
    use konedrive_graph::pool::{Class, TransferPool};
    use std::future::Future;
    use std::task::{Context, Poll};
    guard();
    let pool = TransferPool::starting_at(16, 32);
    let mut held: Vec<_> = (0..16).map(|_| pool.try_acquire(Class::Download).unwrap()).collect();
    let waker = futures_util::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut waiting: Vec<_> = (0..30_000)
        .map(|_| {
            let mut acquire = Box::pin(pool.acquire(Class::Download));
            assert!(acquire.as_mut().poll(&mut cx).is_pending());
            acquire
        })
        .collect();
    let mut upload = Box::pin(pool.acquire(Class::Upload));
    assert!(upload.as_mut().poll(&mut cx).is_pending());
    let mut worst = Duration::ZERO;
    // Downloads and uploads take turns: the upload behind 30 000 downloads first.
    let (_, took) = timed("a release that grants the upload, 30 000 waiting", || drop(held.pop()));
    worst = worst.max(took);
    let (slot, took) = timed("the upload takes its slot", || upload.as_mut().poll(&mut cx));
    let Poll::Ready(slot) = slot else { panic!("the upload was granted") };
    held.push(slot);
    worst = worst.max(took);
    let (_, took) = timed("a release that grants a download", || drop(held.remove(0)));
    worst = worst.max(took);
    let (slot, took) = timed("the download granted takes its slot", || waiting[0].as_mut().poll(&mut cx));
    let Poll::Ready(slot) = slot else { panic!("the first download was granted") };
    held.push(slot);
    worst = worst.max(took);
    let (_, took) = timed("the last waiter polled again", || assert!(waiting[29_999].as_mut().poll(&mut cx).is_pending()));
    worst = worst.max(took);
    let (_, took) = timed("a waiter in the middle gives up", || drop(waiting.remove(15_000)));
    worst = worst.max(took);
    // Every waiter in turn: a release, and the one granted takes its slot.
    let (_, drained) = timed("30 000 waiters granted one after another", || {
        for acquire in waiting.iter_mut().skip(1) {
            drop(held.remove(0));
            let Poll::Ready(slot) = acquire.as_mut().poll(&mut cx) else { panic!("granted in turn") };
            held.push(slot);
        }
    });
    println!("bench: of which one grant and release: {:.4} ms", drained.as_secs_f64() * 1000.0 / 29_998.0);
    within("a pool grant or release", worst, Duration::from_millis(1));
}

/// Recording where 100 000 placed items are, as a Full placement does.
#[test]
#[ignore]
fn recording_a_full_placement() {
    guard();
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(dir.path(), &big_tree());
    // As the reconcile records them: a batch at a time, one transaction each.
    let (_, took) = timed("recording 100 000 placed items", || {
        let mut placed = Vec::new();
        for n in 0..100_000u64 {
            let (d, i) = (n / 1000, n % 1000);
            placed.push((format!("F{d:02}-{i:03}"), object(n).handle.unwrap()));
            if placed.len() == crate::remote::materialize::PLACED_BATCH || n == 99_999 {
                let batch = std::mem::take(&mut placed);
                store.call_blocking(move |s| s.set_local_handles(&batch)).unwrap();
            }
        }
    });
    within("recording a full placement", took, Duration::from_secs(5));
}
