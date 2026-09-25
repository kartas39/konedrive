//! Moves out of the folder (`docs/design/writes.md` §8, §10, §12), on the host: a folder placed by the
//! real materializer, rows made by the real examination, a fake OneDrive (wiremock), and a fake
//! helper that opens a handle by a table of where each object went — as the real one answers:
//! `ESTALE` for what is gone, `EPERM` for an object without the item id. What needs the real
//! helper (the marks themselves) is in the VM suite (`tests/vm/move_out.rs`).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{self, State, XATTR_ITEM_ID, XATTR_ROOT};
use tokio_util::sync::CancellationToken;

use super::fake::{qx, Harness};
use super::move_out::{trash_of, Filler, Helper, MoveOuts, SourceFill, Tidy, CONTENT_LOCAL};
use super::*;
use crate::sync::disk::Disk;
use crate::sync::helper::{Clearance, HelperError};
use crate::sync::local::liveness::answered;
use crate::sync::local::{Batch, Examined, Examiner, FakeLiveness, IgnoreList, Whereabouts};
use crate::sync::materialize::{Materializer, Scope};
use crate::sync::source::LocalDir;
use crate::tree::outbox::{OutboxKind, OutboxRow, OutboxState};
use crate::tree::{Change, Kind, Placement, Row, Table, TreeStore};

const TIME: i64 = 1_700_000_000;

fn row(id: &str, parent: Option<&str>, name: &str, kind: Kind, content: &[u8]) -> Row {
    Row {
        id: id.into(),
        parent_id: parent.map(str::to_owned),
        name: name.into(),
        kind,
        size: if kind == Kind::File { content.len() as u64 } else { 0 },
        mtime: TIME,
        etag: Some(format!("e-{id}")),
        ctag: Some(format!("c-{id}")),
        quickxor: (kind == Kind::File).then(|| qx(content)),
        mime: None,
        placement: Placement::Placed,
    }
}

/// A helper that answers `OpenByHandle` from a table of where each object went.
#[derive(Default)]
struct FakeHelper {
    at: Mutex<HashMap<FileHandle, PathBuf>>,
    /// Every answer is this refusal, while set.
    refuse: Mutex<Option<i32>>,
    /// `NotRunning`, while set.
    down: Mutex<bool>,
    /// What was asked: (call, where the object was).
    calls: Mutex<Vec<(&'static str, PathBuf)>>,
    socket: PathBuf,
}

fn where_is(file: &File) -> PathBuf {
    std::fs::read_link(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(file))).unwrap_or_default()
}

impl FakeHelper {
    /// The object at `path` is there now, and so is everything below it: the helper finds each by
    /// its handle, as the real one does.
    fn follow(&self, path: &Path) {
        let mut at = self.at.lock().unwrap();
        let mut stack = vec![path.to_path_buf()];
        while let Some(p) = stack.pop() {
            at.insert(World::handle(&p), p.clone());
            if std::fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
                stack.extend(std::fs::read_dir(&p).unwrap().map(|e| e.unwrap().path()));
            }
        }
    }

    fn log(&self, call: &'static str, file: &File) {
        self.calls.lock().unwrap().push((call, where_is(file)));
    }

    fn called(&self, call: &str) -> Vec<PathBuf> {
        self.calls.lock().unwrap().iter().filter(|(c, _)| *c == call).map(|(_, p)| p.clone()).collect()
    }

    fn up(&self) -> Result<(), HelperError> {
        if *self.down.lock().unwrap() {
            return Err(HelperError::NotRunning);
        }
        Ok(())
    }
}

#[async_trait]
impl Helper for FakeHelper {
    async fn open_by_handle(&self, _dir: &File, handle: &FileHandle) -> Result<OwnedFd, HelperError> {
        self.up()?;
        if let Some(errno) = *self.refuse.lock().unwrap() {
            return Err(HelperError::Refused(errno));
        }
        let stale = || HelperError::Refused(libc::ESTALE);
        let path = self.at.lock().unwrap().get(handle).cloned().ok_or_else(stale)?;
        let meta = std::fs::symlink_metadata(&path).map_err(|_| stale())?;
        let file = if meta.is_dir() {
            File::open(&path)
        } else {
            std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW).open(&path)
        }
        .map_err(|_| stale())?;
        if FileHandle::of(&file).ok().as_ref() != Some(handle) {
            return Err(stale());
        }
        if xattr::get(&path, XATTR_ITEM_ID).ok().flatten().is_none() {
            return Err(HelperError::Refused(libc::EPERM));
        }
        self.log("open", &file);
        Ok(file.into())
    }

    async fn mark_file(&self, file: &File) -> Result<(), HelperError> {
        self.up()?;
        self.log("mark_file", file);
        Ok(())
    }

    async fn mark_dir(&self, dir: &File) -> Result<(), HelperError> {
        self.up()?;
        self.log("mark_dir", dir);
        Ok(())
    }

    async fn unmark_dir(&self, dir: &File) -> Result<(), HelperError> {
        self.up()?;
        self.log("unmark_dir", dir);
        Ok(())
    }

    fn clearance(&self) -> Option<Clearance> {
        // No helper runs on the host: the way is clear.
        Some(Clearance::NoLink(self.socket.clone()))
    }
}

/// A folder placed from a listing, a fake OneDrive holding the same, and a worker with a fake
/// helper; the contents OneDrive holds are in `source/<item id>`.
struct World {
    dir: tempfile::TempDir,
    root: SyncRoot,
    store: Store,
    liveness: FakeLiveness,
    helper: Arc<FakeHelper>,
    source: PathBuf,
    h: Harness,
}

impl World {
    fn new(items: &[(&str, Option<&str>, &str, &[u8])]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let path = base.join("OneDrive");
        let source = base.join("source");
        std::fs::create_dir(&path).unwrap();
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(base.join("outside")).unwrap();
        let root_id = "5b0e2c7a-1d3f-4e8a-9b6c-0f1e2d3c4b5a".to_owned();
        xattr::set(&path, XATTR_ROOT, root_id.as_bytes()).unwrap();
        let root = SyncRoot { path, root_id };
        let store = Store::new(TreeStore::in_memory().unwrap());
        let mut all = vec![Change::Root(row("R", None, "", Kind::Folder, b""))];
        for (id, parent, name, content) in items {
            let kind = if name.ends_with('/') { Kind::Folder } else { Kind::File };
            let name = name.trim_end_matches('/');
            all.push(Change::Upsert(row(id, Some(parent.unwrap_or("R")), name, kind, content)));
            if kind == Kind::File {
                std::fs::write(source.join(id), content).unwrap();
            }
        }
        store
            .with(|s| {
                s.begin_staging(false)?;
                s.stage(&all)
            })
            .unwrap();
        {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let materializer = Materializer {
                disk: Disk::open(&root, false).unwrap(),
                store: store.clone(),
                link: None,
                runtime: runtime.handle().clone(),
                locks: InodeLocks::new(),
                root_item_id: "R".into(),
                rescue_into: base.join("rescued"),
                cancel: CancellationToken::new(),
                rw: None,
                claimed: None,
            };
            materializer.apply(Scope::Full).unwrap();
        }
        store.with(|s| s.commit_staging("link-1")).unwrap();
        let locks = InodeLocks::new();
        let h = Harness::new(&root, &store, &locks);
        let helper = Arc::new(FakeHelper { socket: base.join("no-helper.sock"), ..FakeHelper::default() });
        let w = World { dir, root, store, liveness: FakeLiveness::new(), helper, source, h };
        w.fills_from(LocalDir::new(w.source.clone()));
        w
    }

