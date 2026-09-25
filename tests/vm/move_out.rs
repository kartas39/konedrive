//! Moves out of the folder (`docs/design/writes.md` §8, §10, §12) against the real helper: what
//! needs its marks and its `OpenByHandle`, and so cannot run on the host.
//!
//! This process is the daemon, as everywhere in the suite: the examination (with the helper's
//! liveness) finds the move out, and the outbox worker runs the `move-out` row, over this
//! process's helper link. Fills come from the suite's own content source. OneDrive is a fake: a
//! small HTTP server on the loopback in this process, which answers `DELETE` and, at the moment a
//! `DELETE` arrives, checks the moved-out object on disk — the whole point is that the item goes
//! only once the content is local. Users' moves are made by `sh` children; readers are children
//! too, so that their opens are intercepted.
//!
//! Each scenario keeps to a directory of its own in the suite's folder, which its base knows as
//! a folder item: the examination looks at that directory only, never at what the other
//! scenarios left in the folder.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{State, XATTR_ITEM_ID};
use konedrived::drive::DriveClient;
use konedrived::sync::disk::Disk;
use konedrived::sync::helper::HelperLink;
use konedrived::sync::local::{Batch, Examiner, HelperLiveness, IgnoreList};
use konedrived::sync::source::ContentSource;
use konedrived::sync::upload::move_out::{Linked, MoveOuts, SourceFill};
use konedrived::sync::upload::{Limits, NoHost, OutboxWorker, WorkerConfig};
use konedrived::token::StaticToken;
use konedrived::tree::outbox::{OutboxKind, OutboxState};
use konedrived::tree::{Change, Kind, Placement, Row, Store, TreeStore};

use crate::{dir_mark_present, Checks, Ctx};

const ROOT_ID: &str = "ROOT_MO";
const WITHIN: Duration = Duration::from_secs(30);

/// What the fake OneDrive saw: each `DELETE`'s item id and `If-Match`, and what the check made of
/// the disk at that moment.
#[derive(Default)]
struct Seen {
    deletes: Vec<(String, String, Result<(), String>)>,
}

type Check = Box<dyn Fn(&str) -> Result<(), String> + Send>;

/// A fake OneDrive on the loopback: `DELETE /me/drive/items/{id}` answers `204` after `check`
/// looked at the disk; anything else answers `404`.
fn fake_onedrive(check: Check) -> Result<(String, Arc<Mutex<Seen>>), String> {
    // The guest may bring the loopback up only on request.
    let _ = Command::new("ip").args(["link", "set", "lo", "up"]).status();
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no loopback: {e}"))?;
    let url = format!("http://{}/", listener.local_addr().map_err(|e| e.to_string())?);
    let seen = Arc::new(Mutex::new(Seen::default()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = answer(stream, &check, &log);
        }
    });
    Ok((url, seen))
}

fn answer(mut stream: TcpStream, check: &Check, seen: &Mutex<Seen>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let first: Vec<&str> = lines.next().unwrap_or_default().split(' ').collect();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())))
        .collect();
    let length: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    while buf.len() < head_end + length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let (method, path) = (first.first().copied().unwrap_or_default(), first.get(1).copied().unwrap_or_default());
    let reply = match (method, path.rsplit_once("/items/")) {
        ("DELETE", Some((_, id))) => {
            let verdict = check(id);
            let guard = headers.get("if-match").cloned().unwrap_or_default();
            seen.lock().unwrap().deletes.push((id.to_owned(), guard, verdict));
            "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".to_owned()
        }
        _ => {
            let body = r#"{"error":{"code":"itemNotFound","message":"none"}}"#;
            format!(
                "HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        }
    };
    stream.write_all(reply.as_bytes())?;
    stream.flush()
}

fn base_row(id: &str, parent: Option<&str>, name: &str, kind: Kind, size: u64) -> Row {
    Row {
        id: id.into(),
        parent_id: parent.map(str::to_owned),
        name: name.into(),
        kind,
        size,
        mtime: 1_700_000_000,
        etag: Some(format!("e-{id}")),
        ctag: Some(format!("c-{id}")),
        quickxor: None,
        mime: None,
        placement: Placement::Placed,
    }
}

fn handle_of(path: &Path) -> Result<FileHandle, String> {
    let dir = File::open(path.parent().unwrap()).map_err(|e| e.to_string())?;
    FileHandle::at(&dir, path.file_name().unwrap()).map_err(|e| format!("no handle for {}: {e}", path.display()))
}

/// One base item: id, parent id (`None`: the scenario's own directory), path relative to the
/// root, kind, size.
type Item<'a> = (&'a str, Option<&'a str>, &'a str, Kind, u64);

