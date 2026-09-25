//! The worker, end to end (`docs/design/writes.md` §12): rows made by the real
//! examination from a real folder, sent to a fake OneDrive (wiremock, never
//! a network), and the folder, the store and the drive compared afterwards.
//! Crashes are fault points: the worker stops at the step, and a new one is
//! built on the same store and folder.

use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{self, State, XATTR_CTAG, XATTR_ITEM_ID, XATTR_ROOT, XATTR_STAMP, XATTR_STATE, XATTR_SYNC};
use tokio_util::sync::CancellationToken;
use wiremock::ResponseTemplate;

use super::fake::{qx, Harness};
use super::*;
use crate::sync::disk::Disk;
use crate::sync::local::{Batch, Examined, Examiner, FakeLiveness, IgnoreList};
use crate::sync::materialize::{Materializer, Scope};
use crate::tree::outbox::{OutboxKind, OutboxRow, OutboxState};
use crate::tree::{Change, Kind, Placement, Row, Table, TreeStore};

use OutboxKind::{Create, Delete, Mkdir, Move, Update};

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

fn folder(id: &str, parent: &str, name: &str) -> Change {
    Change::Upsert(row(id, Some(parent), name, Kind::Folder, b""))
}

fn file(id: &str, parent: &str, name: &str, content: &[u8]) -> Change {
    Change::Upsert(row(id, Some(parent), name, Kind::File, content))
}

/// A folder placed from a listing by the real materializer, a fake
/// OneDrive holding the same, and a worker for it.
struct World {
    _dir: tempfile::TempDir,
    root: SyncRoot,
    store: Store,
    liveness: FakeLiveness,
    locks: InodeLocks,
    h: Harness,
}

impl World {
    fn new(changes: &[Change]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("OneDrive");
        std::fs::create_dir(&path).unwrap();
        let root_id = "5b0e2c7a-1d3f-4e8a-9b6c-0f1e2d3c4b5a".to_owned();
        xattr::set(&path, XATTR_ROOT, root_id.as_bytes()).unwrap();
        let root = SyncRoot { path, root_id };
        let store = Store::new(TreeStore::in_memory().unwrap());
        let mut all = vec![Change::Root(row("R", None, "", Kind::Folder, b""))];
        all.extend_from_slice(changes);
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
                rescue_into: dir.path().join("rescued"),
                cancel: CancellationToken::new(),
                rw: None,
                claimed: None,
            };
            materializer.apply(Scope::Full).unwrap();
        }
        store.with(|s| s.commit_staging("link-1")).unwrap();
        let locks = InodeLocks::new();
        let h = Harness::new(&root, &store, &locks);
        World { _dir: dir, root, store, liveness: FakeLiveness::new(), locks, h }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path.join(rel)
    }

    fn examine_batch(&self, batch: &Batch) -> Examined {
        let disk = Disk::open(&self.root, false).unwrap();
        let ignore = IgnoreList::default();
        let now = crate::sync::activity::unix_now();
        Examiner { disk: &disk, store: &self.store, liveness: &self.liveness, ignore: &ignore, locks: &self.locks, now }.examine(batch).unwrap()
    }

    fn examine(&self, pairs: &[(&str, &str)]) -> Examined {
        let mut batch = Batch::new();
        for (dir, name) in pairs {
            batch.name(Path::new(dir), OsStr::new(name));
        }
        self.examine_batch(&batch)
    }

    fn rows(&self) -> Vec<OutboxRow> {
        self.store.with(|s| s.outbox_rows()).unwrap()
    }

    fn summary(&self) -> Vec<(OutboxKind, String, OutboxState)> {
        self.rows().into_iter().map(|r| (r.kind, r.rel.display().to_string(), r.state)).collect()
    }

    /// Downloaded, as a fill leaves it.
    fn hydrate(&self, rel: &str, content: &[u8]) {
        let file = File::options().write(true).open(self.path(rel)).unwrap();
        file.set_len(0).unwrap();
        (&file).write_all(content).unwrap();
        placeholder::write_state(&file, State::Hydrated).unwrap();
        placeholder::write_stamp(&file).unwrap();
    }

    /// An edit in place of a downloaded file: new content, a later time.
    fn edit(&self, rel: &str, content: &[u8]) {
        let file = File::options().write(true).truncate(true).open(self.path(rel)).unwrap();
        (&file).write_all(content).unwrap();
        placeholder::set_mtime(&file, std::time::SystemTime::now() + Duration::from_secs(5)).unwrap();
    }

    fn write(&self, rel: &str, content: &[u8]) {
        std::fs::write(self.path(rel), content).unwrap();
    }

    fn rename(&self, from: &str, to: &str) {
        std::fs::rename(self.path(from), self.path(to)).unwrap();
    }

    fn handle(&self, rel: &str) -> FileHandle {
        let path = self.path(rel);
        FileHandle::at(&File::open(path.parent().unwrap()).unwrap(), path.file_name().unwrap()).unwrap()
    }

    fn attr(&self, rel: &str, name: &str) -> Option<String> {
        xattr::get(self.path(rel), name).unwrap().map(|v| String::from_utf8(v).unwrap())
    }

    fn run(&self) -> Arc<Engine> {
        self.h.run()
    }

    fn cloud<T>(&self, f: impl FnOnce(&mut fake::Cloud) -> T) -> T {
        self.h.graph.with(f)
    }

    fn base(&self, id: &str) -> Option<Row> {
        self.store.with(|s| s.get(Table::Items, id)).unwrap()
    }

    /// The cloud's content at `path`.
    fn content(&self, path: &str) -> Option<Vec<u8>> {
        self.cloud(|c| c.at(path).map(|i| i.content.clone()))
    }

    fn id_at(&self, path: &str) -> Option<String> {
        self.cloud(|c| c.at(path).map(|i| i.id.clone()))
    }
}