    /// The fills of moved-out placeholders come from `source`.
    fn fills_from(&self, source: LocalDir) {
        self.fills_with(Arc::new(SourceFill(Arc::new(source))));
    }

    fn fills_with(&self, filler: Arc<dyn Filler>) {
        let root = self.root.path.clone();
        *self.h.moved_out.lock().unwrap() = Some(MoveOuts {
            helper: self.helper.clone(),
            filler,
            route: None,
            home_trash: Some(self.trash()),
            roots: Arc::new(move || vec![root.clone()]),
        });
    }

    /// The user's own Trash, as `$XDG_DATA_HOME/Trash` would be.
    fn trash(&self) -> PathBuf {
        self.base().join("Trash")
    }

    /// `rel` sent to the Trash as a desktop sends it: its `.trashinfo` first. Where it went.
    fn to_trash(&self, rel: &str) -> PathBuf {
        let name = Path::new(rel).file_name().unwrap().to_str().unwrap().to_owned();
        std::fs::create_dir_all(self.trash().join("info")).unwrap();
        std::fs::write(self.trash().join(format!("info/{name}.trashinfo")), "[Trash Info]\n").unwrap();
        let to = self.trash().join("files").join(&name);
        self.move_out(rel, &to);
        to
    }

    fn base(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path.join(rel)
    }

    fn handle(path: &Path) -> FileHandle {
        FileHandle::at(&File::open(path.parent().unwrap()).unwrap(), path.file_name().unwrap()).unwrap()
    }

    /// `rel` moved out of the folder, to `to`; the helper and the liveness know where it went.
    fn move_out(&self, rel: &str, to: &Path) -> FileHandle {
        let handle = Self::handle(&self.path(rel));
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::rename(self.path(rel), to).unwrap();
        self.liveness.alive_tree(to);
        self.helper.follow(to);
        handle
    }

    fn examine(&self, pairs: &[(&str, &str)]) -> Examined {
        let mut batch = Batch::new();
        for (dir, name) in pairs {
            batch.name(Path::new(dir), OsStr::new(name));
        }
        let disk = Disk::open(&self.root, false).unwrap();
        let ignore = IgnoreList::default();
        let now = crate::sync::activity::unix_now();
        let locks = InodeLocks::new();
        Examiner { disk: &disk, store: &self.store, liveness: &self.liveness, ignore: &ignore, locks: &locks, now }.examine(&batch).unwrap()
    }

    fn rows(&self) -> Vec<OutboxRow> {
        self.store.with(|s| s.outbox_rows()).unwrap()
    }

    fn deletes(&self) -> usize {
        self.h.graph.with(|c| c.count("DELETE", "items/"))
    }

    fn in_bin(&self, id: &str) -> bool {
        self.h.graph.with(|c| c.bin.contains_key(id) && !c.items.contains_key(id))
    }

    fn konedrive_attrs(path: &Path) -> Vec<String> {
        xattr::list(path)
            .unwrap()
            .filter_map(|n| n.to_str().map(str::to_owned))
            .filter(|n| n.starts_with("user.konedrive."))
            .collect()
    }

    /// Rows in backoff are due now, as after a restart that waited long enough.
    fn due_now(&self) {
        self.store.with(|s| s.outbox_retry_now()).unwrap();
    }

    /// What `dropped` left outside the folder tidied, as a drop with no worker tidies it.
    fn tidy(&self, dropped: &[OutboxRow]) {
        let mo = self.h.moved_out.lock().unwrap().clone().unwrap();
        let locks = InodeLocks::new();
        self.h.runtime.block_on(Tidy { mo: &mo, root: &self.root, store: &self.store, locks: &locks }.dropped(dropped));
    }

    /// Another account's registered folder, `name` beside this one, with every folder there is.
    fn another_folder(&self, name: &str) -> PathBuf {
        let other = self.base().join(name);
        std::fs::create_dir_all(&other).unwrap();
        let roots = vec![self.root.path.clone(), other.clone()];
        self.h.moved_out.lock().unwrap().as_mut().unwrap().roots = Arc::new(move || roots.clone());
        other
    }
}

fn state(path: &Path) -> Option<State> {
    placeholder::read_state(&File::open(path).unwrap()).unwrap()
}

/// §4.6, WR5: a placeholder moved anywhere but the Trash is marked again, downloaded where it
/// went, stripped of konedrive's attributes, and only then deleted in OneDrive.
#[test]
fn a_placeholder_moved_out_is_downloaded_where_it_went_then_deleted() {
    let w = World::new(&[("P", None, "p.txt", b"the content")]);
    let to = w.base().join("outside/p.txt");
    w.move_out("p.txt", &to);
    w.examine(&[("", "p.txt")]);
    let rows = w.rows();
    assert_eq!(rows.iter().map(|r| r.kind).collect::<Vec<_>>(), vec![OutboxKind::MoveOut]);
    assert_eq!(state(&to), Some(State::OnlineOnly));

    w.h.run();
    assert_eq!(std::fs::read(&to).unwrap(), b"the content", "downloaded where it went");
    assert!(World::konedrive_attrs(&to).is_empty(), "an ordinary file now: {:?}", World::konedrive_attrs(&to));
    let marked = w.helper.called("mark_file");
    assert!(!marked.is_empty() && marked.iter().all(|p| p == &to), "marked again before the download: {marked:?}");
    assert!(w.in_bin("P"), "the item went to OneDrive's recycle bin");
    assert!(w.rows().is_empty());
    assert!(w.store.with(|s| s.get(Table::Items, "P")).unwrap().is_none());
}

