//! The write phase end to end (`docs/design/writes.md` §12, "VM, `quick` (Btrfs), with a fake Graph server
//! in the guest"): a read-write folder brought up by the daemon's own `SyncService` —
//! the real helper, the watcher, the examination and the outbox worker — against the fake
//! OneDrive the worker's host tests use (`konedrived::sync::upload::fake`, on wiremock, listening
//! on the guest's loopback; `fault-injection` builds it).
//!
//! The folder shares the suite's helper connection: an intercepted open in it is filled by the
//! suite's own content source, from `source/<item id>`, which each scenario fills for what it
//! seeds. Users' changes are made by `sh` children, so that their opens are intercepted and their
//! events are not the daemon's own. Each scenario has a folder of its own, forgotten at its end.

use std::fs::File;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use konedrive_fs::placeholder::{read_state, State};
use konedrived::config::{ConfigError, Mode};
use konedrived::state::{AccountSnapshot, SignInState, StateHandle};
use konedrived::sync::upload::fake::{FakeGraph, FakeItem, ROOT};
use konedrived::sync::{SyncPaths, SyncService};

use crate::{dir_mark_present, Checks, Ctx};

/// The drive the fake OneDrive answers `GET /me/drive` with.
const FAKE_DRIVE: &str = "D";

const WITHIN: Duration = Duration::from_secs(40);

/// What the fake OneDrive holds at the start: folders (id, name) under the root, then files (id,
/// path below the root with folders by name, content).
struct Seed<'a> {
    folders: &'a [(&'a str, &'a str)],
    files: &'a [(&'a str, &'a str, &'a [u8])],
}

/// One read-write folder, its daemon side, and its fake OneDrive.
struct World<'c> {
    ctx: &'c Ctx,
    base: PathBuf,
    folder: PathBuf,
    graph: FakeGraph,
    service: Arc<SyncService>,
}

impl<'c> World<'c> {
    fn new(ctx: &'c Ctx, tag: &str, seed: Seed<'_>) -> Result<Self, String> {
        // The fake OneDrive listens on the guest's loopback.
        let _ = Command::new("ip").args(["link", "set", "lo", "up"]).status();
        ctx.source.reset();
        let base = ctx.outside.parent().ok_or("the suite has no base")?.join(format!("w9-{tag}"));
        let _ = std::fs::remove_dir_all(&base);
        let folder = base.join("OneDrive");
        std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
        let graph = ctx.runtime.block_on(FakeGraph::start());
        graph.with(|cloud| {
            for (id, name) in seed.folders {
                cloud.add(FakeItem {
                    id: (*id).into(),
                    parent: Some(ROOT.into()),
                    name: (*name).into(),
                    folder: true,
                    content: Vec::new(),
                    hash: None,
                    size: 0,
                    etag: format!("e-{id}"),
                    ctag: format!("c-{id}"),
                    mtime: 0,
                });
            }
            for (id, path, content) in seed.files {
                let (parent, name) = match path.rsplit_once('/') {
                    Some((dir, name)) => (seed.folders.iter().find(|(_, n)| *n == dir).map_or(ROOT, |(id, _)| *id), name),
                    None => (ROOT, *path),
                };
                cloud.add_file(id, parent, name, content);
            }
        });
        for (id, _, content) in seed.files {
            std::fs::write(ctx.source_dir.join(id), content).map_err(|e| e.to_string())?;
        }
        let persist = ctx.runtime.block_on(crate::one_account(&base))?;
        // The write gate open for the fake drive, as a read-write account on the list has it
        // (write design §2.3): `config.toml` says read-write, lists the drive and records it as
        // the account's; the account runs read-write, its token can write, and was seen to reach
        // that drive. The worker asks all of it before each row.
        persist
            .store
            .update(|config| {
                config.write_test_drive_ids = vec![FAKE_DRIVE.into()];
                let account = config
                    .accounts
                    .iter_mut()
                    .find(|a| a.id == persist.account)
                    .ok_or_else(|| ConfigError::NoAccount(persist.account.clone()))?;
                account.mode = Mode::ReadWrite;
                account.drive_id = FAKE_DRIVE.into();
                Ok::<_, ConfigError>(())
            })
            .map_err(|e| format!("cannot open the write gate in config.toml: {e}"))?;
        let account = StateHandle::new(AccountSnapshot {
            state: SignInState::SignedIn,
            mode: Mode::ReadWrite,
            granted_scopes: "Files.ReadWrite offline_access".into(),
            live_drive: FAKE_DRIVE.into(),
            ..AccountSnapshot::default()
        });
        let service = SyncService::new(Some(ctx.link()?), Some(account), Some(persist));
        service.set_drive(graph.client());
        service.set_sync_paths(SyncPaths { tree_db: base.join("tree.sqlite"), rescue_dir: base.join("rescued"), thumbnails: None });
        service.start_in_mode(Mode::ReadWrite);
        ctx.runtime.block_on(service.register_root(&folder)).map_err(|e| format!("cannot register {}: {e}", folder.display()))?;
        let world = World { ctx, base, folder, graph, service };
        world.wait("the first cycle", || (world.service.status().0 > 0).then_some(()))?;
        world.wait("the lock lifted", || {
            std::fs::metadata(&world.folder).is_ok_and(|m| m.mode() & 0o200 != 0).then_some(())
        })?;
        for (_, path, _) in seed.files {
            world.wait(&format!("{path} placed"), || world.path(path).exists().then_some(()))?;
        }
        Ok(world)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.folder.join(rel)
    }