/// What a commit leaves on a file (§3.5): the item id, `hydrated`, the cTag
/// OneDrive gave, the stamp of the content sent, and no upload mark.
fn assert_committed(w: &World, rel: &str, cloud_path: &str) {
    let id = w.id_at(cloud_path).unwrap_or_else(|| panic!("{cloud_path} is not in OneDrive: {:?}", w.cloud(|c| c.paths())));
    assert_eq!(w.attr(rel, XATTR_ITEM_ID).as_deref(), Some(id.as_str()), "{rel}");
    let is_file = std::fs::metadata(w.path(rel)).unwrap().is_file();
    if is_file {
        let meta = std::fs::metadata(w.path(rel)).unwrap();
        assert_eq!(w.attr(rel, XATTR_STATE).as_deref(), Some("hydrated"), "{rel}");
        assert_eq!(w.attr(rel, XATTR_CTAG), w.cloud(|c| c.at(cloud_path).map(|i| i.ctag.clone())), "{rel}");
        assert_eq!(w.attr(rel, XATTR_STAMP), Some(format!("{} {}.{}", meta.len(), meta.mtime(), meta.mtime_nsec())), "{rel}");
        let here = qx(&std::fs::read(w.path(rel)).unwrap());
        assert_eq!(w.cloud(|c| c.at(cloud_path).and_then(|i| i.hash.clone())), Some(here), "{rel}: the content");
    }
    assert_eq!(w.attr(rel, XATTR_SYNC), None, "{rel}");
    let base = w.base(&id).unwrap();
    assert_eq!(w.store.with(|s| s.local_handle(&id)).unwrap(), Some(w.handle(rel)), "{rel}");
    assert_eq!(Some(base.name.as_str()), cloud_path.rsplit('/').next());
}

/// New folders and files go up, parents first; each is committed on the
/// file and in the store; the examination then finds nothing (§4.1, §4.2).
#[test]
fn new_folders_and_files_go_up_and_are_committed() {
    let w = World::new(&[]);
    std::fs::create_dir_all(w.path("docs/deep")).unwrap();
    w.write("docs/deep/a.txt", b"alpha");
    w.write("docs/empty", b"");
    w.write("top.txt", b"top");
    w.examine(&[("", "docs"), ("", "top.txt")]);
    let mut kinds: Vec<OutboxKind> = w.rows().iter().map(|r| r.kind).collect();
    kinds.sort();
    assert_eq!(kinds, vec![Create, Create, Create, Mkdir, Mkdir]);
    assert_eq!(w.attr("top.txt", XATTR_SYNC), None);

    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["docs", "docs/deep", "docs/deep/a.txt", "docs/empty", "top.txt"]);
    for (rel, path) in [("docs", "docs"), ("docs/deep", "docs/deep"), ("docs/deep/a.txt", "docs/deep/a.txt"), ("docs/empty", "docs/empty"), ("top.txt", "top.txt")] {
        assert_committed(&w, rel, path);
    }
    assert_eq!(w.h.host.kinds().iter().filter(|k| *k == kind::UPLOADED).count(), 5);
    assert_eq!(w.store.with(|s| s.outbox_seq()).unwrap(), 5);
    // Echo, locally: the next examination of everything finds nothing to send.
    w.examine_batch(&Batch::full());
    assert!(w.rows().is_empty(), "{:?}", w.summary());
}

/// An edit goes up guarded by the base's eTag, a rename is a PATCH, and a
/// row behind a running one is sent after it.
#[test]
fn edits_and_renames_go_up_guarded() {
    let w = World::new(&[folder("D", "R", "docs"), file("A", "R", "a.txt", b"old")]);
    w.hydrate("a.txt", b"old");
    w.edit("a.txt", b"new content");
    w.examine(&[("", "a.txt")]);
    assert_eq!(w.summary(), vec![(Update, "a.txt".into(), OutboxState::Ready)]);
    w.run();
    assert!(w.rows().is_empty());
    assert_committed(&w, "a.txt", "a.txt");
    assert_eq!(w.id_at("a.txt").as_deref(), Some("A"), "the item keeps its id");

    w.rename("a.txt", "docs/b.txt");
    w.examine(&[("", "a.txt"), ("docs", "b.txt")]);
    assert_eq!(w.summary(), vec![(Move, "docs/b.txt".into(), OutboxState::Ready)]);
    w.run();
    assert!(w.rows().is_empty());
    assert_eq!(w.cloud(|c| c.paths()), vec!["docs", "docs/b.txt"]);
    assert_eq!(w.base("A").map(|r| (r.parent_id, r.name)), Some((Some("D".into()), "b.txt".into())));
    assert!(w.h.host.kinds().contains(&kind::CLOUD_MOVED.to_owned()));

    // Moved and changed at once: one row, the move first, then the content
    // against the eTag the move answered with (§3.5).
    w.rename("docs/b.txt", "c.txt");
    w.edit("c.txt", b"third");
    w.examine(&[("docs", "b.txt"), ("", "c.txt")]);
    assert_eq!(w.summary(), vec![(Update, "c.txt".into(), OutboxState::Ready)]);
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["c.txt", "docs"]);
    assert_committed(&w, "c.txt", "c.txt");
}

/// F55 (4): an edit of a download that is older than the base (a new
/// version not yet downloaded over it) is queued with that download's cTag
/// and no eTag; the worker guards it with that cTag, the guard fails, and
/// both versions are kept.
#[test]
fn an_edit_of_an_outdated_download_is_guarded_by_its_ctag() {
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.hydrate("a.txt", b"old");
    // A newer version came in the delta; the file is not replaced yet.
    w.cloud(|c| c.edit("A", b"newer"));
    let newer = w.cloud(|c| c.item("A").cloned().unwrap());
    w.store
        .with(|s| {
            let mut base = s.get(Table::Items, "A")?.unwrap();
            base.etag = Some(newer.etag.clone());
            base.ctag = Some(newer.ctag.clone());
            base.quickxor = newer.hash.clone();
            base.size = newer.size;
            s.begin_staging(true)?;
            s.stage(&[Change::Upsert(base)])?;
            s.commit_staging("link-2")
        })
        .unwrap();
    w.edit("a.txt", b"mine, from the old one");
    w.examine(&[("", "a.txt")]);
    let row = &w.rows()[0];
    assert_eq!((row.kind, row.base.as_ref().and_then(|b| b.etag.clone()), row.base.as_ref().and_then(|b| b.ctag.clone())), (Update, None, Some("c-A".into())));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert!(w.cloud(|c| c.guards.iter().any(|(path, tag)| path.ends_with("items/A/createUploadSession") && tag == "c-A")), "{:?}", w.cloud(|c| c.guards.clone()));
    assert_eq!(w.cloud(|c| c.paths()), vec!["a-fedora.txt", "a.txt"]);
    assert_eq!(w.content("a.txt").unwrap(), b"newer");
    assert_committed(&w, "a-fedora.txt", "a-fedora.txt");
}