/// §5: a download that stops part-way deletes nothing; the row stays, and the next run — a
/// restart — finishes the download and only then deletes.
#[test]
fn a_download_that_stops_part_way_deletes_nothing_until_it_is_whole() {
    let w = World::new(&[("P", None, "p.bin", &[7u8; 4096])]);
    // Every fetch breaks after 1000 bytes: the fill gives up (a fill resumes a stream that
    // breaks once by itself).
    w.fills_from(LocalDir::new(w.source.clone()).fail_at(1000));
    let to = w.base().join("outside/p.bin");
    w.move_out("p.bin", &to);
    w.examine(&[("", "p.bin")]);

    w.h.run();
    assert_eq!(w.deletes(), 0, "nothing is deleted while the content is not whole");
    assert_eq!(state(&to), Some(State::OnlineOnly), "the failed fill was rolled back");
    let rows = w.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].kind, rows[0].state), (OutboxKind::MoveOut, OutboxState::Retry));
    assert!(rows[0].reason.as_deref().is_some_and(|r| r.starts_with(reason::DOWNLOAD)), "{:?}", rows[0].reason);
    assert_eq!(rows[0].snapshot, None, "not marked local");

    w.fills_from(LocalDir::new(w.source.clone()));
    w.due_now();
    w.h.run();
    assert_eq!(std::fs::read(&to).unwrap(), vec![7u8; 4096]);
    assert!(w.in_bin("P"));
    assert!(w.rows().is_empty());
}

/// F90: `EPERM` is never "gone" — the row stays and nothing is deleted — unless the row's own
/// marker says it took the attributes off itself. `ESTALE` is gone: the user deleted it.
#[test]
fn eperm_keeps_the_row_and_estale_deletes() {
    let w = World::new(&[("P", None, "p.txt", b"p"), ("Q", None, "q.txt", b"q")]);
    w.move_out("p.txt", &w.base().join("outside/p.txt"));
    let q = w.base().join("outside/q.txt");
    w.move_out("q.txt", &q);
    w.examine(&[("", "p.txt"), ("", "q.txt")]);
    assert_eq!(w.rows().len(), 2);

    *w.helper.refuse.lock().unwrap() = Some(libc::EPERM);
    w.h.run();
    assert_eq!(w.deletes(), 0);
    assert!(w.rows().iter().all(|r| r.reason.as_deref() == Some(reason::UNREACHABLE)), "{:?}", w.rows());

    // A helper that does not answer decides nothing either.
    *w.helper.refuse.lock().unwrap() = None;
    *w.helper.down.lock().unwrap() = true;
    w.due_now();
    w.h.run();
    assert_eq!(w.deletes(), 0);
    assert!(w.rows().iter().all(|r| r.reason.as_deref() == Some(reason::NO_HELPER)), "{:?}", w.rows());

    // Q deleted by the user after it left: `ESTALE`, gone — believed when it says so twice. P's
    // marker set and its item id taken off (a crash after our own strip): its `EPERM` is expected.
    *w.helper.down.lock().unwrap() = false;
    std::fs::remove_file(&q).unwrap();
    let p = w.rows().into_iter().find(|r| r.item_id.as_deref() == Some("P")).unwrap();
    w.store.with(|s| s.outbox_set_snapshot(p.seq, Some(CONTENT_LOCAL))).unwrap();
    xattr::remove(w.base().join("outside/p.txt"), XATTR_ITEM_ID).unwrap();
    w.due_now();
    w.h.run();
    assert!(w.in_bin("P") && !w.in_bin("Q"), "one ESTALE is not enough");
    assert_eq!(w.rows()[0].reason.as_deref(), Some(reason::GONE_ONCE));
    w.due_now();
    w.h.run();
    assert!(w.in_bin("Q"));
    assert!(w.rows().is_empty());
}

/// Every failure to decode a handle is `ESTALE`, so a handle taken on
/// another filesystem than the folder's now (a new disk, a snapshot rolled back) says nothing. The
/// record of that filesystem is the root's own handle (not `f_fsid`, a device number on XFS). A
/// row meeting a changed record waits; the next examination takes every handle again — a Full
/// scan: what is missing is placed again rather than deleted, and a `move-out` takes the handle
/// of what stands where it went — and the row then runs as any other.
#[test]
fn a_changed_filesystem_takes_the_handles_again_and_deletes_nothing() {
    use crate::sync::local::liveness::{handle_namespace, HANDLES_ON};
    let w = World::new(&[("P", None, "p.txt", b"moved"), ("Q", None, "q.txt", b"q")]);
    let p = w.base().join("outside/p.txt");
    let p_handle = w.move_out("p.txt", &p);
    w.examine(&[("", "p.txt")]);
    let row = w.rows()[0].clone();
    assert_eq!(row.target_name.as_deref(), p.to_str(), "where it went is kept");
    let root = File::open(&w.root.path).unwrap();
    let recorded = w.store.with(|s| s.meta(HANDLES_ON)).unwrap().unwrap();
    assert_eq!(recorded, handle_namespace(&root).unwrap());
    assert!(recorded.starts_with("root:"), "keyed on the root's handle: {recorded}");

    // The filesystem changed: P's recorded handle is from the old one and no longer found.
    w.store.with(|s| s.set_meta(HANDLES_ON, Some("root:0102"))).unwrap();
    let stale = FileHandle { kind: p_handle.kind, bytes: vec![0; p_handle.bytes.len()] };
    w.store.with(|s| s.outbox_amend(row.seq, |r| r.inode.as_mut().unwrap().handle = Some(stale.clone()))).unwrap();
    for _ in 0..2 {
        w.h.run();
        w.due_now();
    }
    assert_eq!(w.deletes(), 0);
    assert_eq!(w.rows()[0].reason.as_deref(), Some(reason::STALE_HANDLE));

    // The examination takes the handles again: Q, missing meanwhile, is placed again, not deleted.
    std::fs::rename(w.path("q.txt"), w.base().join("gone-q.txt")).unwrap();
    let examined = w.examine(&[("", "q.txt")]);
    assert!(examined.renewed);
    assert_eq!(examined.unproven, vec!["Q".to_owned()]);
    assert_eq!(w.store.with(|s| s.meta(HANDLES_ON)).unwrap(), Some(recorded));
    assert_eq!(w.rows().len(), 1);
    assert_eq!(w.rows()[0].inode.as_ref().unwrap().handle.as_ref(), Some(&p_handle), "found again where it went");

    w.due_now();
    w.h.run();
    assert_eq!(std::fs::read(&p).unwrap(), b"moved");
    assert!(w.in_bin("P") && !w.in_bin("Q"));
}

