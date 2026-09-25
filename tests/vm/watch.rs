//! The write phase's watcher against the real helper: what needs
//! the helper's permission marks, or root, and so cannot run on the host.
//!
//! The watcher runs in this process, as it does in the daemon: `MarkDir`
//! goes over this process's helper link, and events this process causes are
//! its own (the fills `serve_hydrations` makes here included). Changes a user
//! would make are made by `sh` children. Its batches go to a recorder, not to
//! an examination: the suite's folder has no tree store.

use std::ffi::OsStr;
use std::fs::File;
use std::path::Path;
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use konedrive_fs::handle::FileHandle;
use konedrived::sync::local::Batch;
use konedrived::sync::watcher::{Handled, Sink, Timing, WatchConfig, Watcher};

use crate::{dir_mark_present, Checks, Ctx};

struct Recorder(mpsc::Sender<Batch>);

impl Sink for Recorder {
    fn handle(&mut self, batch: &Batch) -> Handled {
        let _ = self.0.send(batch.clone());
        Handled::Done { recheck: Batch::new() }
    }
}

const QUIET: Duration = Duration::from_millis(300);

/// The suite's folder watched as the daemon watches a read-write folder,
/// with a short quiet spell. The bring-up's Full local scan is taken off the
/// receiver.
fn start(ctx: &Ctx) -> Result<(Watcher, mpsc::Receiver<Batch>), String> {
    let link = ctx.link()?;
    let mut config = WatchConfig::new(ctx.sync_root(), Arc::new(Mutex::new(Some(link))), ctx.runtime.handle().clone());
    config.timing = Timing {
        quiet: QUIET,
        ceiling: Duration::from_secs(5),
        recheck: Duration::from_secs(1),
        retry: Duration::from_secs(1),
        ..Timing::default()
    };
    let (tx, rx) = mpsc::channel();
    let watcher = Watcher::start(config, Box::new(Recorder(tx))).map_err(|e| format!("the watcher did not start: {e}"))?;
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(batch) if batch.is_full() => Ok((watcher, rx)),
        Ok(batch) => Err(format!("the first batch is not the Full local scan: {batch:?}")),
        Err(_) => Err("the bring-up handed nothing over".into()),
    }
}

fn next(rx: &mpsc::Receiver<Batch>) -> Result<Batch, String> {
    rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "no batch was handed over".to_string())
}

/// `script` run by `sh` in the folder: another process, as a user is.
fn sh(ctx: &Ctx, script: &str) -> Result<(), String> {
    let status = Command::new("sh").arg("-c").arg(script).current_dir(&ctx.root).status().map_err(|e| e.to_string())?;
    status.success().then_some(()).ok_or_else(|| format!("`{script}` failed: {status}"))
}