/// §4.8: a file above the one-request size goes in fragments, persisted as
/// it goes; stopped mid-session, the next start resumes where the server
/// stands and sends only the rest.
#[test]
fn a_large_file_resumes_mid_session_after_a_crash() {
    let w = World::new(&[]);
    let content: Vec<u8> = (0..(1024 * 1024 + 77)).map(|i| (i % 251) as u8).collect();
    w.write("big.bin", &content);
    w.examine(&[("", "big.bin")]);
    let engine = w.h.engine();
    engine.arm(Fault::MidSession(2));
    w.h.drain(&engine);
    let row = &w.rows()[0];
    assert_eq!(row.state, OutboxState::Running, "stopped as by a crash");
    assert!(row.session_url.is_some());
    assert_eq!(row.session_next, Some(2 * 320 * 1024));
    let sent_before = w.cloud(|c| c.count("PUT", "upload/"));
    assert_eq!(sent_before, 2);

    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.count("PUT", "upload/")) - sent_before, 2, "only the rest: 1 MiB + 77 is four fragments");
    assert_eq!(w.cloud(|c| c.count("POST", "createUploadSession")), 1, "the same session");
    assert_eq!(w.content("big.bin").unwrap(), content);
    assert_committed(&w, "big.bin", "big.bin");
}

/// §5, one crash row at a time: each step replayed on a new worker reaches
/// the same end — one item in OneDrive, adopted by hash, by place or by
/// kind, never a copy, and the outbox empty.
#[test]
fn every_crash_point_is_replayed_to_the_same_end() {
    let big: Vec<u8> = (0..(700 * 1024)).map(|i| (i % 253) as u8).collect();
    let cases: Vec<(&str, Fault)> = vec![
        ("create", Fault::AfterSend),
        ("create", Fault::CommitStep1Partial),
        ("create", Fault::AfterCommitStep1),
        ("update", Fault::AfterSend),
        ("update", Fault::AfterCommitStep1),
        ("large", Fault::SessionNotPersisted),
        ("large", Fault::AfterSend),
        ("mkdir", Fault::AfterSend),
        ("move", Fault::AfterSend),
        ("delete", Fault::AfterSend),
    ];
    for (what, fault) in cases {
        let w = World::new(&[file("A", "R", "a.txt", b"old"), file("B", "R", "b.txt", b"b")]);
        w.hydrate("a.txt", b"old");
        w.hydrate("b.txt", b"b");
        let (rel, expect): (&str, Vec<&str>) = match what {
            "create" => {
                w.write("n.txt", b"new file");
                w.examine(&[("", "n.txt")]);
                ("n.txt", vec!["a.txt", "b.txt", "n.txt"])
            }
            "update" => {
                w.edit("a.txt", b"edited");
                w.examine(&[("", "a.txt")]);
                ("a.txt", vec!["a.txt", "b.txt"])
            }
            "large" => {
                w.write("big.bin", &big);
                w.examine(&[("", "big.bin")]);
                ("big.bin", vec!["a.txt", "b.txt", "big.bin"])
            }
            "mkdir" => {
                std::fs::create_dir(w.path("dir")).unwrap();
                w.examine(&[("", "dir")]);
                ("dir", vec!["a.txt", "b.txt", "dir"])
            }
            "move" => {
                w.rename("b.txt", "c.txt");
                w.examine(&[("", "b.txt"), ("", "c.txt")]);
                ("c.txt", vec!["a.txt", "c.txt"])
            }
            _ => {
                std::fs::remove_file(w.path("b.txt")).unwrap();
                w.examine(&[("", "b.txt")]);
                ("", vec!["a.txt"])
            }
        };
        assert_eq!(w.rows().len(), 1, "{what}: {:?}", w.summary());
        let engine = w.h.engine();
        engine.arm(fault);
        w.h.drain(&engine);
        assert_eq!(w.rows()[0].state, OutboxState::Running, "{what} {fault:?}: stopped at the step");

        w.run();
        assert!(w.rows().is_empty(), "{what} {fault:?}: {:?}", w.summary());
        assert_eq!(w.cloud(|c| c.paths()), expect, "{what} {fault:?}");
        if !rel.is_empty() {
            assert_committed(&w, rel, rel);
        }
        assert!(w.cloud(|c| c.bin.keys().all(|id| id == "B")), "{what} {fault:?}: nothing else deleted");
    }
}