/// The suite's folder as a read-write folder's base knows it: the scenario's directory (`home`,
/// directly in the root) and `items`, all placed, with their handles recorded; a listing has
/// completed.
struct Base {
    store: Store,
    link: Arc<Mutex<Option<HelperLink>>>,
}

impl Base {
    fn new(ctx: &Ctx, home: &str, items: &[Item<'_>]) -> Result<Self, String> {
        let home_id = format!("ITEM_{home}");
        xattr::set(ctx.root.join(home), XATTR_ITEM_ID, home_id.as_bytes()).map_err(|e| e.to_string())?;
        let store = Store::new(TreeStore::in_memory().map_err(|e| e.to_string())?);
        let mut changes = vec![
            Change::Root(base_row(ROOT_ID, None, "", Kind::Folder, 0)),
            Change::Upsert(base_row(&home_id, Some(ROOT_ID), home, Kind::Folder, 0)),
        ];
        for (id, parent, rel, kind, size) in items {
            let name = Path::new(rel).file_name().unwrap().to_str().unwrap();
            if *kind == Kind::Folder {
                xattr::set(ctx.root.join(rel), XATTR_ITEM_ID, id.as_bytes()).map_err(|e| e.to_string())?;
            }
            changes.push(Change::Upsert(base_row(id, Some(parent.unwrap_or(&home_id)), name, *kind, *size)));
        }
        store
            .with(|s| {
                s.begin_staging(false)?;
                s.stage(&changes)?;
                s.commit_staging("link-move-out")
            })
            .map_err(|e| e.to_string())?;
        let mut placed = vec![(home_id.as_str(), home)];
        placed.extend(items.iter().map(|(id, _, rel, _, _)| (*id, *rel)));
        for (id, rel) in placed {
            let handle = handle_of(&ctx.root.join(rel))?;
            store.with(|s| s.set_local_handle(id, Some(&handle))).map_err(|e| e.to_string())?;
        }
        Ok(Self { store, link: Arc::new(Mutex::new(Some(ctx.link()?))) })
    }

    /// The examination of `names` (directory, name) with the helper's liveness: its rows.
    fn examine(&self, ctx: &Ctx, names: &[(&str, &str)]) -> Result<Vec<(OutboxKind, Option<String>)>, String> {
        let mut batch = Batch::new();
        for (dir, name) in names {
            batch.name(Path::new(dir), std::ffi::OsStr::new(name));
        }
        let disk = Disk::open(&ctx.sync_root(), false).map_err(|e| e.to_string())?;
        let liveness = HelperLiveness::new(Arc::new(Linked(Arc::clone(&self.link))), ctx.sync_root(), ctx.runtime.handle().clone());
        let ignore = IgnoreList::default();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        Examiner { disk: &disk, store: &self.store, liveness: &liveness, ignore: &ignore, locks: &ctx.locks, now }
            .examine(&batch)
            .map_err(|e| format!("the examination failed: {e}"))?;
        let rows = self.store.with(|s| s.outbox_rows()).map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(|r| (r.kind, r.item_id)).collect())
    }