/// `ESTALE` twice is not "gone" while the object may still stand where it was last
/// proved to be — an inode that cannot be read answers `ESTALE` every time. Only nothing (or
/// another object) there makes it a delete.
#[test]
fn estale_is_gone_only_with_nothing_where_the_object_was() {
    let w = World::new(&[("P", None, "p.txt", b"p")]);
    let p = w.base().join("outside/p.txt");
    w.move_out("p.txt", &p);
    w.examine(&[("", "p.txt")]);
    *w.helper.refuse.lock().unwrap() = Some(libc::ESTALE);
    for _ in 0..2 {
        w.h.run();
        w.due_now();
    }
    assert_eq!(w.deletes(), 0);
    assert_eq!(w.rows()[0].reason.as_deref(), Some(reason::GONE_UNPROVED), "it still stands there");

    std::fs::remove_file(&p).unwrap();
    for _ in 0..2 {
        w.h.run();
        w.due_now();
    }
    assert!(w.in_bin("P"));
    assert!(w.rows().is_empty());
}

/// In a folder sent to the Trash, the placeholders go before anything is stripped,
/// so a removal that fails leaves nothing stripped behind a marker that may be taken off; and a
/// marker is never taken off an object back in the folder, whatever it stripped already.
#[test]
fn a_trashed_folder_strips_nothing_until_its_placeholders_are_gone() {
    use std::os::unix::fs::PermissionsExt;
    let w = World::new(&[("K", None, "kept/", b""), ("K1", Some("K"), "a-down.txt", b"downloaded"), ("K2", Some("K"), "b-cloud.txt", b"cloud")]);
    {
        let file = File::options().write(true).open(w.path("kept/a-down.txt")).unwrap();
        std::io::Write::write_all(&mut &file, b"downloaded").unwrap();
        placeholder::write_state(&file, State::Hydrated).unwrap();
    }
    let kept = w.to_trash("kept");
    w.examine(&[("", "kept")]);
    w.fills_from(LocalDir::new(w.base().join("no-source")));
    std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o555)).unwrap();
    w.h.run();
    std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(w.deletes(), 0);
    assert!(kept.join("b-cloud.txt").exists());
    assert!(!World::konedrive_attrs(&kept.join("a-down.txt")).is_empty(), "nothing stripped before the placeholders went");

    w.due_now();
    w.h.run();
    assert!(!kept.join("b-cloud.txt").exists());
    assert!(World::konedrive_attrs(&kept.join("a-down.txt")).is_empty());
    assert!(w.in_bin("K"));

    // A marker stays on a row whose object is back in the folder.
    let w = World::new(&[("P", None, "p.txt", b"p")]);
    let handle = w.move_out("p.txt", &w.base().join("outside/p.txt"));
    w.examine(&[("", "p.txt")]);
    let seq = w.rows()[0].seq;
    w.store.with(|s| s.outbox_set_snapshot(seq, Some(CONTENT_LOCAL))).unwrap();
    std::fs::rename(w.base().join("outside/p.txt"), w.path("back.txt")).unwrap();
    w.helper.at.lock().unwrap().insert(handle, w.path("back.txt"));
    w.h.run();
    assert_eq!(w.rows()[0].snapshot.as_deref(), Some(CONTENT_LOCAL));
    assert_eq!(w.deletes(), 0);
}

/// A crash between two strips of a folder converges: what was stripped already answers
/// `EPERM`, which the row's marker explains, and the folder goes once the rest is stripped.
#[test]
fn a_crash_between_two_strips_of_a_folder_converges() {
    let w = World::new(&[("D", None, "d/", b""), ("P1", Some("D"), "one.txt", b"first"), ("P2", Some("D"), "two.txt", b"second")]);
    let to = w.base().join("outside/d");
    w.move_out("d", &to);
    w.examine(&[("", "d")]);
    let engine = w.h.engine();
    engine.arm(Fault::MidStrip);
    w.h.drain(&engine);
    assert_eq!(w.deletes(), 0);
    assert_eq!(w.rows()[0].snapshot.as_deref(), Some(CONTENT_LOCAL));
    let stripped = ["one.txt", "two.txt"].iter().filter(|n| World::konedrive_attrs(&to.join(n)).is_empty()).count();
    assert_eq!(stripped, 1, "one file stripped, one not");

    w.h.run();
    assert!(["one.txt", "two.txt"].iter().all(|n| World::konedrive_attrs(&to.join(n)).is_empty()));
    assert_eq!(std::fs::read(to.join("two.txt")).unwrap(), b"second");
    assert!(w.in_bin("D") && w.in_bin("P1") && w.in_bin("P2"));
    assert!(w.rows().is_empty());
}

/// A folder one of whose files cannot be downloaded goes nowhere: nothing is stripped or
/// deleted, and the row waits.
#[test]
fn a_folder_with_one_file_that_cannot_be_downloaded_stays() {
    let w = World::new(&[("D", None, "d/", b""), ("P1", Some("D"), "one.txt", b"first"), ("P2", Some("D"), "two.txt", b"second")]);
    std::fs::remove_file(w.source.join("P2")).unwrap();
    let to = w.base().join("outside/d");
    w.move_out("d", &to);
    w.examine(&[("", "d")]);
    w.h.run();
    assert_eq!(w.deletes(), 0);
    assert_eq!(state(&to.join("one.txt")), Some(State::Hydrated), "what could be downloaded was");
    assert_eq!(state(&to.join("two.txt")), Some(State::OnlineOnly));
    assert!(!World::konedrive_attrs(&to.join("one.txt")).is_empty() && !World::konedrive_attrs(&to).is_empty(), "nothing stripped");
    assert!(w.helper.called("unmark_dir").is_empty());
    let row = &w.rows()[0];
    assert!(row.reason.as_deref().is_some_and(|r| r.starts_with(reason::DOWNLOAD)), "{:?}", row.reason);
    assert_eq!(row.snapshot, None);
}

