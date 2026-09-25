//! The watcher with a real notification group, unprivileged, in temporary
//! directories (`docs/design/writes.md` §12): batches after a quiet spell, renames
//! within, into and out of the folder, a new directory marked through the
//! helper and then scanned, the daemon's own events dropped by pid (a fill
//! included, §3.2), an overflow, the root going away, the degraded mode,
//! and one batch followed all the way to an outbox row.
//!
//! Unless a test says otherwise the watcher drops nothing by pid, so the test
//! process itself can stand in for the user.

use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{self, XATTR_ROOT};
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::sync::disk::Disk;
use crate::sync::helper::HelperLink;
use crate::sync::local::{IgnoreList, NoLiveness};
use crate::sync::materialize::{Materializer, Scope};
use crate::sync::source::LocalDir;
use crate::sync::InodeLocks;
use crate::tree::outbox::OutboxKind;
use crate::tree::{Change, Kind, Placement, Row, Store, TreeStore};

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
        let mut config = WatchConfig::new(self.root.clone(), Arc::new(Mutex::new(None)), self.runtime.handle().clone());
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
        Handled::Done { recheck: Batch::new() }
    }
}

fn next(rx: &mpsc::Receiver<Batch>) -> Batch {
    rx.recv_timeout(WAIT).expect("a batch")
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

/// A tiny helper: refuses the first `refuse` `MarkDir`s (`EPERM`),
/// acknowledges everything else, and tells which directories it marked, by
/// inode.
fn fake_helper(path: &Path, refuse: usize) -> mpsc::Receiver<u64> {
    use nix::sys::socket::{accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr};
    let listener = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None).unwrap();
    bind(listener.as_raw_fd(), &UnixAddr::new(path).unwrap()).unwrap();
    listen(&listener, Backlog::new(4).unwrap()).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let fd = accept(listener.as_raw_fd()).unwrap();
        // SAFETY: `accept` returned a descriptor nothing else owns.
        let mut channel = Channel::new(unsafe { UnixStream::from_raw_fd(fd) }).unwrap();
        channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
        let mut refused = 0;
        while let Ok((message, fd)) = channel.recv::<ToHelper>() {
            let mut errno = 0;
            if let (ToHelper::MarkDir, Some(fd)) = (&message, fd) {
                if refused < refuse {
                    refused += 1;
                    errno = libc::EPERM;
                } else {
                    let _ = tx.send(File::from(fd).metadata().unwrap().ino());
                }
            }
            channel.send(&ToDaemon::Ack { errno }, None).unwrap();
        }
    });
    rx
}

impl Fx {
    /// `config` with a link to a [`fake_helper`] refusing its first `refuse`
    /// `MarkDir`s.
    fn with_helper(&self, mut config: WatchConfig, refuse: usize) -> (WatchConfig, mpsc::Receiver<u64>) {
        let socket = self.dir.path().join("helper.sock");
        let marked = fake_helper(&socket, refuse);
        let link = self.runtime.block_on(HelperLink::connect(&socket)).unwrap().0;
        config.link = Arc::new(Mutex::new(Some(link)));
        (config, marked)
    }

    fn ino(&self, rel: &str) -> u64 {
        std::fs::metadata(self.path(rel)).unwrap().ino()
    }
}

/// Waits until the helper has marked every one of `inodes`.
fn marked_all(marked: &mpsc::Receiver<u64>, mut inodes: Vec<u64>) {
    let deadline = Instant::now() + WAIT;
    while !inodes.is_empty() && Instant::now() < deadline {
        if let Ok(ino) = marked.recv_timeout(Duration::from_millis(100)) {
            inodes.retain(|i| *i != ino);
        }
    }
    assert!(inodes.is_empty(), "never marked for interception: {inodes:?}");
}