    /// An outbox worker on this base, against the fake OneDrive at `url`, paused or not.
    fn worker(&self, ctx: &Ctx, url: &str, paused: bool) -> Result<OutboxWorker, String> {
        let drive = DriveClient::new(url::Url::parse(url).map_err(|e| e.to_string())?, Arc::new(StaticToken::new("T")))
            .map_err(|e| e.to_string())?;
        let worker = OutboxWorker::new(WorkerConfig {
            root: ctx.sync_root(),
            store: self.store.clone(),
            drive,
            locks: ctx.locks.clone(),
            machine_name: "vm".into(),
            tree_lock: Arc::new(tokio::sync::Mutex::new(())),
            host: Arc::new(NoHost),
            limits: Limits::default(),
            moved_out: Some(MoveOuts {
                helper: Arc::new(Linked(Arc::clone(&self.link))),
                filler: Arc::new(SourceFill(Arc::clone(&ctx.source) as Arc<dyn ContentSource>)),
                route: None,
                home_trash: None,
                roots: {
                    let root = ctx.root.clone();
                    Arc::new(move || vec![root.clone()])
                },
            }),
        });
        if paused {
            worker.pause(None).map_err(|e| e.to_string())?;
        }
        let _runtime = ctx.runtime.enter();
        worker.start();
        Ok(worker)
    }

    fn stop(&self, ctx: &Ctx, worker: OutboxWorker) {
        ctx.runtime.block_on(worker.stop());
    }