/// A folder moved back into the folder while its placeholders download is not
/// stripped, unmarked or deleted: the examination takes it on.
#[test]
fn a_folder_moved_back_during_its_download_is_left_alone() {
    struct MovesBack {
        from: PathBuf,
        to: PathBuf,
        helper: Arc<FakeHelper>,
        inner: SourceFill,
    }
    #[async_trait]
    impl Filler for MovesBack {
        async fn fill(&self, file: File, shown: &Path, clearance: Option<&Clearance>) -> Result<(), crate::sync::source::FillError> {
            if self.from.exists() {
                std::fs::rename(&self.from, &self.to).unwrap();
                self.helper.follow(&self.to);
            }
            self.inner.fill(file, shown, clearance).await
        }
    }
    let w = World::new(&[("D", None, "d/", b""), ("P1", Some("D"), "one.txt", b"first"), ("P2", Some("D"), "two.txt", b"second")]);
    let out = w.base().join("outside/d");
    w.move_out("d", &out);
    w.examine(&[("", "d")]);
    w.fills_with(Arc::new(MovesBack {
        from: out.clone(),
        to: w.path("d"),
        helper: Arc::clone(&w.helper),
        inner: SourceFill(Arc::new(LocalDir::new(w.source.clone()))),
    }));
    w.h.run();
    assert_eq!(w.deletes(), 0);
    assert!(w.helper.called("unmark_dir").is_empty(), "no directory in the folder is unmarked");
    for rel in ["d", "d/one.txt", "d/two.txt"] {
        assert!(World::konedrive_attrs(&w.path(rel)).iter().any(|a| a == XATTR_ITEM_ID), "{rel} keeps its item id");
    }
    let row = &w.rows()[0];
    assert_eq!((row.reason.as_deref(), row.snapshot.as_deref()), (Some(reason::BACK_INSIDE), None));
}

/// A directory that only looks like a Trash (a `.Trash-<uid>` that is not at a mount's
/// top, with or without a `.trashinfo`) is anywhere else: downloaded first. So is a placeholder
/// in the real Trash that has another link: only its Trash name would go.
#[test]
fn a_lookalike_trash_and_a_linked_placeholder_are_downloaded_first() {
    let w = World::new(&[("L", None, "l.txt", b"looks like a trash"), ("H", None, "h.txt", b"linked")]);
    let fake = w.base().join(format!("outside/.Trash-{}", nix::unistd::geteuid().as_raw()));
    std::fs::create_dir_all(fake.join("info")).unwrap();
    std::fs::write(fake.join("info/l.txt.trashinfo"), "[Trash Info]\n").unwrap();
    let l = fake.join("files/l.txt");
    w.move_out("l.txt", &l);
    let h = w.to_trash("h.txt");
    std::fs::hard_link(&h, w.base().join("outside/h-link.txt")).unwrap();
    w.examine(&[("", "l.txt"), ("", "h.txt")]);
    assert_eq!(w.rows().len(), 2);

    w.h.run();
    assert_eq!(std::fs::read(&l).unwrap(), b"looks like a trash");
    assert!(World::konedrive_attrs(&l).is_empty());
    assert_eq!(std::fs::read(&h).unwrap(), b"linked", "downloaded, not removed");
    assert!(w.in_bin("L") && w.in_bin("H"));
}

