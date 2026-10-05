//! The watcher with a real notification group, unprivileged, in temporary
//! directories (`docs/design/writes.md` §12): batches after a quiet spell, renames
//! within, into and out of the folder, a new directory marked through the
//! helper and then scanned, the daemon's own events dropped by pid (a fill
//! included, §3.2), an overflow, the root going away, the degraded mode,
//! and one batch followed all the way to an outbox row. What is examined when
//! (the retries, the rechecks, the periodic scan) is tested with the time
//! given by hand, in `schedule/tests.rs` and `reader/timers/tests.rs`.
//!
//! Unless a test says otherwise the watcher drops nothing by pid, so the test
//! process itself can stand in for the user.

use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{self, XATTR_ROOT};

use super::*;
use crate::hydration::source::LocalDir;
use crate::local::scan::ScanReport;
use crate::local::liveness::NoLiveness;
use crate::local::IgnoreList;
use crate::helper::testing::FakeHelper;
use crate::remote::testing::{Options, Says, Step, World};
use konedrive_tree::outbox::OutboxKind;
use konedrive_tree::{Change, Kind, Placement, Row};

const WAIT: Duration = Duration::from_secs(10);

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

struct Fx {
    dir: tempfile::TempDir,
    root: SyncRoot,
    /// Beside the folder, on its filesystem: "outside".
    outside: PathBuf,
    runtime: tokio::runtime::Runtime,
}

impl Fx {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let path = base.join("OneDrive");
        let outside = base.join("elsewhere");
        std::fs::create_dir(&path).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let root_id = "4c1f0b2e-7d6a-4f3b-9e8d-2a1b0c9d8e7f".to_owned();
        xattr::set(&path, XATTR_ROOT, root_id.as_bytes()).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        Self { dir, root: SyncRoot { path, root_id }, outside, runtime }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path.join(rel)
    }

    fn config(&self) -> WatchConfig {
        self.config_for(&self.root)
    }

    /// The watcher of `root`, with no helper, the tests' clocks, and nothing
    /// dropped by pid.
    fn config_for(&self, root: &SyncRoot) -> WatchConfig {
        let mut config = WatchConfig::new(root.clone(), crate::helper::LinkCell::default(), self.runtime.handle().clone());
        config.own_pid = None;
        config.timing = timing();
        config
    }

    /// A watcher whose batches come out of the receiver, the bring-up's
    /// Full local scan already taken off it.
    fn start(&self, config: WatchConfig) -> (Watcher, mpsc::Receiver<Batch>) {
        let (tx, rx) = mpsc::channel();
        let watcher = Watcher::start(config, Box::new(Recorder(tx))).unwrap();
        let first = next(&rx);
        assert!(first.is_full(), "the bring-up hands over a Full local scan: {first:?}");
        (watcher, rx)
    }

    /// The handle of `rel`, by name.
    fn handle(&self, rel: &str) -> FileHandle {
        let path = self.path(rel);
        FileHandle::at(&File::open(path.parent().unwrap()).unwrap(), path.file_name().unwrap()).unwrap()
    }

    /// `script`, run by `sh` in the folder: another process, whose events
    /// carry no pid of ours.
    fn shell(&self, script: &str) {
        let status = Command::new("sh").arg("-c").arg(script).current_dir(&self.root.path).status().unwrap();
        assert!(status.success(), "{script}");
    }
}

struct Recorder(mpsc::Sender<Batch>);

impl Sink for Recorder {
    fn handle(&mut self, batch: &Batch) -> Handled {
        let _ = self.0.send(batch.clone());
        Handled::Done { recheck: Batch::new(), passed: Box::default() }
    }
}

fn next(rx: &mpsc::Receiver<Batch>) -> Batch {
    rx.recv_timeout(WAIT).expect("a batch")
}

/// The next Full local scan, whatever is handed over before it.
fn next_full(rx: &mpsc::Receiver<Batch>) -> Batch {
    loop {
        let batch = next(rx);
        if batch.is_full() {
            return batch;
        }
    }
}