#[test]
fn a_new_directory_is_marked_for_interception_then_watched_and_its_tree_is_dirty() {
    let fx = Fx::new();
    std::fs::create_dir(fx.path("old")).unwrap();
    let (config, marked) = fx.with_helper(fx.config(), 0);
    let (watcher, rx) = fx.start(config);
    // The helper's own walk may have passed the root before `old` was made.
    assert_eq!(marked.try_iter().collect::<Vec<_>>(), vec![fx.ino("old")], "the bring-up asks for every directory");

    // Made while the watcher looks away: `sub` exists before `new` is
    // marked, and is found by the scan.
    watcher.pause(true);
    fx.shell("mkdir -p new/sub && echo x > new/sub/f");
    watcher.pause(false);
    let mut expected = Batch::new();
    named(&mut expected, "", "new", fx.handle("new"));
    expected.tree(Path::new("new"));
    assert_eq!(next(&rx), expected);
    let mut asked: Vec<u64> = marked.try_iter().collect();
    asked.sort();
    let mut inodes = vec![fx.ino("new"), fx.ino("new/sub")];
    inodes.sort();
    assert_eq!(asked, inodes, "both new directories were marked for interception");

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
    assert_eq!(fx.runtime.block_on(crate::sync::source::hydrate(fd, &LocalDir::new(&source))), 0);
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
    let (config, marked) = fx.with_helper(fx.config(), 0);
    let (watcher, rx) = fx.start(config);
    let queue: usize = std::fs::read_to_string("/proc/sys/fs/fanotify/max_queued_events").unwrap().trim().parse().unwrap();
    watcher.pause(true);
    for n in 0..queue + 10 {
        File::create(fx.path(&format!("f{n}"))).unwrap();
    }
    // Its event is lost with the overflow.
    std::fs::create_dir(fx.path("late")).unwrap();
    watcher.pause(false);
    let batch = next(&rx);
    assert!(batch.is_full(), "an overflow cannot be localised");
    assert_eq!(watcher.status().overflows, 1);
    marked_all(&marked, vec![fx.ino("late")]);

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
    let (mut config, marked) = fx.with_helper(fx.config(), 0);
    config.mark_limit = Some(2);
    config.on_status = Some(Arc::new(move |s: &WatchStatus| {
        let _ = tx.send(s.clone());
    }));
    let (watcher, rx) = fx.start(config);
    let status = notes.recv_timeout(WAIT).unwrap();
    assert!(status.note().unwrap().contains("max_user_marks"), "{status:?}");
    let status = watcher.status();
    assert_eq!((status.directories, status.unwatched), (4, 2), "{status:?}");
    // Nothing happens in the folder; the scan comes anyway.
    assert!(next(&rx).is_full());

    // Two of these three are made in directories nobody watches: no event,
    // but the periodic walk has them marked for interception all the same.
    for name in ["a", "b", "c"] {
        std::fs::create_dir(fx.path(&format!("{name}/n"))).unwrap();
    }
    marked_all(&marked, vec![fx.ino("a/n"), fx.ino("b/n"), fx.ino("c/n")]);
    watcher.stop();
}