    /// Waits until the outbox is empty.
    fn drained(&self, within: Duration) -> Result<(), String> {
        let started = Instant::now();
        loop {
            let rows = self.store.with(|s| s.outbox_rows()).map_err(|e| e.to_string())?;
            if rows.is_empty() {
                return Ok(());
            }
            if started.elapsed() > within {
                let what: Vec<String> = rows.iter().map(|r| format!("{} {:?} {:?}", r.kind.as_str(), r.state, r.reason)).collect();
                return Err(format!("the outbox still holds {what:?}"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// `script` run by `sh` in the folder: another process, as a user is.
fn sh(ctx: &Ctx, script: &str) -> Result<(), String> {
    let status = Command::new("sh").arg("-c").arg(script).current_dir(&ctx.root).status().map_err(|e| e.to_string())?;
    status.success().then_some(()).ok_or_else(|| format!("`{script}` failed: {status}"))
}

fn konedrive_attrs(path: &Path) -> Vec<String> {
    xattr::list(path)
        .map(|names| names.filter_map(|n| n.to_str().map(str::to_owned)).filter(|n| n.starts_with("user.konedrive.")).collect())
        .unwrap_or_default()
}

/// The check a `DELETE` makes of each file: its content is `payload`, and it is an ordinary file
/// now — read by this process, which the helper lets through, so what is read is what is there.
fn local_and_ordinary(path: &Path, payload: &[u8]) -> Result<(), String> {
    let content = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if content != payload {
        return Err(format!("{} holds {} bytes that are not its content", path.display(), content.len()));
    }
    let attrs = konedrive_attrs(path);
    if !attrs.is_empty() {
        return Err(format!("{} still carries {attrs:?}", path.display()));
    }
    Ok(())
}

fn one_delete(seen: &Mutex<Seen>, id: &str, guard: &str) -> Result<(), String> {
    let seen = seen.lock().unwrap();
    match seen.deletes.as_slice() {
        [(got, got_guard, verdict)] if got == id && got_guard == guard => verdict.clone().map_err(|why| format!("at the DELETE: {why}")),
        other => Err(format!("OneDrive got {:?}, not one DELETE of {id} guarded by {guard}", other.iter().map(|(i, g, _)| (i, g)).collect::<Vec<_>>())),
    }
}

fn wait_for(what: &str, within: Duration, mut done: impl FnMut() -> bool) -> Result<(), String> {
    let started = Instant::now();
    while started.elapsed() < within {
        if done() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(format!("{what} did not happen within {within:?}"))
}

/// §4.6, §11: a placeholder moved out of the folder is marked again before anything else runs —
/// here by a paused worker, which sends nothing — so a reader gets its content, never zeros; the
/// item is deleted in OneDrive only once the file is local and an ordinary file.
pub fn placeholder_moved_out(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let payload = vec![0x5a_u8; 96 * 1024];
    ctx.place("mo-file/p.bin", "ITEM_MO_FILE", &payload)?;
    let base = Base::new(ctx, "mo-file", &[("ITEM_MO_FILE", None, "mo-file/p.bin", Kind::File, payload.len() as u64)])?;
    let moved = ctx.outside.join("mo-file.bin");
    let _ = std::fs::remove_file(&moved);
    sh(ctx, &format!("mv mo-file/p.bin '{}'", moved.display()))?;
    let rows = base.examine(ctx, &[("mo-file", "p.bin")])?;
    if rows != vec![(OutboxKind::MoveOut, Some("ITEM_MO_FILE".to_owned()))] {
        return Err(format!("the examination made {rows:?}, not one move-out"));
    }
    let ino = ctx.ino_of(&moved)?;
    let expect = payload.clone();
    let check_at = moved.clone();
    let (url, seen) = fake_onedrive(Box::new(move |_| local_and_ordinary(&check_at, &expect)))?;

    let worker = base.worker(ctx, &url, true)?;
    let outcome = (|| {
        wait_for("the re-mark", Duration::from_secs(10), || dir_mark_present(ctx.helper_pid(), ino))?;
        let before = ctx.fetches();
        let content = ctx.read(&moved)?;
        if content != payload {
            return Err(format!("a reader of the moved-out placeholder got {} bytes that are not its content", content.len()));
        }
        if ctx.fetches() != before + 1 {
            return Err(format!("expected one fetch for the reader, saw {}", ctx.fetches() - before));
        }
        if !seen.lock().unwrap().deletes.is_empty() {
            return Err("a paused worker deleted the item".into());
        }
        worker.resume().map_err(|e| e.to_string())?;
        base.drained(WITHIN)?;
        one_delete(&seen, "ITEM_MO_FILE", "e-ITEM_MO_FILE")
    })();
    base.stop(ctx, worker);
    outcome?;
    checks.note(ctx.fs, "move-out", "a placeholder: re-marked while paused, read whole, then deleted in OneDrive");
    let _ = std::fs::remove_file(&moved);
    Ok(())
}

/// §4.6: a directory moved out has each placeholder of its item downloaded where it went by the
/// worker itself, every directory unmarked, the attributes taken off, and only then the folder is
/// deleted in OneDrive, whole and with no guard at all.
pub fn directory_moved_out(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let (a, b) = (b"ALPHA IN THE FOLDER".to_vec(), vec![0x42_u8; 200 * 1024]);
    ctx.place("mo-dirs/d/a.bin", "ITEM_MO_A", &a)?;
    ctx.place("mo-dirs/d/sub/b.bin", "ITEM_MO_B", &b)?;
    let base = Base::new(
        ctx,
        "mo-dirs",
        &[
            ("ITEM_MO_DIR", None, "mo-dirs/d", Kind::Folder, 0),
            ("ITEM_MO_A", Some("ITEM_MO_DIR"), "mo-dirs/d/a.bin", Kind::File, a.len() as u64),
            ("ITEM_MO_SUB", Some("ITEM_MO_DIR"), "mo-dirs/d/sub", Kind::Folder, 0),
            ("ITEM_MO_B", Some("ITEM_MO_SUB"), "mo-dirs/d/sub/b.bin", Kind::File, b.len() as u64),
        ],
    )?;
    let moved = ctx.outside.join("mo-dir");
    let _ = std::fs::remove_dir_all(&moved);
    sh(ctx, &format!("mv mo-dirs/d '{}'", moved.display()))?;
    let rows = base.examine(ctx, &[("mo-dirs", "d")])?;
    if rows != vec![(OutboxKind::MoveOut, Some("ITEM_MO_DIR".to_owned()))] {
        return Err(format!("the examination made {rows:?}, not one move-out of the folder"));
    }
    let inos = [ctx.ino_of(&moved)?, ctx.ino_of(&moved.join("sub"))?];
    if !inos.iter().all(|&ino| dir_mark_present(ctx.helper_pid(), ino)) {
        return Err("the directories' marks did not travel with them".into());
    }
    let (check_a, check_b, pa, pb) = (moved.join("a.bin"), moved.join("sub/b.bin"), a.clone(), b.clone());
    let (url, seen) = fake_onedrive(Box::new(move |_| {
        local_and_ordinary(&check_a, &pa)?;
        local_and_ordinary(&check_b, &pb)
    }))?;
    let before = ctx.fetches();
    let worker = base.worker(ctx, &url, false)?;
    let outcome = base.drained(WITHIN).and_then(|()| one_delete(&seen, "ITEM_MO_DIR", ""));
    base.stop(ctx, worker);
    outcome?;
    if ctx.fetches() != before + 2 {
        return Err(format!("expected two downloads, saw {}", ctx.fetches() - before));
    }
    for (dir, ino) in [moved.clone(), moved.join("sub")].iter().zip(inos) {
        if dir_mark_present(ctx.helper_pid(), ino) {
            return Err(format!("{} is still marked", dir.display()));
        }
        if !konedrive_attrs(dir).is_empty() {
            return Err(format!("{} still carries {:?}", dir.display(), konedrive_attrs(dir)));
        }
    }
    let content = ctx.read(&moved.join("sub/b.bin"))?;
    if content != b {
        return Err("a reader of the downloaded file got something else".into());
    }
    checks.note(ctx.fs, "move-out", "a directory: two files downloaded where they went, unmarked, then the folder deleted");
    let _ = std::fs::remove_dir_all(&moved);
    Ok(())
}

/// §4.6, the Trash: a placeholder sent to the Trash is removed from it with its `.trashinfo`, and
/// deleted in OneDrive, without a download.
pub fn placeholder_to_the_trash(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    ctx.place("mo-trash/t.bin", "ITEM_MO_TRASH", b"NEVER DOWNLOADED")?;
    let base = Base::new(ctx, "mo-trash", &[("ITEM_MO_TRASH", None, "mo-trash/t.bin", Kind::File, 16)])?;
    // A mount's Trash is at the mount's top (`/mnt/<fs>`), as a desktop puts it; one anywhere
    // else would be an ordinary directory.
    let top = ctx.outside.ancestors().find(|p| p.parent() == Some(Path::new("/mnt"))).ok_or("the suite is not below /mnt")?;
    let trash = top.join(format!(".Trash-{}", nix::unistd::geteuid().as_raw()));
    let (files, info) = (trash.join("files"), trash.join("info"));
    std::fs::create_dir_all(&files).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&info).map_err(|e| e.to_string())?;
    let trashed = files.join("mo-trash.bin");
    let trashinfo = info.join("mo-trash.bin.trashinfo");
    std::fs::write(&trashinfo, "[Trash Info]\nPath=mo-trash.bin\n").map_err(|e| e.to_string())?;
    sh(ctx, &format!("mv mo-trash/t.bin '{}'", trashed.display()))?;
    let rows = base.examine(ctx, &[("mo-trash", "t.bin")])?;
    if rows != vec![(OutboxKind::MoveOut, Some("ITEM_MO_TRASH".to_owned()))] {
        return Err(format!("the examination made {rows:?}, not one move-out"));
    }
    let (check_file, check_info) = (trashed.clone(), trashinfo.clone());
    let (url, seen) = fake_onedrive(Box::new(move |_| {
        if check_file.exists() || check_info.exists() {
            return Err("the placeholder or its .trashinfo is still in the Trash".into());
        }
        Ok(())
    }))?;
    let before = ctx.fetches();
    let worker = base.worker(ctx, &url, false)?;
    let outcome = base.drained(WITHIN).and_then(|()| one_delete(&seen, "ITEM_MO_TRASH", "e-ITEM_MO_TRASH"));
    base.stop(ctx, worker);
    outcome?;
    if ctx.fetches() != before {
        return Err(format!("the Trash case downloaded {} time(s)", ctx.fetches() - before));
    }
    checks.note(ctx.fs, "move-out", "the Trash: the placeholder and its .trashinfo removed, no download, deleted in OneDrive");
    Ok(())
}

/// §5: a download that stops part-way deletes nothing; after a restart (a new worker, the helper
/// restarted, so every mark is gone) the placeholder is marked again first — the new worker is
/// paused — and then downloaded whole, and only then deleted.
pub fn crash_mid_download_then_restart(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let payload: Vec<u8> = (0..256 * 1024u32).map(|i| (i % 251) as u8).collect();
    ctx.place("mo-crash/c.bin", "ITEM_MO_CRASH", &payload)?;
    let base = Base::new(ctx, "mo-crash", &[("ITEM_MO_CRASH", None, "mo-crash/c.bin", Kind::File, payload.len() as u64)])?;
    let moved = ctx.outside.join("mo-crash.bin");
    let _ = std::fs::remove_file(&moved);
    sh(ctx, &format!("mv mo-crash/c.bin '{}'", moved.display()))?;
    base.examine(ctx, &[("mo-crash", "c.bin")])?;
    let expect = payload.clone();
    let check_at = moved.clone();
    let (url, seen) = fake_onedrive(Box::new(move |_| local_and_ordinary(&check_at, &expect)))?;

    // Every fetch breaks after 64 KiB: the worker's download fails part-way.
    ctx.source.fail_at.store(64 * 1024, Ordering::SeqCst);
    ctx.source.fail_once.store(false, Ordering::SeqCst);
    let worker = base.worker(ctx, &url, false)?;
    let failed = wait_for("a failed download", WITHIN, || {
        base.store.with(|s| s.outbox_rows()).is_ok_and(|rows| {
            rows.iter().any(|r| r.state == OutboxState::Retry && r.reason.as_deref().is_some_and(|w| w.starts_with("download-failed")))
        })
    });
    base.stop(ctx, worker);
    ctx.source.reset();
    failed?;
    if !seen.lock().unwrap().deletes.is_empty() {
        return Err("the item was deleted after a download that stopped part-way".into());
    }
    // `online-only` after the roll-back, or `hydrating` if the stop cut a second attempt short:
    // either way not local.
    let left = ctx.state_of(&moved)?;
    if !matches!(left, Some(State::OnlineOnly | State::Hydrating)) {
        return Err(format!("the file is {left:?} after the failed download"));
    }
    let ino = ctx.ino_of(&moved)?;
    if !dir_mark_present(ctx.helper_pid(), ino) {
        return Err("the first worker did not mark the moved-out placeholder".into());
    }

    // The restart: a new helper, with no marks at all, and a new worker on the same store.
    ctx.restart_helper()?;
    *base.link.lock().unwrap() = Some(ctx.link()?);
    if dir_mark_present(ctx.helper_pid(), ino) {
        return Err("the restarted helper still marks the file; the check proves nothing".into());
    }
    base.store.with(|s| s.outbox_retry_now()).map_err(|e| e.to_string())?;
    let worker = base.worker(ctx, &url, true)?;
    let outcome = (|| {
        wait_for("the re-mark after the restart", Duration::from_secs(10), || dir_mark_present(ctx.helper_pid(), ino))?;
        if !seen.lock().unwrap().deletes.is_empty() {
            return Err("the paused worker deleted the item".into());
        }
        worker.resume().map_err(|e| e.to_string())?;
        base.drained(WITHIN)?;
        one_delete(&seen, "ITEM_MO_CRASH", "e-ITEM_MO_CRASH")
    })();
    base.stop(ctx, worker);
    outcome?;
    let meta = std::fs::metadata(&moved).map_err(|e| e.to_string())?;
    checks.note(
        ctx.fs,
        "move-out",
        &format!(
            "a crash mid-download: left {left:?}, nothing deleted; after a restart re-marked first, then {} bytes local and deleted",
            meta.size()
        ),
    );
    let _ = std::fs::remove_file(&moved);
    Ok(())
}