/// `name` in `dir`, as a `FAN_CLOSE_WRITE` makes it dirty.
fn written(batch: &mut Batch, dir: &str, name: &str, handle: FileHandle) {
    batch.written(Path::new(dir), OsStr::new(name), Some(handle));
}

fn named(batch: &mut Batch, dir: &str, name: &str, handle: FileHandle) {
    batch.name(Path::new(dir), OsStr::new(name));
    batch.object(handle);
}

#[test]
fn changes_come_as_one_batch_with_each_directory_where_it_is_now() {
    let fx = Fx::new();
    std::fs::create_dir(fx.path("docs")).unwrap();
    let (watcher, rx) = fx.start(fx.config());
    let docs = fx.handle("docs");

    std::fs::write(fx.path("docs/a.txt"), b"a").unwrap();
    std::fs::rename(fx.path("docs"), fx.path("papers")).unwrap();
    std::fs::write(fx.path("papers/b.txt"), b"b").unwrap();

    let mut expected = Batch::new();
    written(&mut expected, "papers", "a.txt", fx.handle("papers/a.txt"));
    named(&mut expected, "", "docs", docs.clone());
    named(&mut expected, "", "papers", docs);
    written(&mut expected, "papers", "b.txt", fx.handle("papers/b.txt"));
    assert_eq!(next(&rx), expected, "one batch, and a.txt where its directory is now");
    assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "nothing more");
    assert_eq!(watcher.status().handed_over, 2);
    watcher.stop();
}

impl Fx {
    /// `config` with a link to a helper of its own, which marks whatever it
    /// is asked to until the test tells it otherwise.
    fn with_helper(&self, mut config: WatchConfig) -> (WatchConfig, FakeHelper) {
        let helper = FakeHelper::standalone();
        config.link = crate::helper::LinkCell::holding(Some(self.runtime.block_on(helper.connect())));
        (config, helper)
    }

    fn ino(&self, rel: &str) -> u64 {
        std::fs::metadata(self.path(rel)).unwrap().ino()
    }
}

/// The directories the helper was asked to mark so far, by inode, in order.
fn asked(helper: &FakeHelper) -> Vec<u64> {
    helper.marks().iter().map(|mark| mark.ino).collect()
}