/// §6, the cells where both sides changed one item.
#[test]
fn conflicts_keep_both_and_the_first_rename_wins() {
    // edit × edit: the local version becomes a copy beside OneDrive's.
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.hydrate("a.txt", b"old");
    w.edit("a.txt", b"mine");
    w.examine(&[("", "a.txt")]);
    w.cloud(|c| c.edit("A", b"theirs"));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["a-fedora.txt", "a.txt"]);
    assert_eq!(w.content("a.txt").unwrap(), b"theirs");
    assert_eq!(w.content("a-fedora.txt").unwrap(), b"mine");
    assert_committed(&w, "a-fedora.txt", "a-fedora.txt");
    assert!(!w.path("a.txt").exists(), "the cloud's version is placed at the name by the reconcile");
    let copy = w.path("a-fedora.txt").display().to_string();
    assert_eq!(w.store.with(|s| s.conflict_kind(&copy)).unwrap().as_deref(), Some("copy"));
    assert_eq!(w.store.with(|s| s.local_handle("A")).unwrap(), None, "never taken for a delete");
    assert!(w.h.host.kinds().contains(&kind::CONFLICT.to_owned()));
    assert!(w.h.host.cycles.load(Ordering::SeqCst) > 0);
    // the outbox on the bus: the delta carries OneDrive's version; no Full
    // reconcile, which until the read-write reconcile puts waiting renames back.
    assert_eq!(w.h.host.fulls.load(Ordering::SeqCst), 0, "a copy asks for no Full reconcile");

    // edit × rename there: the content goes to the renamed item, and the
    // file takes OneDrive's name.
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.hydrate("a.txt", b"old");
    w.edit("a.txt", b"mine");
    w.examine(&[("", "a.txt")]);
    w.cloud(|c| c.rename("A", "R", "c.txt"));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["c.txt"]);
    assert_committed(&w, "c.txt", "c.txt");

    // edit × delete there: uploaded again as new (local wins).
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.hydrate("a.txt", b"old");
    w.edit("a.txt", b"mine");
    w.examine(&[("", "a.txt")]);
    w.cloud(|c| {
        let gone = c.items.remove("A").unwrap();
        c.bin.insert("A".into(), gone);
    });
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_ne!(w.id_at("a.txt").as_deref(), Some("A"), "a new item");
    assert_committed(&w, "a.txt", "a.txt");
    assert!(w.base("A").is_none());
    assert!(w.h.host.kinds().contains(&kind::RESTORED.to_owned()));

    // rename × edit there: the rename goes again with the fresh eTag.
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.rename("a.txt", "b.txt");
    w.examine(&[("", "a.txt"), ("", "b.txt")]);
    w.cloud(|c| c.edit("A", b"theirs"));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["b.txt"]);
    assert_eq!(w.content("b.txt").unwrap(), b"theirs");

    // rename × rename: the first to reach OneDrive wins; the file follows.
    let w = World::new(&[file("A", "R", "a.txt", b"old")]);
    w.rename("a.txt", "b.txt");
    w.examine(&[("", "a.txt"), ("", "b.txt")]);
    w.cloud(|c| c.rename("A", "R", "c.txt"));
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["c.txt"]);
    assert!(w.path("c.txt").exists() && !w.path("b.txt").exists());
    assert_eq!(w.base("A").unwrap().name, "c.txt");

    // rename × delete: a downloaded file goes up as new; a placeholder
    // follows the delete.
    let w = World::new(&[file("A", "R", "a.txt", b"old"), file("P", "R", "p.bin", b"only there")]);
    w.hydrate("a.txt", b"old");
    w.rename("a.txt", "b.txt");
    w.rename("p.bin", "q.bin");
    w.examine(&[("", "a.txt"), ("", "b.txt"), ("", "p.bin"), ("", "q.bin")]);
    w.cloud(|c| {
        for id in ["A", "P"] {
            let gone = c.items.remove(id).unwrap();
            c.bin.insert(id.into(), gone);
        }
    });
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["b.txt"]);
    assert_committed(&w, "b.txt", "b.txt");
    assert!(!w.path("q.bin").exists(), "a placeholder holds nothing here");
    assert!(w.base("P").is_none());

    // delete × edit: OneDrive's version comes back; nothing is deleted.
    let w = World::new(&[file("A", "R", "a.txt", b"old"), file("B", "R", "b.txt", b"b"), file("C", "R", "c.txt", b"c")]);
    for rel in ["a.txt", "b.txt", "c.txt"] {
        std::fs::remove_file(w.path(rel)).unwrap();
    }
    w.examine(&[("", "a.txt"), ("", "b.txt"), ("", "c.txt")]);
    w.cloud(|c| {
        c.edit("A", b"theirs");
        c.rename("B", "R", "b2.txt");
        let gone = c.items.remove("C").unwrap();
        c.bin.insert("C".into(), gone);
    });
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    // edit: kept, and placed again (its local handle forgotten);
    // rename: the content the user deleted, deleted with the fresh eTag;
    // delete: done.
    assert_eq!(w.cloud(|c| c.paths()), vec!["a.txt"]);
    assert_eq!(w.store.with(|s| s.local_handle("A")).unwrap(), None);
    assert!(w.base("A").is_some() && w.base("B").is_none() && w.base("C").is_none());
    assert!(w.h.host.kinds().contains(&kind::RESTORED.to_owned()));
    assert!(w.h.host.fulls.load(Ordering::SeqCst) > 0, "placed again by a Full reconcile");

    // create × create: the same content is adopted, other content copied.
    let w = World::new(&[]);
    w.write("same.txt", b"same");
    w.write("x.txt", b"mine");
    w.examine(&[("", "same.txt"), ("", "x.txt")]);
    w.cloud(|c| {
        c.add_file("S", "R", "same.txt", b"same");
        c.add_file("X", "R", "X.TXT", b"theirs");
    });
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["X.TXT", "same.txt", "x-fedora.txt"]);
    assert_eq!(w.attr("same.txt", XATTR_ITEM_ID).as_deref(), Some("S"), "adopted: nothing sent");
    assert_eq!(w.content("x-fedora.txt").unwrap(), b"mine");
    assert_eq!(w.content("X.TXT").unwrap(), b"theirs");
}

/// §4.7: a folder deleted here whose cTag moved in OneDrive because
/// something was added there: only what the base knew unchanged goes; the
/// folder and the new file stay, to be placed here again.
#[test]
fn a_folder_changed_in_onedrive_is_deleted_only_in_part() {
    let w = World::new(&[folder("F", "R", "f"), file("A", "F", "a.txt", b"a"), folder("S", "F", "sub"), file("B", "S", "b.txt", b"b")]);
    std::fs::remove_dir_all(w.path("f")).unwrap();
    w.examine(&[("", "f")]);
    assert_eq!(w.summary(), vec![(Delete, "f".into(), OutboxState::Ready)]);
    w.cloud(|c| {
        c.add_file("NEW", "F", "new.txt", b"added there");
        c.touch("F");
    });
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["f", "f/new.txt"]);
    let mut binned: Vec<String> = w.cloud(|c| c.bin.keys().cloned().collect());
    binned.sort();
    assert_eq!(binned, vec!["A", "B", "S"], "the subfolder whole, with nothing new in it");
    assert!(w.base("F").is_some() && w.base("A").is_none() && w.base("S").is_none());
    assert_eq!(w.store.with(|s| s.local_handle("F")).unwrap(), None, "placed again, never deleted");
    assert!(w.h.host.fulls.load(Ordering::SeqCst) > 0, "by a Full reconcile");
}