/// A directory that could not be opened when the watcher met it
/// is adopted with everything below it once it can be (its own `chmod`).
#[test]
fn a_directory_closed_at_the_walk_is_watched_all_the_way_down_once_opened() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new();
    std::fs::create_dir_all(fx.path("closed/inner")).unwrap();
    std::fs::set_permissions(fx.path("closed"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let (config, marked) = fx.with_helper(fx.config(), 0);
    let (watcher, rx) = fx.start(config);
    assert_eq!(watcher.status().unwatched, 1, "`closed`, kept in the map without its mark");

    std::fs::set_permissions(fx.path("closed"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut expected = Batch::new();
    expected.name(Path::new("closed"), OsStr::new("."));
    expected.tree(Path::new("closed"));
    assert_eq!(next(&rx), expected, "the whole tree, since nothing in it raised events");
    marked_all(&marked, vec![fx.ino("closed"), fx.ino("closed/inner")]);
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
    let (mut config, marked) = fx.with_helper(fx.config(), 1);
    config.on_status = Some(Arc::new(move |s: &WatchStatus| {
        let _ = tx.send(s.clone());
    }));
    let (watcher, rx) = fx.start(config);
    std::fs::create_dir(fx.path("new")).unwrap();
    next(&rx);
    let status = notes.recv_timeout(WAIT).unwrap();
    assert_eq!(status.uncovered, 1);
    assert!(status.note().unwrap().contains("not yet protected"), "{status:?}");
    assert!(marked.try_recv().is_err(), "refused, not marked");

    watcher.helper_back();
    marked_all(&marked, vec![fx.ino("new")]);
    let status = notes.recv_timeout(WAIT).unwrap();
    assert_eq!((status.uncovered, status.note()), (0, None));
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

/// the mode switch's hook: a watcher only for a read-write folder, started without
/// waiting for its walk; and when the folder is moved away it reads `error`
/// with the reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_write_folder_gets_a_watcher_and_a_folder_moved_away_says_so() {
    use crate::config::Mode;
    use crate::sync::{published_error, published_state, SyncService};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("OneDrive");
    std::fs::create_dir(&path).unwrap();
    let root = SyncRoot { path: path.clone(), root_id: "9d8c7b6a-5f4e-4d3c-8b2a-1f0e9d8c7b6a".into() };
    xattr::set(&path, XATTR_ROOT, root.root_id.as_bytes()).unwrap();
    let service = SyncService::new(None, None, None);
    let store = Store::new(TreeStore::in_memory().unwrap());
    assert!(service.start_watcher(&root, &store).is_none(), "a read-only folder is not watched");
    service.start_in_mode(Mode::ReadWrite);
    let watcher = service.start_watcher(&root, &store).expect("a watcher");
    assert_eq!(*watcher.walked().wait_for(|state| *state != WalkState::Walking).await.unwrap(), WalkState::Done);

    std::fs::rename(&path, dir.path().join("moved")).unwrap();
    let deadline = Instant::now() + WAIT;
    while published_state(&service.state().get()) != "error" && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let snapshot = service.state().get();
    assert_eq!(published_state(&snapshot), "error");
    assert!(published_error(&snapshot).contains("moved or deleted"), "{}", published_error(&snapshot));
    service.stop_watcher(watcher).await;
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

#[test]
fn a_file_made_in_the_folder_becomes_a_create_row_once_the_listing_is_complete() {
    let fx = Fx::new();
    let store = Store::new(TreeStore::in_memory().unwrap());
    store
        .with(|s| {
            s.begin_staging(false)?;
            s.stage(&[Change::Root(row("R", None, "", Kind::Folder)), Change::Upsert(row("D", Some("R"), "docs", Kind::Folder))])
        })
        .unwrap();
    Materializer {
        disk: Disk::open(&fx.root, false).unwrap(),
        store: store.clone(),
        link: None,
        runtime: fx.runtime.handle().clone(),
        locks: InodeLocks::new(),
        root_item_id: "R".into(),
        rescue_into: fx.outside.join("rescued"),
        cancel: CancellationToken::new(),
        rw: None,
        claimed: None,
    }
    .apply(Scope::Full)
    .unwrap();
    // The outbox worker's wake.
    let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sink = ExamineSink {
        root: fx.root.clone(),
        store: store.clone(),
        locks: InodeLocks::new(),
        ignore: IgnoreList::default().shared(),
        liveness: Box::new(NoLiveness),
        link: Arc::new(Mutex::new(None)),
        runtime: fx.runtime.handle().clone(),
        on_rows: Some({
            let woken = Arc::clone(&woken);
            Arc::new(move || {
                woken.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        }),
        on_handles: None,
        tree_lock: None,
    };
    let watcher = Watcher::start(fx.config(), Box::new(sink)).unwrap();
    std::fs::write(fx.path("docs/new.txt"), b"new").unwrap();
    std::fs::write(fx.path("docs/.new.txt.swp"), b"noise").unwrap();
    std::thread::sleep(Duration::from_millis(800));
    assert!(store.with(|s| s.outbox_rows()).unwrap().is_empty(), "no base to compare with until the listing completes");

    store.with(|s| s.commit_staging("link-1")).unwrap();
    let deadline = Instant::now() + WAIT;
    let rows = loop {
        let rows = store.with(|s| s.outbox_rows()).unwrap();
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