/// While a row is in flight (a long download here), a wake still re-marks what left:
/// the helper back is not held up behind the download.
#[test]
fn what_left_is_marked_again_while_other_rows_run() {
    struct Slow {
        busy: Arc<std::sync::atomic::AtomicBool>,
        inner: SourceFill,
    }
    #[async_trait]
    impl Filler for Slow {
        async fn fill(&self, file: File, shown: &Path, clearance: Option<&Clearance>) -> Result<(), crate::sync::source::FillError> {
            self.busy.store(true, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let filled = self.inner.fill(file, shown, clearance).await;
            self.busy.store(false, std::sync::atomic::Ordering::SeqCst);
            filled
        }
    }
    let w = World::new(&[("P", None, "p.txt", b"slow"), ("Q", None, "q.txt", b"waits")]);
    let q = w.base().join("outside/q.txt");
    w.move_out("p.txt", &w.base().join("outside/p.txt"));
    w.move_out("q.txt", &q);
    w.examine(&[("", "p.txt"), ("", "q.txt")]);
    let busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
    w.fills_with(Arc::new(Slow { busy: Arc::clone(&busy), inner: SourceFill(Arc::new(LocalDir::new(w.source.clone()))) }));
    let engine = w.h.engine();
    let running = {
        let engine = Arc::clone(&engine);
        w.h.runtime.spawn(async move { engine.drain(&CancellationToken::new()).await })
    };
    let q_marks = || w.helper.called("mark_file").iter().filter(|p| **p == q).count();
    let started = std::time::Instant::now();
    while !busy.load(std::sync::atomic::Ordering::SeqCst) && started.elapsed() < std::time::Duration::from_secs(5) {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(busy.load(std::sync::atomic::Ordering::SeqCst), "a download is in flight");
    assert_eq!(q_marks(), 1);
    engine.helper_back();
    let started = std::time::Instant::now();
    while q_marks() < 2 && started.elapsed() < std::time::Duration::from_secs(2) {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(q_marks(), 2, "marked again at once");
    assert!(busy.load(std::sync::atomic::Ordering::SeqCst), "while the other row was still in flight");
    w.h.runtime.block_on(running).unwrap();
}

/// §5: a crash after the attributes came off and before the delete converges — the replay meets
/// `EPERM` (no item id any more), the marker says why, and the item is deleted.
#[test]
fn a_crash_between_the_strip_and_the_delete_converges() {
    let w = World::new(&[("P", None, "p.txt", b"content")]);
    let to = w.base().join("outside/p.txt");
    w.move_out("p.txt", &to);
    w.examine(&[("", "p.txt")]);
    let engine = w.h.engine();
    engine.arm(Fault::AfterStrip);
    w.h.drain(&engine);
    assert_eq!(w.deletes(), 0);
    assert!(World::konedrive_attrs(&to).is_empty());
    assert_eq!(w.rows()[0].snapshot.as_deref(), Some(CONTENT_LOCAL));
    assert_eq!(w.rows()[0].state, OutboxState::Running);

    w.h.run();
    assert_eq!(std::fs::read(&to).unwrap(), b"content");
    assert!(w.in_bin("P"));
    assert!(w.rows().is_empty());
}

/// §4.6: a folder moved out has every placeholder of its item downloaded where it went, the
/// attributes taken off every file and directory, every directory unmarked, and only then the
/// folder deleted in OneDrive.
#[test]
fn a_folder_moved_out_is_downloaded_whole_then_deleted() {
    let w = World::new(&[
        ("D", None, "d/", b""),
        ("P1", Some("D"), "one.txt", b"first"),
        ("S", Some("D"), "sub/", b""),
        ("P2", Some("S"), "two.txt", b"second"),
    ]);
    let to = w.base().join("outside/d");
    w.move_out("d", &to);
    w.examine(&[("", "d")]);
    assert_eq!(w.rows().iter().map(|r| (r.kind, r.item_id.clone())).collect::<Vec<_>>(), vec![(OutboxKind::MoveOut, Some("D".into()))]);

    w.h.run();
    assert_eq!(std::fs::read(to.join("one.txt")).unwrap(), b"first");
    assert_eq!(std::fs::read(to.join("sub/two.txt")).unwrap(), b"second");
    for path in [to.clone(), to.join("one.txt"), to.join("sub"), to.join("sub/two.txt")] {
        assert!(World::konedrive_attrs(&path).is_empty(), "{}", path.display());
    }
    let mut unmarked = w.helper.called("unmark_dir");
    unmarked.sort();
    assert_eq!(unmarked, vec![to.clone(), to.join("sub")]);
    assert!(w.in_bin("D") && w.in_bin("P1") && w.in_bin("P2"));
    assert!(w.rows().is_empty());
}

/// §4.6, the Trash: nothing is downloaded. A placeholder is removed with its `.trashinfo`; a
/// downloaded file stays as the user's own; both items go to OneDrive's recycle bin.
#[test]
fn the_trash_takes_placeholders_without_a_download() {
    let w = World::new(&[("T", None, "t.txt", b"placeholder"), ("H", None, "h.txt", b"downloaded")]);
    {
        // H was downloaded before it was trashed.
        let file = File::options().write(true).open(w.path("h.txt")).unwrap();
        std::io::Write::write_all(&mut &file, b"downloaded").unwrap();
        placeholder::write_state(&file, State::Hydrated).unwrap();
    }
    let trash = w.trash();
    let t = w.to_trash("t.txt");
    let h = w.to_trash("h.txt");
    w.examine(&[("", "t.txt"), ("", "h.txt")]);
    assert_eq!(w.rows().len(), 2);
    w.fills_from(LocalDir::new(w.base().join("no-source")));

    w.h.run();
    assert!(!t.exists() && !trash.join("info/t.txt.trashinfo").exists(), "the placeholder left the Trash with its info");
    assert_eq!(std::fs::read(&h).unwrap(), b"downloaded");
    assert!(World::konedrive_attrs(&h).is_empty());
    assert!(trash.join("info/h.txt.trashinfo").exists());
    assert!(w.in_bin("T") && w.in_bin("H"));
    assert!(w.rows().is_empty());
}

/// §4.6, the Trash, for folders: nothing is downloaded into it. A folder of placeholders leaves
/// the Trash whole, with its `.trashinfo`; a folder holding a downloaded file keeps that file, as
/// the user's own, and loses its placeholders.
#[test]
fn a_folder_in_the_trash_keeps_only_what_was_downloaded() {
    let w = World::new(&[
        ("E", None, "empty/", b""),
        ("E1", Some("E"), "one.txt", b"cloud only"),
        ("K", None, "kept/", b""),
        ("K1", Some("K"), "down.txt", b"downloaded"),
        ("K2", Some("K"), "cloud.txt", b"cloud only"),
    ]);
    {
        let file = File::options().write(true).open(w.path("kept/down.txt")).unwrap();
        std::io::Write::write_all(&mut &file, b"downloaded").unwrap();
        placeholder::write_state(&file, State::Hydrated).unwrap();
    }
    let trash = w.trash();
    for name in ["empty", "kept"] {
        w.to_trash(name);
    }
    w.examine(&[("", "empty"), ("", "kept")]);
    assert_eq!(w.rows().len(), 2);
    w.fills_from(LocalDir::new(w.base().join("no-source")));

    w.h.run();
    assert!(!trash.join("files/empty").exists() && !trash.join("info/empty.trashinfo").exists());
    let kept = trash.join("files/kept");
    assert_eq!(std::fs::read(kept.join("down.txt")).unwrap(), b"downloaded");
    assert!(!kept.join("cloud.txt").exists(), "the placeholder left the Trash");
    assert!(World::konedrive_attrs(&kept).is_empty() && World::konedrive_attrs(&kept.join("down.txt")).is_empty());
    assert!(trash.join("info/kept.trashinfo").exists());
    assert!(["E", "E1", "K", "K1", "K2"].iter().all(|id| w.in_bin(id)));
    assert!(w.rows().is_empty());
}

/// the examination: restoring a held move out (`RestoreDeletes`) places the item in the folder again,
/// and tidies what had left: placeholders outside go, a downloaded file stays stripped, the
/// directory is stripped and unmarked. Nothing is deleted in OneDrive.
#[test]
fn restoring_a_held_move_out_tidies_what_left() {
    let w = World::new(&[("D", None, "d/", b""), ("P1", Some("D"), "cloud.txt", b"cloud"), ("P2", Some("D"), "down.txt", b"down"), ("Q", None, "q.txt", b"q")]);
    {
        let file = File::options().write(true).open(w.path("d/down.txt")).unwrap();
        std::io::Write::write_all(&mut &file, b"down").unwrap();
        placeholder::write_state(&file, State::Hydrated).unwrap();
    }
    let (d, q) = (w.base().join("outside/d"), w.base().join("outside/q.txt"));
    w.move_out("d", &d);
    w.move_out("q.txt", &q);
    w.examine(&[("", "d"), ("", "q.txt")]);
    for row in w.rows() {
        w.store.with(|s| s.outbox_set_state(row.seq, OutboxState::Held, Some("mass-delete"), None)).unwrap();
    }
    let dropped = w.store.with(|s| s.outbox_drop_held()).unwrap();
    assert_eq!(dropped.len(), 2);

    w.tidy(&dropped);
    assert!(!q.exists() && !d.join("cloud.txt").exists(), "the placeholders outside went");
    assert_eq!(std::fs::read(d.join("down.txt")).unwrap(), b"down");
    assert!(World::konedrive_attrs(&d).is_empty() && World::konedrive_attrs(&d.join("down.txt")).is_empty());
    assert_eq!(w.helper.called("unmark_dir"), vec![d.clone()]);
    assert_eq!(w.deletes(), 0);
}

/// `move-out` rows dropped with no worker to finish them (a switch to read-only,
/// a Forget, a Remove) leave nothing outside that would read as zeros: the placeholder goes, a
/// downloaded file stays stripped, and the item forgets its local object, so that it is placed
/// again rather than taken for a delete. Nothing is deleted in OneDrive.
#[test]
fn dropped_move_outs_leave_no_placeholder_outside() {
    let w = World::new(&[("P", None, "p.txt", b"cloud"), ("Q", None, "q.txt", b"down")]);
    {
        let file = File::options().write(true).open(w.path("q.txt")).unwrap();
        std::io::Write::write_all(&mut &file, b"down").unwrap();
        placeholder::write_state(&file, State::Hydrated).unwrap();
    }
    let (p, q) = (w.base().join("outside/p.txt"), w.base().join("outside/q.txt"));
    w.move_out("p.txt", &p);
    w.move_out("q.txt", &q);
    w.examine(&[("", "p.txt"), ("", "q.txt")]);
    assert_eq!(w.rows().len(), 2);
    assert!(w.store.with(|s| s.local_handle("P")).unwrap().is_some());

    let dropped = w.store.with(move_out::drop_rows).unwrap();
    w.tidy(&dropped);
    assert!(!p.exists(), "the placeholder outside went");
    assert_eq!(std::fs::read(&q).unwrap(), b"down");
    assert!(World::konedrive_attrs(&q).is_empty());
    assert!(w.rows().is_empty());
    assert!(w.store.with(|s| s.local_handle("P")).unwrap().is_none(), "placed again, not taken for a delete");
    assert_eq!(w.deletes(), 0);
}

/// `docs/design/writes.md` §8.3: a placeholder moved into another account's folder, which
/// turns read-only before this account's move out has run. That folder's read-only reconcile, whose
/// tree does not know the id, sets the object aside alive rather than removing it; the move out
/// then finds it by its handle and downloads it where it is. The file ends up on disk with its
/// content, and not only in OneDrive's recycle bin.
#[test]
fn a_placeholder_moved_into_a_read_only_account_ends_up_on_disk() {
    let w = World::new(&[("P", None, "p.txt", b"the content")]);
    // Account B's folder, placed from its own listing while it was read-write.
    let b = w.another_folder("B");
    xattr::set(&b, XATTR_ROOT, b"b-root").unwrap();
    let b_root = SyncRoot { path: b.clone(), root_id: "b-root".into() };
    let b_store = Store::new(TreeStore::in_memory().unwrap());
    let b_items = vec![Change::Root(row("RB", None, "", Kind::Folder, b"")), Change::Upsert(row("B1", Some("RB"), "b.txt", Kind::File, b"b"))];
    let a_store = w.store.clone();
    let claimed: crate::sync::materialize::Claimed = Arc::new(move |id| a_store.with(|s| Ok(s.get(Table::Items, id)?.is_some())).unwrap());
    let reconcile_b = |locked: bool| {
        Materializer {
            disk: Disk::open(&b_root, locked).unwrap(),
            store: b_store.clone(),
            link: None,
            runtime: w.h.runtime.handle().clone(),
            locks: InodeLocks::new(),
            root_item_id: "RB".into(),
            rescue_into: w.base().join("rescued-b/now"),
            cancel: CancellationToken::new(),
            rw: None,
            claimed: Some(Arc::clone(&claimed)),
        }
        .apply(Scope::Full)
        .unwrap()
    };
    b_store
        .with(|s| {
            s.begin_staging(false)?;
            s.stage(&b_items)
        })
        .unwrap();
    reconcile_b(false);
    b_store.with(|s| s.commit_staging("b-1")).unwrap();

    // The user moves A's placeholder into B; A's examination sees it leave, and its move out waits.
    let in_b = b.join("p.txt");
    w.move_out("p.txt", &in_b);
    w.examine(&[("", "p.txt")]);
    assert_eq!(w.rows()[0].kind, OutboxKind::MoveOut);

    // B is read-only now: its Full reconcile does not know P, and A claims it.
    b_store.with(|s| s.begin_staging(true)).unwrap();
    let applied = reconcile_b(true);
    assert!(!in_b.exists(), "out of B's folder");
    let aside = applied.rescued.iter().find(|r| r.original == Path::new("p.txt")).map(|r| r.rescued.clone()).expect("set aside");
    assert_eq!(state(&aside), Some(State::OnlineOnly), "alive, attributes and all");
    assert_eq!(w.deletes(), 0);

    // A's move out runs: the helper finds the object by its handle, wherever it is.
    w.helper.follow(&aside);
    w.h.run();
    assert_eq!(std::fs::read(&aside).unwrap(), b"the content", "on disk, with its content");
    assert!(World::konedrive_attrs(&aside).is_empty());
    assert!(w.in_bin("P"));
    assert!(w.rows().is_empty());
}

/// A moved-out object gone from inside another account's folder, where it was
/// last proved to be, may have been removed there as none of that account's: nothing is deleted
/// in OneDrive. The row goes, and the item forgets its local object, so that it is placed again
/// here.
#[test]
fn a_move_out_gone_inside_another_accounts_folder_deletes_nothing() {
    let w = World::new(&[("P", None, "p.txt", b"p")]);
    let in_b = w.another_folder("B").join("p.txt");
    w.move_out("p.txt", &in_b);
    w.examine(&[("", "p.txt")]);
    assert_eq!(w.rows()[0].target_name.as_deref(), in_b.to_str());
    std::fs::remove_file(&in_b).unwrap();
    for _ in 0..2 {
        w.h.run();
        w.due_now();
    }
    assert_eq!(w.deletes(), 0, "nothing is deleted in OneDrive");
    assert!(w.rows().is_empty());
    assert!(w.store.with(|s| s.local_handle("P")).unwrap().is_none(), "placed again by the next reconcile");
    assert!(w.store.with(|s| s.get(Table::Items, "P")).unwrap().is_some());
}

/// A downloaded file moved into another account's read-write folder, whose examination
/// strips it and uploads it as its own before this account's move out ran. The move out meets
/// `EPERM` — never "gone" — but the object stands where it was last proved to be, with the same
/// handle: the row goes without a delete, and the item comes back here. Duplicated, nothing lost,
/// and no row waits for ever.
#[test]
fn a_file_another_account_took_for_its_own_is_kept_here_too() {
    let w = World::new(&[("P", None, "p.txt", b"down")]);
    {
        let file = File::options().write(true).open(w.path("p.txt")).unwrap();
        std::io::Write::write_all(&mut &file, b"down").unwrap();
        placeholder::write_state(&file, State::Hydrated).unwrap();
    }
    let in_b = w.another_folder("B").join("p.txt");
    w.move_out("p.txt", &in_b);
    w.examine(&[("", "p.txt")]);
    placeholder::strip_konedrive_xattrs(&File::open(&in_b).unwrap()).unwrap();
    w.h.run();
    assert_eq!(w.deletes(), 0, "nothing is deleted in OneDrive");
    assert!(w.rows().is_empty(), "no row waits for ever: {:?}", w.rows());
    assert!(w.store.with(|s| s.local_handle("P")).unwrap().is_none());
    assert_eq!(std::fs::read(&in_b).unwrap(), b"down");

    // Stripped anywhere else, the object is not proved to be anyone's: the row waits.
    let w = World::new(&[("Q", None, "q.txt", b"q")]);
    let out = w.base().join("outside/q.txt");
    w.move_out("q.txt", &out);
    w.examine(&[("", "q.txt")]);
    xattr::remove(&out, XATTR_ITEM_ID).unwrap();
    w.h.run();
    assert_eq!(w.rows()[0].reason.as_deref(), Some(reason::UNREACHABLE));
}

/// A placeholder that left a moved-out folder again before the folder's row ran is made local
/// where it went too, before the folder — and it with it — leaves OneDrive.
#[test]
fn what_left_a_moved_out_folder_since_is_downloaded_where_it_went() {
    let w = World::new(&[("D", None, "d/", b""), ("P", Some("D"), "p.txt", b"inside"), ("Q", Some("D"), "q.txt", b"went on")]);
    let q = w.path("d/q.txt");
    let q_handle = World::handle(&q);
    let to = w.base().join("outside/d");
    w.move_out("d", &to);
    w.examine(&[("", "d")]);
    let q_now = w.base().join("outside/q.txt");
    std::fs::rename(to.join("q.txt"), &q_now).unwrap();
    w.helper.at.lock().unwrap().insert(q_handle, q_now.clone());

    w.h.run();
    assert_eq!(std::fs::read(to.join("p.txt")).unwrap(), b"inside");
    assert_eq!(std::fs::read(&q_now).unwrap(), b"went on");
    assert!(World::konedrive_attrs(&q_now).is_empty());
    assert!(w.in_bin("D") && w.in_bin("Q"));
}

/// §4.6: an object back inside the folder before its row ran is the examination's: nothing is
/// downloaded or deleted.
#[test]
fn an_object_back_in_the_folder_is_left_to_the_examination() {
    let w = World::new(&[("P", None, "p.txt", b"p")]);
    let handle = w.move_out("p.txt", &w.base().join("outside/p.txt"));
    w.examine(&[("", "p.txt")]);
    std::fs::rename(w.base().join("outside/p.txt"), w.path("back.txt")).unwrap();
    w.helper.at.lock().unwrap().insert(handle, w.path("back.txt"));
    w.h.run();
    assert_eq!(w.deletes(), 0);
    assert_eq!(state(&w.path("back.txt")), Some(State::OnlineOnly));
    assert_eq!(w.rows()[0].reason.as_deref(), Some(reason::BACK_INSIDE));
}

/// §4.6, §5: what left is marked again before anything else, whatever the rows' states — here a
/// paused worker, which sends nothing — once per helper connection, and again after the helper
/// comes back; and the router is told whose the ids are.
#[test]
fn what_left_is_marked_again_first_even_while_paused() {
    let w = World::new(&[("D", None, "d/", b""), ("P", Some("D"), "p.txt", b"p"), ("Q", None, "q.txt", b"q")]);
    let d = w.base().join("outside/d");
    let q = w.base().join("outside/q.txt");
    w.move_out("d", &d);
    w.move_out("q.txt", &q);
    w.examine(&[("", "d"), ("", "q.txt")]);
    let routed: Arc<Mutex<Vec<std::collections::HashSet<String>>>> = Arc::default();
    {
        let mut moved_out = w.h.moved_out.lock().unwrap();
        let routed = Arc::clone(&routed);
        moved_out.as_mut().unwrap().route = Some(Arc::new(move |ids| routed.lock().unwrap().push(ids)));
    }
    let engine = w.h.engine();
    engine.pause(None).unwrap();
    w.h.drain(&engine);
    assert_eq!(w.deletes(), 0, "paused");
    assert_eq!(w.helper.called("mark_file"), vec![q.clone()]);
    assert_eq!(w.helper.called("mark_dir"), vec![d.clone()]);
    let ids = routed.lock().unwrap().last().cloned().unwrap();
    assert_eq!(ids, ["D", "P", "Q"].into_iter().map(String::from).collect());

    w.h.drain(&engine);
    assert_eq!(w.helper.called("mark_file").len(), 1, "once per helper connection");
    engine.helper_back();
    w.h.drain(&engine);
    assert_eq!(w.helper.called("mark_file").len(), 2, "again once the helper is back");
    assert_eq!(routed.lock().unwrap().len(), 1, "the router is told only of a change");
}

/// The liveness the daemon's examination asks: `ESTALE` is gone, a descriptor says where,
/// and `EPERM` — never gone — decides nothing.
#[test]
fn the_helpers_answer_is_read_as_the_examination_needs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("x");
    std::fs::write(&path, b"").unwrap();
    let fd: OwnedFd = File::open(&path).unwrap().into();
    assert_eq!(answered(Ok(fd)).unwrap(), Whereabouts::At(path));
    assert_eq!(answered(Err(HelperError::Refused(libc::ESTALE))).unwrap(), Whereabouts::Gone);
    assert_eq!(answered(Err(HelperError::Refused(libc::EPERM))).unwrap_err().raw_os_error(), Some(libc::EPERM));
    assert!(answered(Err(HelperError::NotRunning)).is_err());
}

#[test]
fn a_trash_is_known_by_its_place() {
    let home = Path::new("/home/u/.local/share/Trash");
    let mounts = |p: &Path| p == Path::new("/mnt/d");
    let entry = trash_of(Path::new("/home/u/.local/share/Trash/files/a.txt"), Some(home), 1000, &mounts).unwrap();
    assert_eq!(entry.top, Path::new("/home/u/.local/share/Trash/files/a.txt"));
    assert_eq!(entry.info, Path::new("/home/u/.local/share/Trash/info/a.txt.trashinfo"));
    let inside = trash_of(Path::new("/mnt/d/.Trash-1000/files/dir/deep/x"), None, 1000, &mounts).unwrap();
    assert_eq!(inside.top, Path::new("/mnt/d/.Trash-1000/files/dir"));
    assert_eq!(inside.info, Path::new("/mnt/d/.Trash-1000/info/dir.trashinfo"));
    let shared = trash_of(Path::new("/mnt/d/.Trash/1000/files/y"), None, 1000, &mounts).unwrap();
    assert_eq!(shared.shared.as_deref(), Some(Path::new("/mnt/d/.Trash")));
    assert!(trash_of(Path::new("/mnt/d/.Trash-1001/files/y"), None, 1000, &mounts).is_none(), "another user's");
    assert!(trash_of(Path::new("/mnt/d/x/.Trash-1000/files/y"), None, 1000, &mounts).is_none(), "not at the mount's top");
    assert!(trash_of(Path::new("/mnt/e/.Trash-1000/files/y"), None, 1000, &mounts).is_none(), "not a mount");
    assert!(trash_of(Path::new("/home/u/Trash/files/y"), Some(home), 1000, &mounts).is_none());
    assert!(trash_of(Path::new("/home/u/.local/share/Trash/files"), Some(home), 1000, &mounts).is_none(), "the Trash itself");
}