/// Waits until the helper has been asked to mark every one of `inodes`.
fn marked_all(helper: &FakeHelper, inodes: Vec<u64>) {
    let deadline = Instant::now() + WAIT;
    loop {
        let asked = asked(helper);
        let missing: Vec<u64> = inodes.iter().copied().filter(|ino| !asked.contains(ino)).collect();
        if missing.is_empty() {
            return;
        }
        assert!(Instant::now() < deadline, "never marked for interception: {missing:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Makes the directory `name` in the folder and returns once the reader is
/// held in its `MarkDir`: it reads nothing until the returned sender is told
/// to let the helper answer.
fn reader_held_at(fx: &Fx, helper: &FakeHelper, name: &str) -> mpsc::Sender<()> {
    let (reached, release) = helper.stall_on(&format!("/{name}"));
    std::fs::create_dir(fx.path(name)).unwrap();
    reached.recv_timeout(WAIT).expect("the reader asks the helper to mark the new directory");
    release
}

#[test]
fn a_new_directory_is_marked_for_interception_then_watched_and_its_tree_is_dirty() {
    let fx = Fx::new();
    std::fs::create_dir(fx.path("old")).unwrap();
    let (config, helper) = fx.with_helper(fx.config());
    let (watcher, rx) = fx.start(config);
    // The helper's own walk may have passed the root before `old` was made.
    assert_eq!(asked(&helper), vec![fx.ino("old")], "the bring-up asks for every directory");

    // `sub` and its file are made before `new` is marked, so they raise no
    // event: they are found by the scan of `new`'s tree.
    let release = reader_held_at(&fx, &helper, "new");
    fx.shell("mkdir new/sub && echo x > new/sub/f");
    release.send(()).unwrap();
    let mut expected = Batch::new();
    named(&mut expected, "", "new", fx.handle("new"));
    expected.tree(Path::new("new"));
    assert_eq!(next(&rx), expected);
    assert_eq!(asked(&helper), vec![fx.ino("old"), fx.ino("new"), fx.ino("new/sub")], "each new directory was marked for interception before it was listed");

    // `sub` now raises events of its own.
    std::fs::write(fx.path("new/sub/g"), b"g").unwrap();
    let mut expected = Batch::new();
    written(&mut expected, "new/sub", "g", fx.handle("new/sub/g"));
    assert_eq!(next(&rx), expected);
    watcher.stop();
}

#[test]
fn a_move_across_the_border_is_one_sided_and_a_directory_that_left_is_forgotten() {
    let fx = Fx::new();
    std::fs::create_dir(fx.path("d")).unwrap();
    std::fs::create_dir_all(fx.outside.join("indir/sub")).unwrap();
    std::fs::write(fx.outside.join("in.txt"), b"in").unwrap();
    let (watcher, rx) = fx.start(fx.config());
    let d = fx.handle("d");

    std::fs::rename(fx.outside.join("in.txt"), fx.path("in.txt")).unwrap();
    std::fs::rename(fx.outside.join("indir"), fx.path("indir")).unwrap();
    std::fs::rename(fx.path("d"), fx.outside.join("d")).unwrap();
    let mut expected = Batch::new();
    named(&mut expected, "", "in.txt", fx.handle("in.txt"));
    named(&mut expected, "", "indir", fx.handle("indir"));
    expected.tree(Path::new("indir"));
    named(&mut expected, "", "d", d);
    assert_eq!(next(&rx), expected);

    // `d` still carries the mark, outside: its events are not the folder's.
    // `indir/sub` came in unmarked, and was marked on arrival.
    std::fs::write(fx.outside.join("d/x"), b"x").unwrap();
    std::fs::write(fx.path("indir/sub/y"), b"y").unwrap();
    let mut expected = Batch::new();
    written(&mut expected, "indir/sub", "y", fx.handle("indir/sub/y"));
    assert_eq!(next(&rx), expected);
    watcher.stop();
}

#[test]
fn the_daemons_own_changes_and_a_fill_raise_nothing_to_examine() {
    let fx = Fx::new();
    let source = fx.dir.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("ITEM"), b"the content").unwrap();
    let root = File::open(&fx.root.path).unwrap();
    placeholder::create_placeholder(&root, "doc.txt", "ITEM", 11, SystemTime::now()).unwrap();
    let mut config = fx.config();
    config.own_pid = Some(std::process::id() as i32);
    let (watcher, rx) = fx.start(config);

    // A fill, as the daemon makes it: data, size, time and attributes (§3.2).
    let fd = OwnedFd::from(File::options().read(true).write(true).open(fx.path("doc.txt")).unwrap());
    assert!(fx.runtime.block_on(crate::hydration::source::hydrate_with(fd, &LocalDir::new(&source), None)).is_ok());
    assert_eq!(std::fs::read(fx.path("doc.txt")).unwrap(), b"the content");
    // A directory the daemon makes is watched, and is not a change of its
    // own; what is inside it is looked at once (someone else may
    // have put something there before it was marked).
    std::fs::create_dir(fx.path("placed")).unwrap();
    let mut expected = Batch::new();
    expected.tree(Path::new("placed"));
    assert_eq!(next(&rx), expected, "nothing of the fill, and no `placed` of its own");
    fx.shell("echo x > placed/f && echo y > theirs.txt");

    let mut expected = Batch::new();
    written(&mut expected, "placed", "f", fx.handle("placed/f"));
    written(&mut expected, "", "theirs.txt", fx.handle("theirs.txt"));
    assert_eq!(next(&rx), expected, "only another process's changes");
    watcher.stop();
}

#[test]
fn an_overflow_is_a_full_scan_and_a_walk_that_marks_what_was_missed() {
    let fx = Fx::new();
    let (config, helper) = fx.with_helper(fx.config());
    let (watcher, rx) = fx.start(config);
    let queue: usize = std::fs::read_to_string("/proc/sys/fs/fanotify/max_queued_events").unwrap().trim().parse().unwrap();
    // The reader reads nothing while the helper holds its answer: the queue fills.
    let release = reader_held_at(&fx, &helper, "gate");
    for n in 0..queue + 10 {
        File::create(fx.path(&format!("f{n}"))).unwrap();
    }
    // Its event is lost with the overflow.
    std::fs::create_dir(fx.path("late")).unwrap();
    release.send(()).unwrap();
    let batch = next(&rx);
    assert!(batch.is_full(), "an overflow cannot be localised");
    assert_eq!(watcher.status().overflows, 1);
    marked_all(&helper, vec![fx.ino("late")]);

    std::fs::write(fx.path("late/x"), b"x").unwrap();
    let mut expected = Batch::new();
    written(&mut expected, "late", "x", fx.handle("late/x"));
    assert_eq!(next(&rx), expected, "the walk after the overflow marked `late`");
    watcher.stop();
}

#[test]
fn the_folder_moved_away_stops_the_watcher_and_says_so() {
    let fx = Fx::new();
    let (tx, notes) = mpsc::channel();
    let mut config = fx.config();
    config.on_status = Some(Arc::new(move |s: &WatchStatus| {
        let _ = tx.send(s.clone());
    }));
    let (watcher, rx) = fx.start(config);
    std::fs::rename(&fx.root.path, fx.dir.path().join("moved")).unwrap();
    let status = notes.recv_timeout(WAIT).unwrap();
    assert!(status.root_gone && status.note().unwrap().contains("moved or deleted"), "{status:?}");
    assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "nothing handed over");
    watcher.stop();
}

#[test]
fn past_the_mark_budget_the_folder_is_scanned_on_a_timer() {
    let fx = Fx::new();
    for name in ["a", "b", "c"] {
        std::fs::create_dir(fx.path(name)).unwrap();
    }
    let (tx, notes) = mpsc::channel();
    let (mut config, helper) = fx.with_helper(fx.config());
    config.mark_budget = Some(2);
    config.on_status = Some(Arc::new(move |s: &WatchStatus| {
        let _ = tx.send(s.clone());
    }));
    let (watcher, rx) = fx.start(config);
    let status = notes.recv_timeout(WAIT).unwrap();
    assert!(status.note().unwrap().contains("max_user_marks"), "{status:?}");
    let status = watcher.status();
    assert_eq!((status.directories, status.unwatched), (4, 2), "{status:?}");
    // Nothing happens in the folder; the scan comes anyway. The reader's walk on the same beat
    // hands over the directories it could not mark, before the scan or after it.
    assert_eq!(next_full(&rx).reason(), Some(ScanReason::Periodic), "the scan of the degraded beat");

    // Two of these three are made in directories nobody watches: no event,
    // but the periodic walk has them marked for interception all the same.
    for name in ["a", "b", "c"] {
        std::fs::create_dir(fx.path(&format!("{name}/n"))).unwrap();
    }
    marked_all(&helper, vec![fx.ino("a/n"), fx.ino("b/n"), fx.ino("c/n")]);
    watcher.stop();
}

/// A directory that could not be opened when the watcher met it
/// is adopted with everything below it once it can be (its own `chmod`).
#[test]
fn a_directory_closed_at_the_walk_is_watched_all_the_way_down_once_opened() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: a plain syscall with no arguments.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: running as root, which chmod 000 cannot refuse");
        return;
    }
    let fx = Fx::new();
    std::fs::create_dir_all(fx.path("closed/inner")).unwrap();
    std::fs::set_permissions(fx.path("closed"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let (config, helper) = fx.with_helper(fx.config());
    let (watcher, rx) = fx.start(config);
    assert_eq!(watcher.status().unwatched, 1, "`closed`, kept in the map without its mark");

    std::fs::set_permissions(fx.path("closed"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut expected = Batch::new();
    expected.name(Path::new("closed"), OsStr::new("."));
    expected.tree(Path::new("closed"));
    assert_eq!(next(&rx), expected, "the whole tree, since nothing in it raised events");
    marked_all(&helper, vec![fx.ino("closed"), fx.ino("closed/inner")]);
    std::fs::write(fx.path("closed/inner/f"), b"f").unwrap();
    let mut expected = Batch::new();
    written(&mut expected, "closed/inner", "f", fx.handle("closed/inner/f"));
    assert_eq!(next(&rx), expected, "`inner` was marked on the way down");
    watcher.stop();
}

/// A `MarkDir` the helper refused is asked again when it is back,
/// and says so meanwhile.
#[test]
fn a_directory_the_helper_did_not_mark_is_asked_again() {
    let fx = Fx::new();
    let (tx, notes) = mpsc::channel();
    let (mut config, helper) = fx.with_helper(fx.config());
    config.on_status = Some(Arc::new(move |s: &WatchStatus| {
        let _ = tx.send(s.clone());
    }));
    let (watcher, rx) = fx.start(config);
    helper.refuse_marks(libc::EPERM);
    std::fs::create_dir(fx.path("new")).unwrap();
    next(&rx);
    let status = notes.recv_timeout(WAIT).unwrap();
    assert_eq!(status.uncovered, 1);
    assert!(status.note().unwrap().contains("not yet protected"), "{status:?}");

    helper.refuse_marks(0);
    watcher.helper_back();
    let status = notes.recv_timeout(WAIT).unwrap();
    assert_eq!((status.uncovered, status.note()), (0, None));
    assert_eq!(asked(&helper), vec![fx.ino("new"); 2], "refused once, then asked again and marked");
    assert!(next(&rx).is_full(), "and a Full local scan for what changed while it was away");
    watcher.stop();
}

/// §3.4: a write through `O_TMPFILE` is reported under `#<inode>`, a name
/// that never exists; the object's handle finds it.
#[test]
fn an_o_tmpfile_write_is_handed_over_with_its_pseudo_name_and_its_object() {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let fx = Fx::new();
    let (watcher, rx) = fx.start(fx.config());
    let mut file = File::options().write(true).custom_flags(libc::O_TMPFILE).open(&fx.root.path).unwrap();
    file.write_all(b"saved").unwrap();
    let ino = file.metadata().unwrap().ino();
    let proc = std::ffi::CString::new(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap();
    let target = std::ffi::CString::new(fx.path("linked").into_os_string().into_encoded_bytes()).unwrap();
    // SAFETY: two NUL-terminated paths; the usual way to name an O_TMPFILE.
    assert_eq!(unsafe { libc::linkat(libc::AT_FDCWD, proc.as_ptr(), libc::AT_FDCWD, target.as_ptr(), libc::AT_SYMLINK_FOLLOW) }, 0);
    drop(file);
    let handle = fx.handle("linked");
    let mut expected = Batch::new();
    named(&mut expected, "", "linked", handle.clone());
    written(&mut expected, "", &format!("#{ino}"), handle);
    assert_eq!(next(&rx), expected);
    watcher.stop();
}

/// A flush hands over what the events made dirty and waits for
/// its examination, without waiting for the quiet spell.
#[test]
fn a_flush_examines_what_is_pending_at_once() {
    let fx = Fx::new();
    let mut config = fx.config();
    config.timing.quiet = Duration::from_secs(3600);
    config.timing.ceiling = Duration::from_secs(3600);
    let (tx, rx) = mpsc::channel();
    let watcher = Watcher::start(config, Box::new(Recorder(tx))).unwrap();
    let mut walked = watcher.walked();
    fx.runtime.block_on(walked.wait_for(|state| *state == WalkState::Done)).unwrap();
    assert!(watcher.flush(WAIT), "the bring-up's Full scan, examined");
    assert!(next(&rx).is_full());
    std::fs::write(fx.path("saved.txt"), b"s").unwrap();
    assert!(watcher.flush(WAIT));
    let mut expected = Batch::new();
    written(&mut expected, "", "saved.txt", fx.handle("saved.txt"));
    assert_eq!(rx.try_recv().unwrap(), expected, "examined before `flush` returned");
    assert!(watcher.flush(WAIT), "nothing pending is flushed at once");
    watcher.stop();
}

/// A sink that fails a number of batches, and examines what comes after.
struct Failing(u32, mpsc::Sender<Batch>);

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

/// LO3: a batch that keeps failing is said in `LastError`, until one passes.
#[test]
fn a_batch_that_keeps_failing_is_said_until_one_passes() {
    let fx = Fx::new();
    let said: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let mut config = fx.config();
    config.on_status = Some({
        let said = Arc::clone(&said);
        Arc::new(move |status: &WatchStatus| said.lock().unwrap().push(status.failing.clone()))
    });
    let (tx, rx) = mpsc::channel();
    let watcher = Watcher::start(config, Box::new(Failing(FAILING_AFTER, tx))).unwrap();
    // The bring-up's Full local scan fails three times, and passes at the fourth.
    assert!(next(&rx).is_full());
    assert!(watcher.flush(WAIT));
    assert_eq!(watcher.status().failing, None);
    let said: Vec<String> = said.lock().unwrap().iter().flatten().cloned().collect();
    assert!(!said.is_empty() && said.iter().all(|why| why.contains("the store is closed")), "the failing batch was not said: {said:?}");
    let failing = WatchStatus { failing: said.first().cloned(), ..WatchStatus::default() };
    assert!(failing.note().is_some_and(|note| note.contains("keeps failing")), "{:?}", failing.note());
    watcher.stop();
}

/// A sink that says it was handed a batch, and panics on it.
struct Panicking(mpsc::Sender<()>);

impl Sink for Panicking {
    fn handle(&mut self, _batch: &Batch) -> Handled {
        let _ = self.0.send(());
        panic!("the examination panicked (this test's own panic)");
    }
}

/// LO4: an examiner thread that ends with nobody asking it to says so, as the
/// reader does: nothing is examined any more, so `LastError` must tell.
#[test]
fn an_examiner_that_dies_says_the_watcher_stopped() {
    let fx = Fx::new();
    let (tx, handed) = mpsc::channel();
    let watcher = Watcher::start(fx.config(), Box::new(Panicking(tx))).unwrap();
    // The bring-up's Full local scan: the sink panics on it, and its thread ends.
    handed.recv_timeout(WAIT).expect("the bring-up's Full local scan is handed to the sink");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !watcher.status().stopped && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    // What is changed from now on is handed to nobody.
    std::fs::write(fx.path("after.txt"), b"a").unwrap();
    assert!(!watcher.flush(WAIT), "nothing examines any more");
    let status = watcher.status();
    assert!(
        status.stopped && status.note().is_some_and(|note| note.contains("the watcher stopped")),
        "the examiner thread ended on a panic and the watcher does not say it stopped: {status:?}"
    );
    watcher.stop();
}

fn row(id: &str, parent: Option<&str>, name: &str, kind: Kind) -> Row {
    Row {
        id: id.into(),
        parent_id: parent.map(str::to_owned),
        name: name.into(),
        kind,
        size: 0,
        mtime: 1_700_000_000,
        etag: Some(format!("e-{id}")),
        ctag: Some(format!("c-{id}")),
        quickxor: None,
        mime: None,
        placement: Placement::Placed,
    }
}

/// A read-write folder whose first listing (the root and `docs`) is placed in
/// it by the real reconcile and not committed yet: there is no base to
/// examine against until the returned step is committed.
fn listed_not_committed(fx: &Fx) -> (World, Step) {
    fx.runtime.block_on(async {
        let world = World::new(Options { writes: true, ..Options::default() }).await;
        let listing = [Change::Root(row("R", None, "", Kind::Folder)), Change::Upsert(row("D", Some("R"), "docs", Kind::Folder))];
        let mut step = world.step(Says::Whole(&listing)).await;
        step.apply().await.unwrap();
        (world, step)
    })
}

/// The daemon's sink over `world`'s folder and store, with nobody to ask
/// where an object went.
fn sink(fx: &Fx, world: &World) -> ExamineSink {
    ExamineSink {
        root: world.root.clone(),
        store: world.store.clone(),
        locks: world.locks.clone(),
        ignore: IgnoreList::default().shared(),
        liveness: Box::new(NoLiveness),
        link: crate::helper::LinkCell::default(),
        runtime: fx.runtime.handle().clone(),
        on_rows: None,
        on_handles: None,
        tree_lock: None,
        scan: None,
    }
}

#[test]
fn a_file_made_in_the_folder_becomes_a_create_row_once_the_listing_is_complete() {
    let fx = Fx::new();
    let (world, step) = listed_not_committed(&fx);
    let store = world.store.clone();
    // The outbox worker's wake.
    let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let on_rows: Arc<dyn Fn() + Send + Sync> = {
        let woken = Arc::clone(&woken);
        Arc::new(move || {
            woken.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
    };
    let sink = ExamineSink { on_rows: Some(on_rows), ..sink(&fx, &world) };
    let watcher = Watcher::start(fx.config_for(&world.root), Box::new(sink)).unwrap();
    std::fs::write(world.path("docs/new.txt"), b"new").unwrap();
    std::fs::write(world.path("docs/.new.txt.swp"), b"noise").unwrap();
    std::thread::sleep(Duration::from_millis(800));
    assert!(store.call_blocking(move |s| s.outbox_rows()).unwrap().is_empty(), "no base to compare with until the listing completes");

    fx.runtime.block_on(step.commit()).unwrap();
    let deadline = Instant::now() + WAIT;
    let rows = loop {
        let rows = store.call_blocking(move |s| s.outbox_rows()).unwrap();
        if !rows.is_empty() || Instant::now() > deadline {
            break rows;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let summary: Vec<_> = rows.iter().map(|r| (r.kind, r.rel.clone())).collect();
    assert_eq!(summary, vec![(OutboxKind::Create, PathBuf::from("docs/new.txt"))]);
    assert!(watcher.status().examined >= 1);
    assert!(woken.load(std::sync::atomic::Ordering::SeqCst) >= 1, "the rows wake the outbox worker");
    watcher.stop();
}

/// The daemon's sink tells the folder's state how a Full local scan goes (issue #8): its
/// reason, then idle with when it finished, how long it took and what it saw. A scan with no
/// base yet, and a single place examined, change nothing.
#[test]
fn the_sink_reports_a_full_scan_and_not_a_single_place() {
    use crate::config::Mode;
    use crate::status::snapshot::ScanState;
    let fx = Fx::new();
    let (world, step) = listed_not_committed(&fx);
    std::fs::write(world.path("docs/new.txt"), b"new").unwrap();
    let state = world.state.clone();
    state.update(|s| {
        s.cycle.items_placed = 2;
        s.local.scan.follow(Mode::ReadWrite);
    });
    let mut sink = ExamineSink { scan: Some(ScanReport { state: state.clone(), every: Duration::ZERO }), ..sink(&fx, &world) };
    let idle = state.get().local.scan;
    assert!(matches!(sink.handle(&Batch::scan(ScanReason::ReadWrite)), Handled::NotYet));
    assert_eq!(state.get().local.scan, idle, "no base yet: no scan ran");

    fx.runtime.block_on(step.commit()).unwrap();
    assert!(matches!(sink.handle(&Batch::scan(ScanReason::ReadWrite)), Handled::Done { .. }));
    let scan = state.get().local.scan;
    assert_eq!((scan.state, scan.reason.as_str(), scan.expected), (ScanState::Idle, "read-write", 2));
    assert_eq!((scan.directories, scan.files), (1, 1), "docs, and docs/new.txt");
    assert!(scan.started > 0 && scan.finished >= scan.started, "{scan:?}");

    let mut one = Batch::new();
    one.name(Path::new("docs"), OsStr::new("new.txt"));
    assert!(matches!(sink.handle(&one), Handled::Done { .. }));
    assert_eq!(state.get().local.scan, scan, "a single place examined is not a scan");
}