/// F55 (7) (d): a swap, a folder replaced by its own subfolder, and a
/// folder wrapped in a new one of its name, end to end: each goes through
/// a temporary name, nothing is adopted or copied, and nothing but what the
/// user deleted is deleted.
#[test]
fn swaps_and_folders_replaced_in_place_go_through_a_temporary_name() {
    // The subfolder: mv F/sub F.tmp && rm -rf F && mv F.tmp F.
    let w = World::new(&[folder("F", "R", "F"), folder("S", "F", "sub"), file("A", "S", "a.txt", b"a"), file("B", "F", "b.txt", b"b")]);
    w.rename("F/sub", "F.tmp");
    std::fs::remove_dir_all(w.path("F")).unwrap();
    w.rename("F.tmp", "F");
    w.examine(&[("F", "sub"), ("", "F.tmp"), ("F", "b.txt"), ("", "F")]);
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["F", "F/a.txt"]);
    assert_eq!(w.id_at("F").as_deref(), Some("S"));
    assert!(w.cloud(|c| c.bin.contains_key("F") && c.bin.contains_key("B") && !c.bin.contains_key("A")));
    assert!(w.cloud(|c| c.count("PATCH", "items/S")) >= 2, "through the temporary name");

    // The wrap: mkdir t && mv d t/ && mv t d.
    let w = World::new(&[folder("D", "R", "d"), file("X", "D", "x.txt", b"x")]);
    std::fs::create_dir(w.path("t")).unwrap();
    w.rename("d", "t/d");
    w.rename("t", "d");
    w.examine(&[("", "t"), ("", "d"), ("t", "d")]);
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["d", "d/d", "d/d/x.txt"]);
    assert_eq!(w.id_at("d/d").as_deref(), Some("D"));
    assert!(w.cloud(|c| c.bin.is_empty()));
    assert_committed(&w, "d", "d");

    // A swap: a and b exchanged.
    let w = World::new(&[file("A", "R", "a", b"a"), file("B", "R", "b", b"b")]);
    w.rename("a", "tmp");
    w.rename("b", "a");
    w.rename("tmp", "b");
    w.examine(&[("", "a"), ("", "b"), ("", "tmp")]);
    assert_eq!(w.rows().len(), 2, "{:?}", w.summary());
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!((w.id_at("a").as_deref(), w.id_at("b").as_deref()), (Some("B"), Some("A")));
    assert!(w.cloud(|c| c.bin.is_empty() && c.paths().iter().all(|p| !p.contains(SWAP_PREFIX))));
}

/// §4.10: a throttle stops the whole worker for as long as OneDrive asked —
/// `Retry-After` in seconds or as an HTTP date — and the row keeps its place.
#[test]
fn throttling_pauses_the_whole_worker() {
    for header in ["120", "date"] {
        let w = World::new(&[]);
        w.write("a.txt", b"a");
        w.examine(&[("", "a.txt")]);
        let now = crate::sync::activity::unix_now();
        let value = if header == "date" { http_date(now + 300) } else { header.to_owned() };
        w.cloud(|c| c.script("POST", "createUploadSession", ResponseTemplate::new(429).insert_header("Retry-After", value.as_str()), 1));
        let engine = w.run();
        let until = engine.status().throttled_until.expect("throttled");
        let wanted = if header == "date" { now + 300 } else { now + 120 };
        assert!((until - wanted).abs() <= 3, "{header}: {until} vs {wanted}");
        assert_eq!(w.summary(), vec![(Create, "a.txt".into(), OutboxState::Ready)]);
        assert_eq!(w.rows()[0].attempts, 0, "a throttle is no failure of the row");
        w.write("b.txt", b"b");
        w.examine(&[("", "b.txt")]);
        let asked = w.cloud(|c| c.log.len());
        w.h.drain(&engine);
        assert_eq!(w.cloud(|c| c.log.len()), asked, "nothing is sent while throttled");
    }
}

fn http_date(at: i64) -> String {
    let text = crate::drive::item::format_graph_time(at); // 2026-09-25T10:00:00Z
    let (date, time) = text.trim_end_matches('Z').split_once('T').unwrap();
    let mut parts = date.split('-');
    let (year, month, day) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap());
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("Thu, {day} {} {year} {time} GMT", months[month.parse::<usize>().unwrap() - 1])
}