/// How long until the helper holds a permission mark on `dir`.
fn marked_within(ctx: &Ctx, dir: &Path, since: Instant, within: Duration) -> Result<Duration, String> {
    let ino = ctx.ino_of(dir)?;
    while since.elapsed() < within {
        if dir_mark_present(ctx.helper_pid(), ino) {
            return Ok(since.elapsed());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(format!("{} was not marked for interception within {within:?}", dir.display()))
}

fn handle(path: &Path) -> Result<FileHandle, String> {
    let parent = File::open(path.parent().unwrap()).map_err(|e| e.to_string())?;
    FileHandle::at(&parent, path.file_name().unwrap()).map_err(|e| e.to_string())
}

fn read_back(ctx: &Ctx, path: &Path, payload: &[u8]) -> Result<(), String> {
    let before = ctx.fetches();
    let content = ctx.read(path)?;
    if content != payload {
        return Err(format!("the reader of {} got {content:?}", path.display()));
    }
    if ctx.fetches() != before + 1 {
        return Err(format!("expected one fetch, saw {}", ctx.fetches() - before));
    }
    Ok(())
}

/// A directory made after the helper's registration walk and
/// before the watcher's is marked for interception by the watcher's walk.
pub fn bring_up_marks_what_the_helper_missed(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    // Made by this process with no `MarkDir`, as if after the helper's walk.
    let dir = ctx.root.join("w4-missed");
    std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
    if dir_mark_present(ctx.helper_pid(), ctx.ino_of(&dir)?) {
        return Err("the helper marked the directory on its own; the check proves nothing".into());
    }
    let (watcher, _rx) = start(ctx)?;
    let outcome = marked_within(ctx, &dir, Instant::now(), Duration::from_secs(5)).map(drop);
    watcher.stop();
    outcome
}

/// Invariant M1 for a directory a user makes: `mkdir`, then at once a
/// placeholder moved into it. The watcher asks the helper to mark the new
/// directory within the event's latency (the window Z2 accepts), and an
/// open of the placeholder is then intercepted and filled.
pub fn new_directory_marked(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let (watcher, rx) = start(ctx)?;
    ctx.place("w4-moved.bin", "ITEM_W4_MOVED", b"MOVED IN")?;
    let dir = ctx.root.join("w4-new");
    let since = Instant::now();
    sh(ctx, "mkdir w4-new && mv w4-moved.bin w4-new/")?;
    let took = marked_within(ctx, &dir, since, Duration::from_secs(5))?;
    checks.note(ctx.fs, "watcher: new directory", &format!("marked for interception {took:?} after the mkdir began"));
    read_back(ctx, &dir.join("w4-moved.bin"), b"MOVED IN")?;
    // Whether the move's new side is reported depends on whether the mark
    // came first; the new directory's whole tree is dirty either way.
    let batch = next(&rx)?;
    watcher.stop();
    shows(&batch, "trees: {\"w4-new\"}")
}

/// Whether `batch` says `what` (its `Debug` form: parts of it race).
fn shows(batch: &Batch, what: &str) -> Result<(), String> {
    let text = format!("{batch:?}");
    (text.contains(what) && !batch.is_full()).then_some(()).ok_or_else(|| format!("handed over {text}, which lacks {what}"))
}

/// A tree moved in from outside the folder: every directory of it is marked
/// before its placeholders are opened.
pub fn tree_moved_in_marked(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let (watcher, rx) = start(ctx)?;
    let placed = ctx.place("w4-leaf.bin", "ITEM_W4_LEAF", b"DEEP INSIDE")?;
    let outside = ctx.outside.join("w4-tree");
    let _ = std::fs::remove_dir_all(&outside);
    std::fs::create_dir_all(outside.join("a/b")).map_err(|e| e.to_string())?;
    std::fs::rename(&placed, outside.join("a/b/leaf.bin")).map_err(|e| e.to_string())?;
    let tree = ctx.root.join("w4-tree");
    let since = Instant::now();
    sh(ctx, &format!("mv '{}' w4-tree", outside.display()))?;
    for dir in [tree.clone(), tree.join("a"), tree.join("a/b")] {
        marked_within(ctx, &dir, since, Duration::from_secs(5))?;
    }
    checks.note(ctx.fs, "watcher: tree moved in", &format!("three levels marked within {:?}", since.elapsed()));
    read_back(ctx, &tree.join("a/b/leaf.bin"), b"DEEP INSIDE")?;
    let batch = next(&rx)?;
    watcher.stop();
    let mut expected = Batch::new();
    expected.name(Path::new(""), OsStr::new("w4-tree"));
    expected.object(handle(&tree)?);
    expected.tree(Path::new("w4-tree"));
    (batch == expected).then_some(()).ok_or_else(|| format!("handed over {batch:?}, expected {expected:?}"))
}

/// §3.2, end to end: a placeholder filled through the helper (the daemon's
/// commit changes size, time and attributes) raises only events with this
/// process's pid, and nothing is handed over to be examined.
pub fn own_fill_is_silent(ctx: &Ctx, _checks: &mut Checks) -> Result<(), String> {
    let (watcher, rx) = start(ctx)?;
    let path = ctx.place("w4-fill.bin", "ITEM_W4_FILL", b"FILLED BY THE DAEMON")?;
    read_back(ctx, &path, b"FILLED BY THE DAEMON")?;
    let outcome = (|| {
        if let Ok(batch) = rx.recv_timeout(QUIET * 4) {
            return Err(format!("the daemon's own placement and fill were handed over: {batch:?}"));
        }
        // The control: a user's change is handed over, and only it.
        sh(ctx, "echo theirs > w4-theirs.txt")?;
        let batch = next(&rx)?;
        let mut expected = Batch::new();
        expected.written(Path::new(""), OsStr::new("w4-theirs.txt"), Some(handle(&ctx.root.join("w4-theirs.txt"))?));
        (batch == expected).then_some(()).ok_or_else(|| format!("handed over {batch:?}, expected {expected:?}"))
    })();
    watcher.stop();
    outcome
}

/// the watcher (F72): a nested Btrfs subvolume is on another device than
/// the folder. Nothing in it is uploaded: it is listed once as
/// `other-device`, and no row is made for it or below it, then or later.
/// The watcher neither watches it nor asks the helper to mark it (which the
/// helper refuses), and `LastError` says so.
pub fn other_device_not_uploaded(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    use konedrive_fs::placeholder::XATTR_ROOT;
    use konedrived::sync::local::{IgnoreList, NoLiveness};
    use konedrived::sync::root::SyncRoot;
    use konedrived::sync::watcher::ExamineSink;
    use konedrived::sync::InodeLocks;
    use konedrived::tree::{Change, Kind, Placement, Row, Store, TreeStore};

    if ctx.fs != "btrfs" {
        checks.note(ctx.fs, "watcher: nested subvolume", "not Btrfs; nothing to check");
        return Ok(());
    }
    // A folder of its own, whose listing completed with only its root, so
    // that the examination runs: the suite's folder is in no tree store.
    let base = ctx.outside.join("w4-other");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    let root = SyncRoot { path: base.clone(), root_id: "6f1e2d3c-4b5a-4968-8776-655443322110".into() };
    xattr::set(&base, XATTR_ROOT, root.root_id.as_bytes()).map_err(|e| e.to_string())?;
    let sh_in = |script: &str| -> Result<(), String> {
        let status = Command::new("sh").arg("-c").arg(script).current_dir(&base).status().map_err(|e| e.to_string())?;
        status.success().then_some(()).ok_or_else(|| format!("`{script}` failed: {status}"))
    };
    sh_in("echo plain > plain.txt && btrfs -q subvolume create sub && echo x > sub/inner.txt && mkdir sub/d")?;
    let store = Store::new(TreeStore::in_memory().map_err(|e| e.to_string())?);
    let top = Row {
        id: "R".into(),
        parent_id: None,
        name: String::new(),
        kind: Kind::Folder,
        size: 0,
        mtime: 0,
        etag: None,
        ctag: None,
        quickxor: None,
        mime: None,
        placement: Placement::Placed,
    };
    store
        .with(|s| {
            s.begin_staging(false)?;
            s.stage(&[Change::Root(top)])?;
            s.commit_staging("w4-link")
        })
        .map_err(|e| e.to_string())?;
    let sink = ExamineSink {
        root: root.clone(),
        store: store.clone(),
        locks: InodeLocks::new(),
        ignore: Arc::new(std::sync::RwLock::new(IgnoreList::default())),
        liveness: Box::new(NoLiveness),
        link: Arc::new(Mutex::new(None)),
        runtime: ctx.runtime.handle().clone(),
        on_rows: None,
        on_handles: None,
        tree_lock: None,
    };
    let mut config = WatchConfig::new(root, Arc::new(Mutex::new(None)), ctx.runtime.handle().clone());
    config.timing = Timing { quiet: QUIET, ceiling: Duration::from_secs(5), retry: Duration::from_secs(1), ..Timing::default() };
    let watcher = Watcher::start(config, Box::new(sink)).map_err(|e| format!("the watcher did not start: {e}"))?;
    let rows = || -> Result<Vec<String>, String> {
        let rows = store.with(|s| s.outbox_rows()).map_err(|e| e.to_string())?;
        Ok(rows.iter().map(|r| format!("{:?} {}", r.kind, r.rel.display())).collect())
    };
    let outcome = (|| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while rows()?.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if rows()? != ["Create plain.txt"] {
            return Err(format!("rows {:?}; only plain.txt may be uploaded", rows()?));
        }
        let skipped: Vec<(String, String)> = store
            .with(|s| s.local_skipped())
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|s| (s.rel.display().to_string(), s.reason))
            .collect();
        if skipped != [("sub".to_owned(), "other-device".to_owned())] {
            return Err(format!("listed as not uploaded: {skipped:?}"));
        }
        let status = watcher.status();
        if (status.other_device, status.groups) != (1, 1) || !status.note().unwrap_or_default().contains("another device") {
            return Err(format!("the watcher says {status:?}, {:?}", status.note()));
        }
        // Later changes inside it are not looked at either.
        sh_in("echo y > sub/later.txt && mkdir sub/e")?;
        std::thread::sleep(QUIET * 4);
        if rows()? != ["Create plain.txt"] {
            return Err(format!("rows {:?} after a change inside the subvolume", rows()?));
        }
        // What the helper answers when asked all the same (F72).
        let answer = File::open(base.join("sub/d"))
            .map_err(|e| e.to_string())
            .and_then(|dir| ctx.runtime.block_on(ctx.link()?.mark_dir(&dir)).map_err(|e| e.to_string()));
        checks.note(ctx.fs, "watcher: nested subvolume", &format!("MarkDir inside it answers {answer:?}"));
        Ok(())
    })();
    watcher.stop();
    let _ = sh_in("rm -rf sub/* && btrfs -q subvolume delete sub");
    outcome
}