    /// `script`, run by `sh` in the folder: another process, as a user is.
    fn sh(&self, script: &str) -> Result<(), String> {
        let status = Command::new("sh").arg("-c").arg(script).current_dir(&self.folder).status().map_err(|e| e.to_string())?;
        status.success().then_some(()).ok_or_else(|| format!("`{script}` failed: {status}"))
    }

    fn wait<T>(&self, what: &str, mut done: impl FnMut() -> Option<T>) -> Result<T, String> {
        let started = Instant::now();
        loop {
            if let Some(value) = done() {
                return Ok(value);
            }
            if started.elapsed() > WITHIN {
                let rows = self.ctx.runtime.block_on(self.service.outbox(0)).unwrap_or_default();
                let rows: Vec<String> = rows.iter().map(|r| format!("{r:?}")).collect();
                let paths = self.graph.with(|c| c.paths());
                return Err(format!("{what}: not within {WITHIN:?}; OneDrive holds {paths:?}; the outbox {rows:?}"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The content OneDrive holds at `path`, and its item id.
    fn cloud(&self, path: &str) -> Option<(String, Vec<u8>)> {
        self.graph.with(|c| c.at(path).map(|i| (i.id.clone(), i.content.clone())))
    }

    /// Waits until OneDrive holds `content` at `path`; its item id.
    fn uploaded(&self, path: &str, content: &[u8]) -> Result<String, String> {
        self.wait(&format!("{path} in OneDrive"), || self.cloud(path).filter(|(_, c)| c == content).map(|(id, _)| id))
    }

    fn outbox_empty(&self) -> bool {
        self.ctx.runtime.block_on(self.service.outbox(0)).is_ok_and(|rows| rows.is_empty())
    }

    /// Requests that change OneDrive, so far.
    fn writes(&self) -> usize {
        self.graph.with(|c| c.log.iter().filter(|(method, _)| method != "GET").count())
    }

    /// The folder forgotten, the fake stopped, the directory removed.
    fn finish(self) {
        let _ = self.ctx.runtime.block_on(self.service.unregister_root());
        drop(self.graph);
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn state_of(path: &Path) -> Option<State> {
    File::open(path).ok().and_then(|f| read_state(&f).ok().flatten())
}

/// Runs `body` on a fresh world, and forgets it whatever came of it.
fn with_world(
    ctx: &Ctx,
    tag: &str,
    seed: Seed<'_>,
    body: impl FnOnce(&World<'_>) -> Result<String, String>,
    checks: &mut Checks,
    name: &str,
) -> Result<(), String> {
    let world = World::new(ctx, tag, seed)?;
    let outcome = body(&world);
    world.finish();
    let note = outcome?;
    checks.note(ctx.fs, name, &note);
    Ok(())
}

/// §3.8, §11: a write open of a placeholder is intercepted and filled first; the write lands on
/// the whole content, and the upload carries it, to the same item.
pub fn write_open_fills_then_uploads(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let cloud = b"CLOUD VERSION\n".as_slice();
    let seed = Seed { folders: &[], files: &[("W9-DOC", "doc.txt", cloud)] };
    with_world(ctx, "write-open", seed, |w| {
        if state_of(&w.path("doc.txt")) != Some(State::OnlineOnly) {
            return Err("doc.txt is not a placeholder to begin with".into());
        }
        let before = ctx.fetches();
        w.sh("printf 'LOCAL LINE\\n' >> doc.txt")?;
        if ctx.fetches() != before + 1 {
            return Err(format!("the write open fetched {} time(s), not once", ctx.fetches() - before));
        }
        let expected = b"CLOUD VERSION\nLOCAL LINE\n";
        let on_disk = std::fs::read(w.path("doc.txt")).map_err(|e| e.to_string())?;
        if on_disk != expected {
            return Err(format!("the file holds {:?}", String::from_utf8_lossy(&on_disk)));
        }
        let id = w.uploaded("doc.txt", expected)?;
        if id != "W9-DOC" {
            return Err(format!("uploaded as a new item {id}, not as an update of W9-DOC"));
        }
        Ok("filled on the write open, then uploaded as an update of the same item".into())
    }, checks, "writes: write open")
}

/// Z2, §3.3, §11: a directory made and at once given a placeholder is marked (`MarkDir`) before
/// the placeholder is opened, so the open is filled; OneDrive gets the folder and the move.
pub fn new_directory_marked_then_uploaded(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let content = b"PLACEHOLDER MOVED IN".as_slice();
    let seed = Seed { folders: &[], files: &[("W9-P", "p.bin", content)] };
    with_world(ctx, "new-dir", seed, |w| {
        let started = Instant::now();
        w.sh("mkdir fresh && mv p.bin fresh/")?;
        let ino = ctx.ino_of(&w.path("fresh"))?;
        w.wait("fresh marked", || dir_mark_present(ctx.helper_pid(), ino).then_some(()))?;
        let marked = started.elapsed();
        let read = ctx.read(&w.path("fresh/p.bin"))?;
        if read != content {
            return Err(format!("the reader got {} bytes that are not the content", read.len()));
        }
        w.wait("the move in OneDrive", || w.cloud("fresh/p.bin").filter(|(id, _)| id == "W9-P").map(drop))?;
        Ok(format!("marked {marked:?} after the mkdir began, read whole, folder made and item moved in OneDrive"))
    }, checks, "writes: new directory")
}

/// §3.3, §4.5, §11: a tree moved into the folder from outside has every directory marked, and
/// everything in it uploaded.
pub fn tree_moved_in_marked_and_uploaded(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    with_world(ctx, "tree-in", Seed { folders: &[], files: &[] }, |w| {
        let outside = w.base.join("t");
        std::fs::create_dir_all(outside.join("a/b")).map_err(|e| e.to_string())?;
        std::fs::write(outside.join("top.txt"), b"TOP").map_err(|e| e.to_string())?;
        std::fs::write(outside.join("a/b/leaf.txt"), b"LEAF").map_err(|e| e.to_string())?;
        w.sh(&format!("mv '{}' t", outside.display()))?;
        for dir in ["t", "t/a", "t/a/b"] {
            let ino = ctx.ino_of(&w.path(dir))?;
            w.wait(&format!("{dir} marked"), || dir_mark_present(ctx.helper_pid(), ino).then_some(()))?;
        }
        w.uploaded("t/top.txt", b"TOP")?;
        w.uploaded("t/a/b/leaf.txt", b"LEAF")?;
        w.wait("the outbox empty", || w.outbox_empty().then_some(()))?;
        Ok("three directories marked, both files uploaded under their folders".into())
    }, checks, "writes: tree moved in")
}

/// §4.8, §5: an upload session left half sent — the daemon stopped after its first fragment was
/// accepted — resumes where OneDrive stands when the daemon starts again: the same session, the
/// fragments not sent yet, the whole content in OneDrive.
pub fn stopped_mid_session_resumes(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    with_world(ctx, "session", Seed { folders: &[], files: &[] }, |w| {
        // The second fragment is throttled for a minute: the session is half sent when the
        // daemon goes.
        w.graph.with(|c| c.throttle("PUT", "upload/", 1, 60, 1));
        let size = 25 * 1024 * 1024;
        let content: Vec<u8> = (0..size).map(|i| (i % 253) as u8).collect();
        let staged = w.base.join("big.bin");
        std::fs::write(&staged, &content).map_err(|e| e.to_string())?;
        w.sh(&format!("cp '{}' big.bin", staged.display()))?;
        let fragments = || w.graph.with(|c| c.count("PUT", "upload/"));
        w.wait("the second fragment throttled", || (fragments() >= 2).then_some(()))?;
        // The daemon goes, and comes back: its worker's state is gone, the row's is kept.
        ctx.runtime.block_on(w.service.stop_sync());
        let sessions = w.graph.with(|c| c.count("POST", "createUploadSession"));
        ctx.runtime.block_on(w.service.refresh()).map_err(|e| format!("the sync did not start again: {e}"))?;
        w.uploaded("big.bin", &content)?;
        let (opened, asked, sent) = w.graph.with(|c| (c.count("POST", "createUploadSession"), c.count("GET", "upload/"), c.count("PUT", "upload/")));
        if opened != sessions || opened != 1 {
            return Err(format!("{opened} upload session(s) opened, not one"));
        }
        if asked == 0 {
            return Err("the session was not asked where it stands".into());
        }
        Ok(format!("one session, asked again after the restart, {sent} fragment request(s) in all"))
    }, checks, "writes: session")
}

/// §3.3, §5, §11: after a helper restart, a Full local scan finds a change no event reported —
/// here a write through a hard link outside the folder (F73) — and uploads it.
pub fn helper_restart_full_scan_finds_a_change(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    with_world(ctx, "restart", Seed { folders: &[], files: &[] }, |w| {
        w.sh("printf 'first' > h.txt")?;
        w.uploaded("h.txt", b"first")?;
        w.wait("the outbox empty", || w.outbox_empty().then_some(()))?;
        let link = w.base.join("h-link");
        std::fs::hard_link(w.path("h.txt"), &link).map_err(|e| e.to_string())?;
        let outcome = (|| {
            w.sh(&format!("printf ' second' >> '{}'", link.display()))?;
            std::thread::sleep(Duration::from_secs(4));
            if w.cloud("h.txt").is_some_and(|(_, c)| c != b"first") || !w.outbox_empty() {
                return Err("the write through the outside link was seen before the restart; the check proves nothing".into());
            }
            ctx.restart_helper()?;
            w.service.hub().set_link(Some(ctx.link()?));
            ctx.runtime.block_on(w.service.resume());
            w.uploaded("h.txt", b"first second")?;
            Ok("unseen before the restart; the Full local scan after it found it and uploaded it".to_owned())
        })();
        let _ = std::fs::remove_file(&link);
        outcome
    }, checks, "writes: helper restart")
}

/// §4.1–§4.7, §3.7: a whole round trip through the real helper, watcher and worker — create,
/// edit, rename, move, delete — after which OneDrive holds what the folder holds, and a delta
/// cycle echoing it all back changes nothing.
pub fn round_trip(ctx: &Ctx, checks: &mut Checks) -> Result<(), String> {
    let seed = Seed { folders: &[("W9-DOCS", "Docs")], files: &[("W9-KEEP", "Docs/keep.txt", b"KEEP")] };
    with_world(ctx, "round-trip", seed, |w| {
        w.sh("printf 'one' > rt.txt")?;
        let id = w.uploaded("rt.txt", b"one")?;
        w.sh("printf 'one two' > rt.txt")?;
        let edited = w.uploaded("rt.txt", b"one two")?;
        w.sh("mv rt.txt rt2.txt")?;
        let renamed = w.uploaded("rt2.txt", b"one two")?;
        w.sh("mv rt2.txt Docs/")?;
        let moved = w.uploaded("Docs/rt2.txt", b"one two")?;
        if [&edited, &renamed, &moved].iter().any(|i| **i != id) {
            return Err(format!("one item, {id}, should have been edited, renamed and moved: {edited} {renamed} {moved}"));
        }
        w.sh("mkdir Docs/sub && printf 'deep' > Docs/sub/deep.txt")?;
        w.uploaded("Docs/sub/deep.txt", b"deep")?;
        w.sh("rm Docs/rt2.txt")?;
        w.wait("the delete in OneDrive", || w.graph.with(|c| (c.bin.contains_key(&id) && !c.items.contains_key(&id)).then_some(())))?;
        w.wait("the outbox empty", || w.outbox_empty().then_some(()))?;

        // OneDrive holds what the folder holds.
        let local = walk(&w.folder)?;
        let cloud: Vec<(String, Option<usize>)> = w.graph.with(|c| {
            c.paths().into_iter().map(|p| {
                let item = c.at(&p).expect("listed");
                (p, (!item.folder).then_some(item.size as usize))
            }).collect()
        });
        if local != cloud {
            return Err(format!("the folder holds {local:?}, OneDrive {cloud:?}"));
        }

        // The echo: a cycle brings everything back as it is, and nothing changes.
        let (writes, deltas) = (w.writes(), w.graph.with(|c| c.count("GET", "delta")));
        let before = snapshot(&w.folder)?;
        ctx.runtime.block_on(w.service.refresh()).map_err(|e| e.to_string())?;
        w.wait("the echo cycle", || (w.graph.with(|c| c.count("GET", "delta")) > deltas).then_some(()))?;
        std::thread::sleep(Duration::from_secs(3));
        if w.writes() != writes || !w.outbox_empty() {
            return Err(format!("the echo sent {} request(s) that change OneDrive", w.writes() - writes));
        }
        if snapshot(&w.folder)? != before {
            return Err("the echo changed the folder".into());
        }
        Ok(format!("create, edit, rename, move, mkdir and delete sent; {} entries match; the echo changed nothing", local.len()))
    }, checks, "writes: round trip")
}

/// Every entry below `folder` as OneDrive paths name it, with a file's size.
fn walk(folder: &Path) -> Result<Vec<(String, Option<usize>)>, String> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        for entry in std::fs::read_dir(folder.join(&rel)).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".konedrive-") {
                continue;
            }
            let path = rel.join(&name);
            let meta = entry.metadata().map_err(|e| e.to_string())?;
            if meta.is_dir() {
                stack.push(path.clone());
                out.push((path.display().to_string(), None));
            } else {
                out.push((path.display().to_string(), Some(meta.len() as usize)));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Every entry's inode and times: what an echo must not change.
fn snapshot(folder: &Path) -> Result<Vec<(String, u64, i64, i64)>, String> {
    let mut out = Vec::new();
    for (rel, _) in walk(folder)? {
        let meta = std::fs::symlink_metadata(folder.join(&rel)).map_err(|e| e.to_string())?;
        out.push((rel, meta.ino(), meta.mtime(), meta.ctime()));
    }
    Ok(out)
}