/// Pause (persisted), offline and a sign-in that does not allow writes each
/// stop the worker without touching the rows; a refused name, a full
/// OneDrive and a writer each block or hold their own row; the file's
/// `user.konedrive.sync` says which.
#[test]
fn pause_offline_sign_in_and_blocked_rows() {
    let w = World::new(&[]);
    w.write("a.txt", b"a");
    w.examine(&[("", "a.txt")]);
    let engine = w.h.engine();
    engine.pause(None).unwrap();
    w.h.drain(&engine);
    assert_eq!(w.cloud(|c| c.log.len()), 0);
    assert_eq!((engine.status().paused, engine.status().paused_until), (true, 0));
    assert_eq!(w.attr("a.txt", XATTR_SYNC).as_deref(), Some("pending"));
    let restarted = w.h.engine();
    w.h.drain(&restarted);
    assert!(restarted.status().paused, "the pause survives a restart");
    restarted.resume().unwrap();
    restarted.set_online(false);
    w.h.drain(&restarted);
    assert_eq!(w.cloud(|c| c.log.len()), 0);
    restarted.set_online(true);
    w.cloud(|c| c.script("POST", "createUploadSession", ResponseTemplate::new(403), 1));
    w.h.drain(&restarted);
    assert!(restarted.status().needs_sign_in);
    assert_eq!(w.summary(), vec![(Create, "a.txt".into(), OutboxState::Blocked)]);
    assert_eq!(w.attr("a.txt", XATTR_SYNC).as_deref(), Some("blocked"));
    restarted.signed_in().unwrap();
    w.h.drain(&restarted);
    assert!(w.rows().is_empty());
    assert_eq!(w.attr("a.txt", XATTR_SYNC), None);

    // Refused by the service, OneDrive full, open for writing.
    w.write("refused.txt", b"r");
    w.write("full.txt", b"f");
    w.write("open.txt", b"o");
    w.examine(&[("", "refused.txt"), ("", "full.txt"), ("", "open.txt")]);
    let writer = File::options().append(true).open(w.path("open.txt")).unwrap();
    w.cloud(|c| {
        c.script("POST", "refused.txt", ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": {"code": "invalidRequest", "message": "bad name"}})), 1);
        c.script("POST", "full.txt", ResponseTemplate::new(507), 1);
    });
    w.h.drain(&restarted);
    let state = |rel: &str| w.rows().into_iter().find(|r| r.rel == Path::new(rel)).map(|r| (r.state, r.reason.unwrap_or_default()));
    assert_eq!(state("refused.txt"), Some((OutboxState::Blocked, "refused: bad name".into())));
    assert_eq!(state("full.txt"), Some((OutboxState::Blocked, reason::QUOTA.into())));
    assert_eq!(state("open.txt").map(|s| s.0), Some(OutboxState::Waiting));
    assert_eq!(w.attr("refused.txt", XATTR_SYNC).as_deref(), Some("blocked"));
    assert_eq!(w.attr("open.txt", XATTR_SYNC).as_deref(), Some("pending"));
    let counts = restarted.counts().unwrap();
    assert_eq!((counts.pending, counts.blocked, counts.pending_bytes), (1, 2, 1));
    assert_eq!(w.h.host.kinds().iter().filter(|k| *k == kind::UPLOAD_FAILED).count(), 3, "forbidden, refused, full: once each");
    drop(writer);
    restarted.quota_changed().unwrap();
    restarted.retry_now().unwrap();
    w.h.drain(&restarted);
    assert_eq!(w.summary(), vec![(Create, "refused.txt".into(), OutboxState::Blocked)]);
    assert_committed(&w, "full.txt", "full.txt");
    assert_committed(&w, "open.txt", "open.txt");
}

/// The worker as the mode switch will run it: started, woken, stopped.
#[test]
fn the_worker_runs_until_stopped() {
    let w = World::new(&[]);
    let worker = OutboxWorker::new(w.h.config());
    w.h.runtime.block_on(async {
        worker.start();
        assert!(worker.status().started);
        w.write("a.txt", b"a");
        w.examine(&[("", "a.txt")]);
        worker.wake();
        let mut waited = 0;
        while !w.rows().is_empty() && waited < 200 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += 1;
        }
        worker.stop().await;
    });
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert!(!worker.status().started);
    assert_committed(&w, "a.txt", "a.txt");
}

/// Four independent small files run at once (`Limits::small_slots`), and a
/// child waits for its parent's `mkdir`: the row only sends once the folder
/// it goes into is in OneDrive.
#[test]
fn four_independent_files_run_at_once_and_a_child_waits_for_its_mkdir() {
    let w = World::new(&[]);
    let names = ["a.txt", "b.txt", "c.txt", "d.txt"];
    for name in names {
        w.write(name, name.as_bytes());
    }
    std::fs::create_dir_all(w.path("dir")).unwrap();
    w.write("dir/child.txt", b"child");
    w.examine(&[("", "a.txt"), ("", "b.txt"), ("", "c.txt"), ("", "d.txt"), ("", "dir")]);
    assert_eq!(w.rows().len(), 6, "{:?}", w.summary());

    // Each small file opens an upload session first (`POST
    // createUploadSession`); held open long enough for the poll below to
    // catch all four at once, without holding up the mkdir or the child
    // behind it.
    w.cloud(|c| {
        for name in names {
            c.delay("POST", name, Duration::from_millis(150), 1);
        }
    });

    let worker = OutboxWorker::new(w.h.config());
    let peak = w.h.runtime.block_on(async {
        worker.start();
        worker.wake();
        let mut peak = 0;
        let mut waited = 0;
        while waited < 300 {
            peak = peak.max(worker.status().uploads.len());
            if peak >= 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            waited += 1;
        }
        waited = 0;
        while !w.rows().is_empty() && waited < 300 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            waited += 1;
        }
        worker.stop().await;
        peak
    });

    assert_eq!(peak, 4, "four independent small files were in flight together");
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    for (rel, path) in [("a.txt", "a.txt"), ("b.txt", "b.txt"), ("c.txt", "c.txt"), ("d.txt", "d.txt"), ("dir", "dir"), ("dir/child.txt", "dir/child.txt")] {
        assert_committed(&w, rel, path);
    }
    let position = |fragment: &str| w.cloud(|c| c.log.iter().position(|(_, p)| p.contains(fragment)));
    let mkdir_at = position("children").expect("the mkdir request");
    let child_at = position("child.txt").expect("the child's content request");
    assert!(mkdir_at < child_at, "the child's row waits for its parent's mkdir: {mkdir_at} vs {child_at}");
}

// Fix round 1 (the outbox worker review): one test per Critical and Important finding.

/// The base row a delta cycle would stage for what OneDrive holds as `id`.
fn staged(w: &World, id: &str) -> Row {
    let item = w.cloud(|c| c.item(id).cloned()).unwrap();
    Row {
        id: item.id,
        parent_id: item.parent,
        name: item.name,
        kind: if item.folder { Kind::Folder } else { Kind::File },
        size: item.size,
        mtime: item.mtime,
        etag: Some(item.etag),
        ctag: Some(item.ctag),
        quickxor: item.hash,
        mime: None,
        placement: Placement::Placed,
    }
}

/// A cycle between the detection and the send, as §4.9 runs one first.
fn cycle(w: &World, ids: &[&str]) {
    let rows: Vec<Change> = ids.iter().map(|id| Change::Upsert(staged(w, id))).collect();
    w.store
        .with(|s| {
            s.begin_staging(true)?;
            s.stage(&rows)?;
            s.commit_staging("link-2")
        })
        .unwrap();
}

/// C1: `rm -rf` offline; meanwhile OneDrive gets a new file and an edit
/// in the folder, and a cycle brings both into the base before the worker
/// runs. The delete is compared with what the base held when it was
/// decided: the addition and the edit stay, only what this machine saw
/// unchanged goes.
#[test]
fn a_folder_delete_never_takes_what_a_cycle_brought_in() {
    let w = World::new(&[folder("F", "R", "photos"), file("A", "F", "a.txt", b"a"), file("X", "F", "x.txt", b"x")]);
    std::fs::remove_dir_all(w.path("photos")).unwrap();
    w.examine(&[("", "photos")]);
    assert_eq!(w.summary(), vec![(Delete, "photos".into(), OutboxState::Ready)]);
    let seq = w.rows()[0].seq;
    let seen = w.store.with(|s| s.outbox_seen(seq)).unwrap().expect("remembered when the delete was decided");
    let mut ids: Vec<&String> = seen.keys().collect();
    ids.sort();
    assert_eq!(ids, vec!["A", "F", "X"]);

    w.cloud(|c| {
        c.add_file("N", "F", "new.jpg", b"from the phone");
        c.touch("F");
        c.edit("X", b"edited there");
    });
    cycle(&w, &["N", "X", "F"]);
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["photos", "photos/new.jpg", "photos/x.txt"]);
    assert_eq!(w.cloud(|c| c.bin.keys().cloned().collect::<Vec<_>>()), vec!["A"]);
    assert_eq!(w.content("photos/x.txt").unwrap(), b"edited there");
    assert_eq!(w.store.with(|s| s.local_handle("F")).unwrap(), None, "placed again, never deleted");
}

/// C2: a folder holding a OneNote notebook, which is never placed here,
/// looks empty locally; removing it never sends the notebook to the
/// recycle bin with it.
#[test]
fn a_folder_holding_what_was_never_placed_here_is_not_deleted() {
    let notebook = Row { placement: Placement::Skipped(crate::tree::SkipReason::OneNote), ..row("NB", Some("F"), "Notebook", Kind::Folder, b"") };
    let w = World::new(&[folder("F", "R", "notes"), file("A", "F", "a.txt", b"a"), Change::Upsert(notebook)]);
    assert!(!w.path("notes/Notebook").exists());
    std::fs::remove_dir_all(w.path("notes")).unwrap();
    w.examine(&[("", "notes")]);
    assert_eq!(w.summary(), vec![(Delete, "notes".into(), OutboxState::Ready)]);
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.count("DELETE", "items/F")), 0, "the folder itself is never deleted");
    assert_eq!(w.cloud(|c| c.paths()), vec!["notes", "notes/Notebook"]);
    assert_eq!(w.cloud(|c| c.bin.keys().cloned().collect::<Vec<_>>()), vec!["A"]);
}

/// I1: a swap where one side was also edited. The edited file goes through
/// a temporary name; its content is throttled after the PATCH to that name
/// landed, and the replay meets `412`. The file keeps its name here, and
/// OneDrive ends with both files swapped and nothing under a temporary name.
/// Then the same for a move whose PATCH answer was lost and whose row an
/// examination merged into before the replay: it keeps its temporary name.
#[test]
fn a_row_through_a_temporary_name_keeps_the_users_name_after_a_retry() {
    for edited in [true, false] {
        let w = World::new(&[file("A", "R", "a", b"a"), file("B", "R", "b", b"b")]);
        w.hydrate("a", b"a");
        w.hydrate("b", b"b");
        w.rename("a", "tmp");
        w.rename("b", "a");
        w.rename("tmp", "b");
        if edited {
            w.edit("b", b"a, edited");
        }
        w.examine(&[("", "a"), ("", "b"), ("", "tmp")]);
        let of = |id: &str| w.rows().into_iter().find(|r| r.item_id.as_deref() == Some(id)).unwrap();
        // B's move is held back, so that A's row meets b still taken.
        let b_seq = of("B").seq;
        w.store.with(|s| s.outbox_set_state(b_seq, OutboxState::Held, None, None)).unwrap();
        let engine = w.h.engine();
        if edited {
            w.cloud(|c| c.script("POST", "createUploadSession", ResponseTemplate::new(429).insert_header("Retry-After", "1"), 1));
        } else {
            engine.arm(Fault::AfterSend);
        }
        w.h.drain(&engine);
        let a = of("A");
        assert!(a.target_name.as_deref().is_some_and(|n| n.starts_with(SWAP_PREFIX)), "{edited}: {a:?}");
        assert!(w.cloud(|c| c.item("A").unwrap().name.starts_with(SWAP_PREFIX)), "the PATCH to the temporary name landed");
        if !edited {
            // Backed off, then examined: the merge keeps the temporary name.
            w.store.with(|s| s.outbox_set_state(a.seq, OutboxState::Retry, Some("test"), Some(0))).unwrap();
            w.examine(&[("", "a"), ("", "b")]);
            assert_eq!(of("A").target_name, a.target_name, "a replay looks for it there");
        }
        w.store.with(|s| s.outbox_set_state(b_seq, OutboxState::Ready, None, None)).unwrap();
        w.run();
        assert!(w.rows().is_empty(), "{edited}: {:?}", w.summary());
        let here: Vec<String> = std::fs::read_dir(&w.root.path).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(here.iter().all(|n| !n.starts_with(SWAP_PREFIX)), "{edited}: {here:?}");
        assert_eq!((w.id_at("a").as_deref(), w.id_at("b").as_deref()), (Some("B"), Some("A")), "{edited}");
        assert_eq!((w.attr("a", XATTR_ITEM_ID).as_deref(), w.attr("b", XATTR_ITEM_ID).as_deref()), (Some("B"), Some("A")));
        assert!(w.cloud(|c| c.bin.is_empty() && c.paths().iter().all(|p| !p.contains(SWAP_PREFIX))), "{edited}");
        if edited {
            assert_eq!(w.content("b").unwrap(), b"a, edited");
            assert_committed(&w, "b", "b");
        }
    }
}

/// I2: a large upload stopped mid-session, then the file saved again with
/// the same size, and the worker stopped while it cancels the old session.
/// That session is never resumed with the new bytes: what OneDrive ends
/// with is exactly the new content.
#[test]
fn a_session_is_never_resumed_with_other_content() {
    let w = World::new(&[]);
    let first: Vec<u8> = (0..(700 * 1024)).map(|i| (i % 251) as u8).collect();
    let second: Vec<u8> = (0..(700 * 1024)).map(|i| (i % 241) as u8).collect();
    w.write("big.bin", &first);
    w.examine(&[("", "big.bin")]);
    let engine = w.h.engine();
    engine.arm(Fault::MidSession(1));
    w.h.drain(&engine);
    assert!(w.rows()[0].session_url.is_some());

    let file = File::options().write(true).open(w.path("big.bin")).unwrap();
    (&file).write_all(&second).unwrap();
    placeholder::set_mtime(&file, std::time::SystemTime::now() + Duration::from_secs(5)).unwrap();
    drop(file);
    // The cancel of the old session hangs, and the worker is stopped then.
    w.cloud(|c| c.script("DELETE", "upload/", ResponseTemplate::new(204).set_delay(Duration::from_secs(5)), 1));
    let worker = OutboxWorker::new(w.h.config());
    w.h.runtime.block_on(async {
        worker.start();
        let mut waited = 0;
        while w.cloud(|c| c.count("DELETE", "upload/")) == 0 && waited < 250 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += 1;
        }
        worker.stop().await;
    });
    let row = &w.rows()[0];
    assert_eq!(row.session_url, None, "the session went with the content it was opened for");
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.content("big.bin").unwrap(), second);
    assert_committed(&w, "big.bin", "big.bin");
}

/// I3: a delete × edit dropped while a cycle is between staging and swap.
/// The drop waits for the cycle's swap (the tree lock), so the swap cannot
/// give the item back the local object it forgot — which would make the
/// next examination delete OneDrive's newer version.
#[test]
fn delete_commits_wait_for_the_cycles_swap() {
    let w = World::new(&[file("A", "R", "a.txt", b"a"), file("B", "R", "b.txt", b"b")]);
    std::fs::remove_file(w.path("a.txt")).unwrap();
    w.examine(&[("", "a.txt")]);
    w.cloud(|c| c.edit("A", b"theirs"));
    let lock = w.h.runtime.block_on(Arc::clone(&w.h.tree_lock).lock_owned());
    w.store.with(|s| s.begin_staging(true)).unwrap();
    let engine = w.h.engine();
    let task = w.h.runtime.spawn({
        let engine = Arc::clone(&engine);
        async move { engine.drain(&CancellationToken::new()).await }
    });
    let mut waited = 0;
    while w.cloud(|c| c.count("GET", "items/A")) == 0 && waited < 500 {
        std::thread::sleep(Duration::from_millis(10));
        waited += 1;
    }
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(w.rows().len(), 1, "the drop waits for the cycle");
    w.store.with(|s| s.commit_staging("link-2")).unwrap();
    drop(lock);
    w.h.runtime.block_on(task).unwrap();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.store.with(|s| s.local_handle("A")).unwrap(), None, "forgotten after the swap, not before");
    assert!(w.cloud(|c| c.item("A").is_some() && c.bin.is_empty()));
}

// The re-review's residuals.

/// N1: a folder moved and then deleted while its move was being sent; an
/// addition arrives in OneDrive meanwhile, and the move's answer (with the
/// folder's new cTag) is written into the delete behind it. The whole
/// delete is guarded by the cTag recorded when it was decided, so the
/// addition is kept.
#[test]
fn a_whole_folder_delete_is_guarded_by_the_recorded_ctag() {
    let w = World::new(&[folder("F", "R", "f"), file("A", "F", "a.txt", b"a")]);
    w.rename("f", "g");
    w.examine(&[("", "f"), ("", "g")]);
    let moved = w.rows()[0].seq;
    w.store.with(|s| s.outbox_set_state(moved, OutboxState::Running, None, None)).unwrap();
    std::fs::remove_dir_all(w.path("g")).unwrap();
    w.examine(&[("", "g")]);
    assert_eq!(w.summary(), vec![(Move, "g".into(), OutboxState::Running), (Delete, "g".into(), OutboxState::Ready)]);
    w.cloud(|c| {
        c.add_file("N", "F", "new.txt", b"added there");
        c.touch("F");
    });
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["g", "g/new.txt"]);
    assert_eq!(w.cloud(|c| c.bin.keys().cloned().collect::<Vec<_>>()), vec!["A"]);
}

/// N3: a folder delete with no record of what it was decided against (a
/// row from before the record) deletes nothing; the folder is placed again.
#[test]
fn a_folder_delete_without_a_record_deletes_nothing() {
    let w = World::new(&[folder("F", "R", "f"), file("A", "F", "a.txt", b"a")]);
    std::fs::remove_dir_all(w.path("f")).unwrap();
    w.examine(&[("", "f")]);
    let seq = w.rows()[0].seq;
    w.store.with(|s| s.outbox_forget_seen(seq)).unwrap();
    w.run();
    assert!(w.rows().is_empty(), "{:?}", w.summary());
    assert_eq!(w.cloud(|c| c.paths()), vec!["f", "f/a.txt"]);
    assert!(w.cloud(|c| c.bin.is_empty() && c.count("DELETE", "items/") == 0));
    assert_eq!(w.store.with(|s| s.local_handle("F")).unwrap(), None, "placed again");
}

/// The activity words the worker writes are the ones the daemon's list of
/// kinds names, so the window and the CLI can rely on them.
#[test]
fn the_workers_activity_words_are_the_daemons_kinds() {
    use crate::sync::activity::Kind;
    for (word, kind) in [
        (kind::UPLOADED, Kind::Uploaded),
        (kind::CLOUD_MOVED, Kind::CloudMoved),
        (kind::CLOUD_DELETED, Kind::CloudDeleted),
        (kind::UPLOAD_FAILED, Kind::UploadFailed),
        (kind::RESTORED, Kind::Restored),
        (kind::CONFLICT, Kind::Conflict),
    ] {
        assert_eq!(word, kind.as_str());
    }
}
