//! What the daemon takes off the disk itself is never deleted or moved in
//! OneDrive (issue #104): an item removed in OneDrive, and one that stops
//! being placed while it is still there. Against the fake OneDrive, in a
//! read-write folder, with the examination's "where is it now?" answered
//! from where objects really are ([`Scanning`]), so that a stale local
//! object would be proved gone and turned into a `DELETE`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_fs::placeholder::{self, State};

use super::super::{Listing, ListingContext};
use super::tests::{now, world, write_version, Scanning, World};
use crate::folder::disk::Disk;
use crate::upload::fake::ROOT;
use crate::local::{Examined, Examiner, IgnoreList};

/// A name OneDrive has and this folder cannot (over 255 bytes): the item is
/// `skipped:name-too-long`.
fn long_name() -> String {
    format!("{}.txt", "x".repeat(260))
}

/// A Full local scan, run where it is called, its "where is it now?"
/// answered from where the objects really are.
fn scan_now(w: &World) -> Option<Examined> {
    let (root, store, locks, bases) = (w.root.clone(), w.store.clone(), w.locks.clone(), w.everywhere());
    let disk = Disk::open(&root, false).unwrap();
    let liveness = Scanning(bases);
    Examiner { disk: &disk, store: &store, liveness: &liveness, ignore: &IgnoreList::default(), locks: &locks, now: now() }
        .full_scan()
        .map_err(|e| assert!(matches!(e, crate::local::ExamineError::NoBase), "{e}"))
        .ok()
}

impl World {
    /// A read-write listing whose every cycle runs a Full local scan after
    /// the folder was reconciled and before `staging` is swapped in.
    fn listing_scanning_before_swap(self: &Arc<Self>) -> Arc<Listing> {
        let mut writes = self.writes(None);
        let me = Arc::downgrade(self);
        writes.before_swap = Some(Arc::new(move || {
            if let Some(w) = me.upgrade() {
                scan_now(&w);
            }
        }));
        Listing::new(ListingContext { writes: Some(writes), ..self.context_parts() })
    }

    /// Nothing removed or moved in OneDrive by the daemon, and no row in the
    /// outbox that would.
    async fn nothing_deleted_or_moved(&self, when: &str) {
        assert_eq!(self.deletes(), 0, "{when}: a DELETE reached OneDrive");
        assert_eq!(self.graph.with(|c| c.count("PATCH", "items/")), 0, "{when}: a move reached OneDrive");
        let rows = self.store.call(move |s| s.outbox_rows()).await.unwrap();
        assert!(
            !rows.iter().any(|r| r.kind.removes() || r.kind == konedrive_tree::outbox::OutboxKind::Move),
            "{when}: a removal or a move in the outbox: {rows:?}"
        );
    }

    /// The examination of every place a cycle handed over, as the watcher
    /// runs them.
    async fn examine_handed(&self) {
        let handed = std::mem::take(&mut *self.examined.lock().unwrap());
        for batch in handed {
            self.examine(batch).await;
        }
    }

    async fn recorded_handle(&self, id: &str) -> Option<konedrive_fs::handle::FileHandle> {
        let id = id.to_owned();
        self.store.call(move |s| s.local_handle(&id)).await.unwrap()
    }
}

fn handle_of(path: &Path) -> konedrive_fs::handle::FileHandle {
    konedrive_fs::handle::FileHandle::of(&std::fs::File::open(path).unwrap()).unwrap()
}

/// (a) `docs/f.txt` gets a name over 255 bytes in OneDrive, and then its
/// name back. Every cycle runs a Full local scan after its reconcile and
/// before its swap: the window in which the reconcile has taken the file off
/// the disk while `items` still places it, with the inode it had — and, on
/// the way back, in which the new tree places it again while `items` still
/// has it skipped. Reached with the `before_swap` hook: in the daemon the
/// watcher holds the tree lock while it examines, so this window is closed
/// there too (limitations log F190).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_an_item_skipped_and_back_with_a_scan_before_each_swap_is_never_deleted() {
    let w = Arc::new(world().await);
    let listing = w.listing_scanning_before_swap();
    w.cycle(&listing).await;
    assert_eq!(id_at(&w.path("docs/f.txt")).as_deref(), Some("F"));

    w.graph.with(|c| c.rename("F", "D", &long_name()));
    w.cycle(&listing).await;
    w.examine_handed().await;
    w.scan_and_upload().await;
    w.nothing_deleted_or_moved("skipped").await;
    w.cycle(&listing).await;
    w.scan_and_upload().await;
    w.nothing_deleted_or_moved("skipped, a cycle later").await;
    assert!(!w.path("docs/f.txt").exists(), "a skipped item is not on disk");

    w.graph.with(|c| c.rename("F", "D", "f.txt"));
    w.cycle(&listing).await;
    w.cycle(&listing).await;
    w.scan_and_upload().await;
    w.nothing_deleted_or_moved("back").await;
    assert_eq!(id_at(&w.path("docs/f.txt")).as_deref(), Some("F"), "placed again");
    assert!(w.graph.with(|c| c.item("F").is_some_and(|f| f.name == "f.txt") && c.bin.is_empty()));
}

/// (b) The same, with the scans after each swap: one while the item is away
/// from the disk (skipped, and taken off), one once it is back. Reached by
/// scanning between cycles.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b_an_item_skipped_and_back_with_a_scan_after_each_swap_is_never_deleted() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    w.graph.with(|c| c.rename("F", "D", &long_name()));
    w.cycle(&listing).await;
    w.examine_handed().await;
    w.cycle(&listing).await;
    assert!(!w.path("docs/f.txt").exists(), "taken off the disk");
    w.scan_and_upload().await;
    w.nothing_deleted_or_moved("while it is away").await;

    w.graph.with(|c| c.rename("F", "D", "f.txt"));
    w.cycle(&listing).await;
    w.scan_and_upload().await;
    w.nothing_deleted_or_moved("after its return's swap").await;
    w.cycle(&listing).await;
    assert_eq!(id_at(&w.path("docs/f.txt")).as_deref(), Some("F"), "placed again");
    assert!(w.graph.with(|c| c.item("F").is_some() && c.bin.is_empty()));
}

/// (c) The same, with the scan once the item is back on disk: a new
/// placeholder, a new inode, which is the one the base records. Reached by
/// letting the cycles place it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c_an_item_back_on_disk_under_a_new_inode_is_never_deleted() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    let old = handle_of(&w.path("docs/f.txt"));
    w.graph.with(|c| c.rename("F", "D", &long_name()));
    w.cycle(&listing).await;
    w.examine_handed().await;
    w.cycle(&listing).await;
    w.graph.with(|c| c.rename("F", "D", "f.txt"));
    w.cycle(&listing).await;
    w.cycle(&listing).await;
    assert_eq!(id_at(&w.path("docs/f.txt")).as_deref(), Some("F"), "back on disk");
    let new = handle_of(&w.path("docs/f.txt"));
    assert_ne!(new, old, "a new inode");
    assert_eq!(w.recorded_handle("F").await, Some(new), "the base records the new one");
    w.scan_and_upload().await;
    w.nothing_deleted_or_moved("back under a new inode").await;
    assert!(w.graph.with(|c| c.item("F").is_some() && c.bin.is_empty()));
}

/// (d) `docs` is removed in OneDrive while it holds a download open in a
/// program, an ignored `*.tmp` and a symlink: all of it goes in the cycle,
/// and a Full local scan after it deletes nothing. Reached by holding the
/// file open across the cycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d_a_folder_removed_in_onedrive_with_an_open_file_an_ignored_name_and_a_symlink_goes_whole() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    std::fs::write(w.path("docs/scratch.tmp"), b"tmp").unwrap();
    std::os::unix::fs::symlink("../top.txt", w.path("docs/link")).unwrap();
    let open = std::fs::File::open(w.path("docs/f.txt")).unwrap();
    w.graph.with(|c| c.trash("D"));
    w.cycle(&listing).await;
    assert!(!w.path("docs").exists(), "removed whole, in the cycle");
    assert!(w.path("top.txt").exists(), "the symlink's target is not under the folder");
    assert_eq!(std::fs::read(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&open))).unwrap(), b"one", "the program keeps what it had open");
    w.scan_and_upload().await;
    w.nothing_deleted_or_moved("after the cycle").await;
    assert!(w.base("D").is_none() && w.base("F").is_none());
    assert_eq!(w.graph.with(|c| c.paths()).len(), 1, "nothing made again in OneDrive: {:?}", w.graph.with(|c| c.paths()));
    drop(open);
}

/// (e) `docs` is removed in OneDrive while a file in it is being downloaded:
/// the download is stopped and the folder goes in the cycle. Reached by a
/// real fill from the fake OneDrive, held up there, run under the file's
/// inode lock as the daemon's fills are (`sync::unless_removed`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e_a_folder_removed_in_onedrive_with_a_file_being_downloaded_goes_whole() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    w.graph.with(|c| c.delay("GET", "dl/F", std::time::Duration::from_secs(30), 1));
    let file = std::fs::OpenOptions::new().read(true).write(true).open(w.path("docs/f.txt")).unwrap();
    let key = crate::folder::locks::InodeKey::of(&file).unwrap();
    let fill = {
        let (locks, source) = (w.locks.clone(), crate::hydration::graph_source::GraphSource::new(w.graph.client()));
        tokio::spawn(async move {
            let guard = locks.lock(key).await;
            crate::folder::locks::unless_removed(Some(&guard), crate::hydration::source::hydrate_with(file.into(), &source, None)).await.map(|r| r.is_ok())
        })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while w.graph.with(|c| c.count("GET", "dl/F")) == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(placeholder::read_state(&std::fs::File::open(w.path("docs/f.txt")).unwrap()).unwrap(), Some(State::Hydrating));
    w.graph.with(|c| c.trash("D"));
    w.cycle(&listing).await;
    assert!(!w.path("docs").exists(), "removed whole, in the cycle");
    let stopped = tokio::time::timeout(std::time::Duration::from_secs(10), fill).await.expect("the download was stopped").unwrap();
    assert_eq!(stopped, None, "stopped, not finished");
    w.scan_and_upload().await;
    w.nothing_deleted_or_moved("after the cycle").await;
}

/// Decision 3: a new file written in `docs` whose batch was not handed over
/// yet (within the watcher's quiet spell), and a download changed there, when
/// `docs` gets a name too long in OneDrive: the folder stays, is examined,
/// both go up into the item in OneDrive under its new name, and only then is
/// the folder removed. It is listed in `Skipped()` with its reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_file_in_a_folder_that_stops_being_placed_reaches_onedrive_first() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(w.path("docs/f.txt"), b"one, changed").unwrap();
    std::fs::write(w.path("docs/new.txt"), b"new").unwrap();
    let long = long_name();
    w.graph.with(|c| c.rename("D", ROOT, &long));
    w.cycle(&listing).await;
    assert!(w.path("docs/new.txt").exists(), "stays until what is in it is uploaded");
    let skipped = w.store.call(|s| s.skipped()).await.unwrap();
    assert!(skipped.iter().any(|(_, reason)| *reason == konedrive_tree::SkipReason::NameTooLong), "{skipped:?}");
    w.examine_handed().await;
    w.cycle(&listing).await;
    assert!(w.path("docs").exists(), "its rows still wait");
    w.upload().await;
    assert!(w.graph.with(|c| c.items.values().any(|i| i.name == "new.txt" && i.parent.as_deref() == Some("D"))), "{:?}", w.graph.with(|c| c.paths()));
    assert_eq!(w.graph.with(|c| c.item("F").unwrap().content.clone()), b"one, changed");
    w.cycle(&listing).await;
    assert!(!w.path("docs").exists(), "removed once nothing in it waits");
    w.scan_and_upload().await;
    w.nothing_deleted_or_moved("after it left").await;
    assert!(w.graph.with(|c| c.item("D").is_some_and(|d| d.name == long) && c.bin.is_empty()));
}

/// Decision 3: a row the outbox cannot finish — a blocked `create`: a new
/// file whose name OneDrive refuses — keeps a folder that stopped being
/// placed on disk as long as it stays; renamed, the file goes up into the
/// item, and the folder goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_row_keeps_a_folder_that_stopped_being_placed() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    std::fs::write(w.path("docs/n:ew.txt"), b"new").unwrap();
    let mut batch = crate::local::Batch::new();
    batch.name(Path::new("docs"), std::ffi::OsStr::new("n:ew.txt"));
    w.examine(batch).await;
    let rows = w.store.call(|s| s.outbox_rows()).await.unwrap();
    assert!(rows.iter().any(|r| r.state == konedrive_tree::outbox::OutboxState::Blocked), "{rows:?}");
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    for _ in 0..3 {
        w.cycle(&listing).await;
        w.examine_handed().await;
        w.upload().await;
    }
    assert!(w.path("docs/n:ew.txt").exists(), "kept while its row is blocked");
    std::fs::rename(w.path("docs/n:ew.txt"), w.path("docs/new.txt")).unwrap();
    for _ in 0..3 {
        w.cycle(&listing).await;
        w.upload().await;
    }
    assert!(w.graph.with(|c| c.items.values().any(|i| i.name == "new.txt" && i.parent.as_deref() == Some("D"))));
    assert!(!w.path("docs").exists(), "gone once nothing in it waits");
    assert_eq!(w.deletes(), 0);
}

/// Review fix 3: `docs` stops being placed with a changed file in it, and is
/// placed again elsewhere (renamed back to a short name in another folder)
/// before the old object goes. The change reaches the item; nothing is
/// created in OneDrive; the old object is removed and the new place holds the
/// item.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_leaving_folder_placed_again_elsewhere_uploads_into_its_item_and_goes() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    w.graph.with(|c| {
        c.add(crate::upload::fake::FakeItem {
            id: "P".into(),
            parent: Some(ROOT.into()),
            name: "papers".into(),
            folder: true,
            content: Vec::new(),
            hash: None,
            size: 0,
            etag: "e-P".into(),
            ctag: "c-P".into(),
            mtime: 0,
        })
    });
    w.cycle(&listing).await;
    let items = w.graph.with(|c| c.items.len());
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    // A blocked row keeps the old object while the folder moves on.
    std::fs::write(w.path("docs/n:ew.txt"), b"new").unwrap();
    let mut batch = crate::local::Batch::new();
    batch.name(Path::new("docs"), std::ffi::OsStr::new("n:ew.txt"));
    w.examine(batch).await;
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    w.cycle(&listing).await;
    w.examine_handed().await;
    w.graph.with(|c| c.rename("D", "P", "docs"));
    w.cycle(&listing).await;
    assert_eq!(id_at(&w.path("papers/docs")).as_deref(), Some("D"), "placed again elsewhere");
    assert_eq!(id_at(&w.path("docs")).as_deref(), Some("D"), "the old object is not stripped");
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(w.path("docs/f.txt"), b"one, changed").unwrap();
    std::fs::remove_file(w.path("docs/n:ew.txt")).unwrap();
    w.scan_and_upload().await;
    w.cycle(&listing).await;
    w.upload().await;
    w.cycle(&listing).await;
    assert_eq!(w.graph.with(|c| c.item("F").unwrap().content.clone()), b"one, changed", "the change reached the item");
    assert_eq!(w.graph.with(|c| c.items.len()), items, "nothing created in OneDrive: {:?}", w.graph.with(|c| c.paths()));
    assert!(!w.path("docs").exists(), "the old object is removed");
    assert_eq!(id_at(&w.path("papers/docs/f.txt")).as_deref(), Some("F"));
    assert!(w.store.call(|s| s.leaving()).await.unwrap().is_empty());
    w.scan_and_upload().await;
    w.nothing_deleted_or_moved("after").await;
}

/// Review fix 4: a downloaded file of another account, moved into `docs`
/// within the watcher's quiet spell (its stamp unchanged, its id unknown
/// here), and `docs` then stops being placed: the file reaches OneDrive in
/// this account before the folder goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_moved_in_from_another_account_reaches_onedrive_before_its_folder_goes() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    std::fs::write(w.path("docs/theirs.txt"), b"theirs").unwrap();
    let file = std::fs::File::open(w.path("docs/theirs.txt")).unwrap();
    placeholder::write_item_id(&file, "OTHER-ACCOUNT-ITEM").unwrap();
    placeholder::write_state(&file, State::Hydrated).unwrap();
    placeholder::write_stamp(&file).unwrap();
    drop(file);
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    w.cycle(&listing).await;
    w.cycle(&listing).await;
    w.upload().await;
    w.cycle(&listing).await;
    assert!(w.graph.with(|c| c.items.values().any(|i| i.name == "theirs.txt" && i.parent.as_deref() == Some("D") && i.content == b"theirs")), "{:?}", w.graph.with(|c| c.paths()));
    assert!(!w.path("docs").exists(), "gone once it is up");
}

/// Review fix 5: what keeps a leaving folder on disk is shown, never silent.
/// A `move` row from before for an item in it is dropped, and does not keep
/// it; a file whose state cannot be read keeps it, listed as not uploaded
/// with its reason, until it is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_keeps_a_leaving_folder_is_shown_and_a_move_from_before_does_not() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    std::fs::rename(w.path("docs/f.txt"), w.path("docs/g.txt")).unwrap();
    let mut batch = crate::local::Batch::new();
    batch.name(Path::new("docs"), std::ffi::OsStr::new("f.txt"));
    batch.name(Path::new("docs"), std::ffi::OsStr::new("g.txt"));
    w.examine(batch).await;
    assert!(w.store.call(|s| s.outbox_rows()).await.unwrap().iter().any(|r| r.kind == konedrive_tree::outbox::OutboxKind::Move));
    w.graph.with(|c| c.add_file("U", "D", "u.txt", b"u"));
    w.cycle(&listing).await;
    xattr::set(w.path("docs/u.txt"), placeholder::XATTR_STATE, b"garbage").unwrap();
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    w.cycle(&listing).await;
    w.cycle(&listing).await;
    w.upload().await;
    assert!(w.store.call(|s| s.outbox_rows()).await.unwrap().is_empty(), "the move was dropped");
    assert_eq!(w.graph.with(|c| c.count("PATCH", "items/")), 0);
    assert!(w.path("docs/u.txt").exists(), "kept for the file whose state cannot be read");
    let skipped = w.store.call(|s| s.local_skipped()).await.unwrap();
    assert!(skipped.iter().any(|k| k.rel == Path::new("docs/u.txt") && k.reason == crate::local::examine::UNKNOWN_STATE), "{skipped:?}");
    std::fs::remove_file(w.path("docs/u.txt")).unwrap();
    w.cycle(&listing).await;
    assert!(!w.path("docs").exists(), "gone once its cause is");
    w.nothing_deleted_or_moved("after").await;
}

/// Review fix 5: another filesystem mounted inside a leaving folder — a
/// Btrfs subvolume, which an unprivileged test can make — keeps it, listed
/// with its reason, until it is gone. Skipped where the test's folder is not
/// on Btrfs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_filesystem_mounted_inside_keeps_a_leaving_folder_and_says_so() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp-btrfs");
    std::fs::create_dir_all(&base).unwrap();
    let w = Arc::new(super::tests::world_in(Some(&base)).await);
    let listing = w.listed().await;
    let made = std::process::Command::new("btrfs").arg("subvolume").arg("create").arg(w.path("docs/sub")).output();
    if !made.is_ok_and(|o| o.status.success()) {
        eprintln!("no Btrfs subvolume can be made here: skipped");
        return;
    }
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    w.cycle(&listing).await;
    w.cycle(&listing).await;
    assert!(w.path("docs/sub").exists(), "kept while something is mounted inside");
    let skipped = w.store.call(|s| s.local_skipped()).await.unwrap();
    assert!(skipped.iter().any(|k| k.rel == Path::new("docs/sub") && k.reason == crate::local::examine::MOUNTED_INSIDE), "{skipped:?}");
    std::fs::remove_dir(w.path("docs/sub")).unwrap();
    w.cycle(&listing).await;
    assert!(!w.path("docs").exists(), "gone once nothing is mounted inside");
    w.nothing_deleted_or_moved("after").await;
}

/// The item id at `path`, if anything is there.
fn id_at(path: &Path) -> Option<String> {
    if std::fs::symlink_metadata(path).is_err() {
        return None;
    }
    super::tests::id_at(path)
}

/// Review fix 1: `docs/f.txt` is changed here, and before the change is
/// handed to the outbox OneDrive renames it to a name over 255 bytes. Its
/// content reaches the item; no `PATCH` with a name or a parent is sent, and
/// the item keeps OneDrive's name. Then the same with the row recorded first,
/// against the old name (the worker meets a `412`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_to_an_item_no_longer_placed_never_renames_it_in_onedrive() {
    for recorded_first in [false, true] {
        let w = Arc::new(world().await);
        let listing = w.listed().await;
        write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(w.path("docs/f.txt"), b"one, changed").unwrap();
        if recorded_first {
            let mut batch = crate::local::Batch::new();
            batch.written(Path::new("docs"), std::ffi::OsStr::new("f.txt"), None);
            assert_eq!(w.examine(batch).await.applied.queued.len(), 1);
        }
        let long = long_name();
        w.graph.with(|c| c.rename("F", "D", &long));
        w.cycle(&listing).await;
        w.examine_handed().await;
        w.upload().await;
        let patches: Vec<String> = w.graph.with(|c| c.log.iter().filter(|(m, _)| m == "PATCH").map(|(_, p)| p.clone()).collect());
        assert!(patches.is_empty(), "recorded_first={recorded_first}: {patches:?}");
        w.graph.with(|c| {
            let f = c.item("F").unwrap();
            assert_eq!(f.content, b"one, changed", "recorded_first={recorded_first}");
            assert_eq!(f.name, long, "recorded_first={recorded_first}: OneDrive's name stands");
        });
        assert_eq!(w.recorded_handle("F").await, None, "no local object recorded for an item not placed");
        w.cycle(&listing).await;
        assert!(!w.path("docs/f.txt").exists(), "recorded_first={recorded_first}: removed once uploaded");
        w.nothing_deleted_or_moved("after").await;
    }
}

/// Review fix 2: a store as a build before #104 left it — `docs` skipped
/// (a name too long) and taken off the disk, while it and `f.txt` below it
/// kept the local objects recorded before — gives no `DELETE` once `docs` is
/// placed again, and its children come back on disk. Reached by putting the
/// old handles back by hand after `docs` left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_left_with_stale_objects_below_a_skipped_folder_deletes_nothing_once_it_is_back() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    let (docs, f) = (handle_of(&w.path("docs")), handle_of(&w.path("docs/f.txt")));
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    w.cycle(&listing).await;
    w.examine_handed().await;
    w.cycle(&listing).await;
    assert!(!w.path("docs").exists(), "left");
    w.store.call(move |s| {
        s.set_local_handle("D", Some(&docs))?;
        s.set_local_handle("F", Some(&f))
    }).await.unwrap();

    w.graph.with(|c| c.rename("D", ROOT, "docs"));
    w.cycle(&listing).await;
    w.scan_and_upload().await;
    w.cycle(&listing).await;
    w.scan_and_upload().await;
    w.nothing_deleted_or_moved("once docs is back").await;
    assert_eq!(id_at(&w.path("docs/f.txt")).as_deref(), Some("F"), "its child is back on disk");
    assert!(w.graph.with(|c| c.item("F").is_some() && c.bin.is_empty()));
}

impl World {
    /// `docs` gets a name too long in OneDrive while a new file whose name
    /// OneDrive refuses (a blocked `create`) keeps it on disk.
    async fn docs_leaving_and_held(&self, listing: &Arc<Listing>) {
        std::fs::write(self.path("docs/n:ew.txt"), b"new").unwrap();
        let mut batch = crate::local::Batch::new();
        batch.name(Path::new("docs"), std::ffi::OsStr::new("n:ew.txt"));
        self.examine(batch).await;
        self.graph.with(|c| c.rename("D", ROOT, &long_name()));
        self.cycle(listing).await;
        self.examine_handed().await;
        self.cycle(listing).await;
        assert!(self.path("docs").exists(), "held by its blocked row");
    }
}

/// Round 2, point 1: `docs` leaves, held on disk by a blocked row, and is
/// placed again elsewhere; the user deletes `f.txt` in its new place. The
/// `DELETE` reaches OneDrive: only rows inside the old object are dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_in_the_new_place_of_a_folder_still_leaving_reaches_onedrive() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    w.graph.with(|c| c.add(crate::upload::fake::FakeItem {
        id: "P".into(),
        parent: Some(ROOT.into()),
        name: "papers".into(),
        folder: true,
        content: Vec::new(),
        hash: None,
        size: 0,
        etag: "e-P".into(),
        ctag: "c-P".into(),
        mtime: 0,
    }));
    w.cycle(&listing).await;
    w.docs_leaving_and_held(&listing).await;
    w.graph.with(|c| c.rename("D", "P", "docs"));
    w.cycle(&listing).await;
    assert_eq!(id_at(&w.path("papers/docs/f.txt")).as_deref(), Some("F"));
    std::fs::remove_file(w.path("papers/docs/f.txt")).unwrap();
    {
        let w = Arc::clone(&w);
        tokio::task::spawn_blocking(move || scan_now(&w)).await.unwrap();
    }
    assert!(w.store.call(|s| s.outbox_rows()).await.unwrap().iter().any(|r| r.kind == konedrive_tree::outbox::OutboxKind::Delete));
    w.cycle(&listing).await;
    w.upload().await;
    assert_eq!(w.deletes(), 1, "the user's delete reached OneDrive");
    assert!(w.graph.with(|c| c.bin.contains_key("F")));
    assert!(w.path("docs").exists(), "the old object is still held");
}

/// Round 2, point 1: `f.txt` moved by the user out of `docs` before `docs`
/// stops being placed stays where the user put it, and its move reaches
/// OneDrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_moved_out_before_its_folder_stops_being_placed_keeps_its_move() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    std::fs::rename(w.path("docs/f.txt"), w.path("f2.txt")).unwrap();
    let mut batch = crate::local::Batch::new();
    batch.name(Path::new("docs"), std::ffi::OsStr::new("f.txt"));
    batch.name(Path::new(""), std::ffi::OsStr::new("f2.txt"));
    w.examine(batch).await;
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    for _ in 0..3 {
        w.cycle(&listing).await;
        w.examine_handed().await;
        w.upload().await;
    }
    assert_eq!(id_at(&w.path("f2.txt")).as_deref(), Some("F"), "where the user put it");
    w.graph.with(|c| {
        let f = c.item("F").unwrap();
        assert_eq!((f.parent.as_deref(), f.name.as_str()), (Some(ROOT), "f2.txt"), "its move reached OneDrive");
    });
    assert!(!w.path("docs").exists());
    assert_eq!(w.deletes(), 0);
}

/// Round 2, point 2: `f.txt`, changed here, is removed in OneDrive while
/// `docs` leaves and a blocked row holds it on disk. Nothing of it is sent —
/// no `create`, no upload — and it leaves the disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_removed_in_onedrive_inside_a_leaving_folder_is_never_uploaded_again() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(w.path("docs/f.txt"), b"one, changed").unwrap();
    w.docs_leaving_and_held(&listing).await;
    w.graph.with(|c| c.trash("F"));
    let uploads = || w.graph.with(|c| c.log.iter().filter(|(m, p)| (m == "PUT" || m == "POST") && !p.contains("/D/children")).count());
    let before = uploads();
    for _ in 0..3 {
        w.cycle(&listing).await;
        w.examine_handed().await;
        w.upload().await;
    }
    assert!(!w.path("docs/f.txt").exists(), "it left the disk");
    assert!(w.path("docs").exists(), "the folder is still held");
    assert_eq!(uploads(), before, "nothing of it was sent: {:?}", w.graph.with(|c| c.log.clone()));
    assert!(w.graph.with(|c| c.items.values().all(|i| i.name != "f.txt")));
}

fn folder_item(id: &str, parent: &str, name: &str) -> crate::upload::fake::FakeItem {
    crate::upload::fake::FakeItem {
        id: id.into(),
        parent: Some(parent.into()),
        name: name.into(),
        folder: true,
        content: Vec::new(),
        hash: None,
        size: 0,
        etag: format!("e-{id}"),
        ctag: format!("c-{id}"),
        mtime: 0,
    }
}

impl World {
    /// `PATCH` requests for item `id`: a rename or a move of it.
    fn patches_of(&self, id: &str) -> usize {
        self.graph.with(|c| c.log.iter().filter(|(m, p)| m == "PATCH" && p.ends_with(&format!("items/{id}"))).count())
    }

    /// A new file whose name OneDrive refuses in `dir`: a blocked `create`
    /// that keeps a leaving folder on disk.
    async fn blocked_file_in(&self, dir: &str) {
        std::fs::write(self.path(&format!("{dir}/n:ew.txt")), b"new").unwrap();
        let mut batch = crate::local::Batch::new();
        batch.name(Path::new(dir), std::ffi::OsStr::new("n:ew.txt"));
        self.examine(batch).await;
    }

    async fn rounds(&self, listing: &Arc<Listing>, n: usize) {
        for _ in 0..n {
            self.cycle(listing).await;
            self.examine_handed().await;
            self.upload().await;
        }
    }
}

/// Third review, point 1: `papers/docs` leaves (a name over 255 bytes in
/// OneDrive) and a blocked row holds it on disk; then `papers` is renamed in
/// OneDrive to `archive`, and the reconcile moves it with `docs` inside.
/// The leaving object is followed by its id: no `PATCH` of `docs` (its long
/// name in OneDrive stands), and it goes once nothing in it waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_leaving_folder_whose_parent_is_renamed_in_onedrive_is_never_moved_back() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    w.graph.with(|c| {
        c.add(folder_item("P", ROOT, "papers"));
        c.rename("D", "P", "docs");
    });
    w.rounds(&listing, 1).await;
    assert_eq!(id_at(&w.path("papers/docs")).as_deref(), Some("D"));
    w.blocked_file_in("papers/docs").await;
    let long = long_name();
    w.graph.with(|c| c.rename("D", "P", &long));
    w.rounds(&listing, 2).await;
    assert!(w.path("papers/docs").exists(), "held by its blocked row");
    w.graph.with(|c| c.rename("P", ROOT, "archive"));
    w.rounds(&listing, 3).await;
    assert!(w.path("archive/docs/n:ew.txt").exists(), "moved along with its parent, still held");
    assert_eq!(w.patches_of("D"), 0, "docs was moved or renamed in OneDrive");
    w.scan_and_upload().await;
    w.rounds(&listing, 1).await;
    assert_eq!(w.patches_of("D"), 0, "after a Full scan too");
    assert!(w.graph.with(|c| c.item("D").unwrap().name == long));
    std::fs::rename(w.path("archive/docs/n:ew.txt"), w.path("archive/docs/new.txt")).unwrap();
    w.scan_and_upload().await;
    w.rounds(&listing, 3).await;
    assert!(w.graph.with(|c| c.items.values().any(|i| i.name == "new.txt" && i.parent.as_deref() == Some("D"))), "{:?}", w.graph.with(|c| c.paths()));
    assert!(!w.path("archive/docs").exists(), "gone once nothing in it waits");
    assert_eq!(w.patches_of("D"), 0);
    assert_eq!(w.deletes(), 0);
}

/// Third review, point 1, the variant: `docs` is moved in OneDrive into a
/// folder this folder does not place (a name over 255 bytes): it is never
/// moved back, and goes from the disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_moved_in_onedrive_into_a_skipped_folder_is_never_moved_back() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    w.graph.with(|c| {
        c.add(folder_item("S", ROOT, &long_name()));
        c.rename("D", "S", "docs");
    });
    w.rounds(&listing, 3).await;
    w.scan_and_upload().await;
    w.rounds(&listing, 1).await;
    assert_eq!(w.patches_of("D"), 0);
    assert_eq!(w.graph.with(|c| c.item("D").unwrap().parent.clone()).as_deref(), Some("S"));
    assert!(!w.path("docs").exists());
    assert_eq!(w.deletes(), 0);
}

/// Third review, point 2: a placed file the user moves into a folder that is
/// leaving: its move is carried out — it ends inside that folder's item in
/// OneDrive — with no `DELETE`, and it is not placed again where it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_placed_file_moved_into_a_leaving_folder_keeps_its_move() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    w.blocked_file_in("docs").await;
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    w.rounds(&listing, 2).await;
    assert!(w.path("docs").exists());
    std::fs::rename(w.path("top.txt"), w.path("docs/top.txt")).unwrap();
    let mut batch = crate::local::Batch::new();
    batch.name(Path::new(""), std::ffi::OsStr::new("top.txt"));
    batch.name(Path::new("docs"), std::ffi::OsStr::new("top.txt"));
    w.examine(batch).await;
    w.rounds(&listing, 3).await;
    w.graph.with(|c| {
        let t = c.item("T").unwrap();
        assert_eq!((t.parent.as_deref(), t.name.as_str()), (Some("D"), "top.txt"), "moved into the folder's item");
    });
    assert_eq!(w.patches_of("D"), 0, "the leaving item itself is never moved");
    assert_eq!(w.deletes(), 0);
    assert!(!w.path("top.txt").exists(), "not placed again where it was");
}

/// Third review, point 3: `resyncChangesUploadDifferences` does not mean
/// removed: a changed file inside a leaving folder that the new listing
/// leaves out is kept and goes up again as new, as anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resync_upload_differences_keeps_local_changes_inside_a_leaving_folder() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(w.path("docs/f.txt"), b"one, changed").unwrap();
    w.blocked_file_in("docs").await;
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    w.cycle(&listing).await;
    w.graph.with(|c| {
        c.trash("F");
        c.script(
            "GET",
            "root/delta",
            wiremock::ResponseTemplate::new(410).set_body_json(serde_json::json!({"error": {"code": "resyncRequired", "innerError": {"code": "resyncChangesUploadDifferences"}}})),
            1,
        );
    });
    w.cycle(&listing).await;
    w.rounds(&listing, 2).await;
    assert_eq!(std::fs::read(w.path("docs/f.txt")).unwrap(), b"one, changed", "kept");
    assert!(w.graph.with(|c| c.items.values().any(|i| i.name == "f.txt" && i.parent.as_deref() == Some("D") && i.content == b"one, changed")), "{:?}", w.graph.with(|c| c.paths()));
}

/// Third review, point 4: a change inside a leaving folder answered `404`
/// while OneDrive's listing still has the item is kept, blocked with a
/// reason the user sees — not dropped; once the listing says the item is
/// gone, the row goes and so does the file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_404_not_confirmed_by_the_listing_keeps_the_change_blocked() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    w.blocked_file_in("docs").await;
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    w.rounds(&listing, 2).await;
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(w.path("docs/f.txt"), b"one, changed").unwrap();
    let mut batch = crate::local::Batch::new();
    batch.written(Path::new("docs"), std::ffi::OsStr::new("f.txt"), None);
    w.examine(batch).await;
    w.graph.with(|c| c.script("POST", "items/F/createUploadSession", wiremock::ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": {"code": "itemNotFound"}})), 1));
    w.upload().await;
    let rows = w.store.call(|s| s.outbox_rows()).await.unwrap();
    let row = rows.iter().find(|r| r.item_id.as_deref() == Some("F")).unwrap_or_else(|| panic!("the change is kept: {rows:?} {:?}", w.graph.with(|c| c.log.clone())));
    assert_eq!((row.state, row.reason.as_deref()), (konedrive_tree::outbox::OutboxState::Blocked, Some(crate::upload::reason::LEAVING_NOT_FOUND)));
    assert!(w.path("docs/f.txt").exists());
    w.graph.with(|c| c.trash("F"));
    w.rounds(&listing, 2).await;
    assert!(w.store.call(|s| s.outbox_rows()).await.unwrap().iter().all(|r| r.item_id.as_deref() != Some("F")), "dropped once the listing says it is gone");
    assert!(!w.path("docs/f.txt").exists());
    assert!(w.graph.with(|c| c.items.values().all(|i| i.name != "f.txt")), "never uploaded as new");
}

/// Leaving by handle: `papers/docs` leaves, held on disk by a blocked row;
/// the user renames `papers` here to `archive`, and a cycle runs before any
/// examination sees the rename. The leaving object is found by its file
/// handle and its path followed: no `PATCH` of `docs`, and it still goes
/// once nothing in it waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_leaving_folder_whose_parent_is_renamed_here_is_found_by_its_handle() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    w.graph.with(|c| {
        c.add(folder_item("P", ROOT, "papers"));
        c.rename("D", "P", "docs");
    });
    w.rounds(&listing, 1).await;
    w.blocked_file_in("papers/docs").await;
    let long = long_name();
    w.graph.with(|c| c.rename("D", "P", &long));
    w.rounds(&listing, 2).await;
    assert!(w.path("papers/docs").exists());
    std::fs::rename(w.path("papers"), w.path("archive")).unwrap();
    w.cycle(&listing).await;
    let leaving = w.store.call(|s| s.leaving()).await.unwrap();
    assert_eq!(leaving, vec![("D".to_owned(), PathBuf::from("archive/docs"))], "found by its handle, its path followed");
    w.scan_and_upload().await;
    w.rounds(&listing, 2).await;
    assert_eq!(w.patches_of("D"), 0, "the leaving item is never moved or renamed");
    assert!(w.graph.with(|c| c.item("D").unwrap().name == long));
    std::fs::rename(w.path("archive/docs/n:ew.txt"), w.path("archive/docs/new.txt")).unwrap();
    w.scan_and_upload().await;
    w.rounds(&listing, 3).await;
    assert!(!w.path("archive/docs").exists(), "gone once nothing in it waits");
    assert_eq!(w.patches_of("D"), 0);
    assert_eq!(w.deletes(), 0);
}

/// Leaving by handle, (b): an error other than "gone" — the parent of a
/// leaving folder made unreadable — keeps the leaving row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_error_other_than_gone_keeps_the_leaving_row() {
    use std::os::unix::fs::PermissionsExt;
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    w.graph.with(|c| {
        c.add(folder_item("P", ROOT, "papers"));
        c.rename("D", "P", "docs");
    });
    w.rounds(&listing, 1).await;
    w.blocked_file_in("papers/docs").await;
    w.graph.with(|c| c.rename("D", "P", &long_name()));
    w.rounds(&listing, 2).await;
    std::fs::set_permissions(w.path("papers"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let cycled = listing.cycle(&tokio_util::sync::CancellationToken::new()).await;
    std::fs::set_permissions(w.path("papers"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = cycled;
    let leaving = w.store.call(|s| s.leaving()).await.unwrap();
    assert_eq!(leaving, vec![("D".to_owned(), PathBuf::from("papers/docs"))], "kept");
    w.rounds(&listing, 1).await;
    assert!(w.path("papers/docs").exists());
    assert_eq!(w.patches_of("D"), 0);
}

/// A change blocked as `leaving-not-found` is not stuck: once OneDrive's
/// listing brings its item again, it is retried and goes up into the item.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_blocked_by_a_404_is_retried_when_onedrive_lists_its_item_again() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    w.blocked_file_in("docs").await;
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    w.rounds(&listing, 2).await;
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(w.path("docs/f.txt"), b"one, changed").unwrap();
    let mut batch = crate::local::Batch::new();
    batch.written(Path::new("docs"), std::ffi::OsStr::new("f.txt"), None);
    w.examine(batch).await;
    w.graph.with(|c| c.script("POST", "items/F/createUploadSession", wiremock::ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": {"code": "itemNotFound"}})), 1));
    w.upload().await;
    let blocked = |w: &World| {
        let w = w.store.clone();
        async move { w.call(|s| s.outbox_rows()).await.unwrap().into_iter().any(|r| r.reason.as_deref() == Some(crate::upload::reason::LEAVING_NOT_FOUND)) }
    };
    assert!(blocked(&w).await, "blocked by the 404");
    w.graph.with(|c| c.touch("F"));
    w.cycle(&listing).await;
    assert!(!blocked(&w).await, "retried once OneDrive lists it again");
    w.upload().await;
    assert_eq!(w.graph.with(|c| c.item("F").unwrap().content.clone()), b"one, changed");
}

impl World {
    fn posts_of_children(&self) -> usize {
        self.graph.with(|c| c.log.iter().filter(|(m, p)| m == "POST" && p.ends_with("/children")).count())
    }

    /// `docs` leaves, held on disk by a blocked row, and is placed again in
    /// OneDrive at `papers/docs`.
    async fn docs_leaving_and_placed_again_in_papers(&self, listing: &Arc<Listing>) {
        self.graph.with(|c| c.add(folder_item("P", ROOT, "papers")));
        self.rounds(listing, 1).await;
        self.blocked_file_in("docs").await;
        self.graph.with(|c| c.rename("D", ROOT, &long_name()));
        self.rounds(listing, 2).await;
        self.graph.with(|c| c.rename("D", "P", "docs"));
        self.rounds(listing, 2).await;
        assert_eq!(id_at(&self.path("papers/docs")).as_deref(), Some("D"), "placed again");
        assert!(self.path("docs").exists(), "the old object is still held");
    }
}

/// Fourth review, point 1: the copy placed again is never taken for the
/// leaving object by its id. The user renames `papers/docs` (placed again)
/// to `papers/docs2`: one `PATCH` (the user's rename), no `DELETE`, nothing
/// created, and `papers/docs2/f.txt` stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renaming_the_copy_placed_again_is_the_users_rename_only() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    w.docs_leaving_and_placed_again_in_papers(&listing).await;
    let patches = w.patches_of("D");
    std::fs::rename(w.path("papers/docs"), w.path("papers/docs2")).unwrap();
    w.scan_and_upload().await;
    w.rounds(&listing, 2).await;
    w.scan_and_upload().await;
    assert_eq!(w.patches_of("D") - patches, 1, "the user's rename only");
    assert_eq!(w.deletes(), 0);
    assert_eq!(w.posts_of_children(), 0, "nothing created in OneDrive");
    assert!(w.path("papers/docs2/f.txt").exists());
    assert_eq!(w.graph.with(|c| c.item("D").unwrap().name.clone()), "docs2");
}

/// Fourth review, point 2: the copy placed again, renamed in OneDrive to
/// `docs3`, is followed there by a Full reconcile (a `410`); the leaving
/// object stays where it is, and nothing is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_reconcile_moves_the_copy_placed_again_not_the_leaving_object() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    w.docs_leaving_and_placed_again_in_papers(&listing).await;
    let writes = |w: &World| w.graph.with(|c| c.log.iter().filter(|(m, _)| m != "GET").count());
    let before = writes(&w);
    w.graph.with(|c| {
        c.rename("D", "P", "docs3");
        c.script(
            "GET",
            "root/delta",
            wiremock::ResponseTemplate::new(410).set_body_json(serde_json::json!({"error": {"code": "resyncRequired", "innerError": {"code": "resyncChangesApplyDifferences"}}})),
            1,
        );
    });
    let report = w.cycle(&listing).await;
    assert!(report.full);
    w.rounds(&listing, 1).await;
    assert_eq!(id_at(&w.path("papers/docs3")).as_deref(), Some("D"), "the copy followed to docs3");
    assert!(!w.path("papers/docs").exists());
    assert_eq!(w.store.call(|s| s.leaving()).await.unwrap(), vec![("D".to_owned(), PathBuf::from("docs"))], "the leaving object stays where it is");
    assert_eq!(writes(&w), before, "nothing sent: {:?}", w.graph.with(|c| c.log.clone()));
}

/// Fourth review, point 3: a leaving object whose path is gone, behind a
/// directory that cannot be read: nothing is decided, and its row stays;
/// readable again, it is found by its handle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_directory_on_the_way_keeps_the_leaving_row() {
    use std::os::unix::fs::PermissionsExt;
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    w.graph.with(|c| {
        c.add(folder_item("P", ROOT, "papers"));
        c.rename("D", "P", "docs");
    });
    w.rounds(&listing, 1).await;
    w.blocked_file_in("papers/docs").await;
    w.graph.with(|c| c.rename("D", "P", &long_name()));
    w.rounds(&listing, 2).await;
    std::fs::create_dir(w.path("lock")).unwrap();
    std::fs::rename(w.path("papers"), w.path("lock/papers")).unwrap();
    std::fs::set_permissions(w.path("lock"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let _ = listing.cycle(&tokio_util::sync::CancellationToken::new()).await;
    std::fs::set_permissions(w.path("lock"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(w.store.call(|s| s.leaving()).await.unwrap(), vec![("D".to_owned(), PathBuf::from("papers/docs"))], "kept");
    w.cycle(&listing).await;
    assert_eq!(w.store.call(|s| s.leaving()).await.unwrap(), vec![("D".to_owned(), PathBuf::from("lock/papers/docs"))], "found by its handle");
    assert_eq!(w.patches_of("D"), 0);
}

/// Fourth review, point 4: a placed file the user moved into a leaving
/// folder (moved in OneDrive too), then changed: its upload is answered
/// `404` and blocked; then OneDrive removes it. The blocked row goes, the
/// file leaves the disk, nothing is uploaded as new, and the folder is not
/// held by it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_404_row_goes_once_the_listing_removes_its_item() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    w.blocked_file_in("docs").await;
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    w.rounds(&listing, 2).await;
    write_version(&w.path("top.txt"), b"top", &w.cloud_ctag("T"));
    std::fs::rename(w.path("top.txt"), w.path("docs/top.txt")).unwrap();
    let mut batch = crate::local::Batch::new();
    batch.name(Path::new(""), std::ffi::OsStr::new("top.txt"));
    batch.name(Path::new("docs"), std::ffi::OsStr::new("top.txt"));
    w.examine(batch).await;
    w.rounds(&listing, 2).await;
    assert_eq!(w.graph.with(|c| c.item("T").unwrap().parent.clone()).as_deref(), Some("D"));
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(w.path("docs/top.txt"), b"top, changed").unwrap();
    let mut batch = crate::local::Batch::new();
    batch.written(Path::new("docs"), std::ffi::OsStr::new("top.txt"), None);
    w.examine(batch).await;
    w.graph.with(|c| c.script("POST", "items/T/createUploadSession", wiremock::ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": {"code": "itemNotFound"}})), 1));
    w.upload().await;
    let reasons = || async { w.store.call(|s| s.outbox_rows()).await.unwrap().into_iter().filter_map(|r| r.reason).collect::<Vec<_>>() };
    assert!(reasons().await.iter().any(|r| r == crate::upload::reason::LEAVING_NOT_FOUND), "{:?}", reasons().await);
    w.graph.with(|c| c.trash("T"));
    w.rounds(&listing, 2).await;
    assert!(!reasons().await.iter().any(|r| r == crate::upload::reason::LEAVING_NOT_FOUND), "the blocked row went");
    assert!(!w.path("docs/top.txt").exists(), "removed here as OneDrive removed it");
    assert!(w.graph.with(|c| c.items.values().all(|i| i.name != "top.txt")), "never uploaded as new");
}

impl World {
    /// `docs/f.txt`, downloaded and changed here inside a leaving `docs`
    /// (held by a blocked row), its upload answered `404` while OneDrive's
    /// listing still has it: blocked as `leaving-not-found`.
    async fn f_blocked_by_a_404_in_leaving_docs(&self, listing: &Arc<Listing>) {
        write_version(&self.path("docs/f.txt"), b"one", &self.cloud_ctag("F"));
        self.blocked_file_in("docs").await;
        self.graph.with(|c| c.rename("D", ROOT, &long_name()));
        self.rounds(listing, 2).await;
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(self.path("docs/f.txt"), b"one, changed").unwrap();
        let mut batch = crate::local::Batch::new();
        batch.written(Path::new("docs"), std::ffi::OsStr::new("f.txt"), None);
        self.examine(batch).await;
        self.graph.with(|c| c.script("POST", "items/F/createUploadSession", wiremock::ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": {"code": "itemNotFound"}})), 1));
        self.upload().await;
        assert!(self.not_found_rows().await > 0, "blocked by the 404");
    }

    async fn not_found_rows(&self) -> usize {
        self.store.call(|s| s.outbox_rows()).await.unwrap().into_iter().filter(|r| r.reason.as_deref() == Some(crate::upload::reason::LEAVING_NOT_FOUND)).count()
    }
}

/// Fifth review, point 1: a `leaving-not-found` row whose file the user
/// removed meanwhile — nothing on disk for the reconcile to remove — goes
/// once OneDrive's listing says its item is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_404_row_without_its_file_goes_once_the_listing_removes_its_item() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    w.f_blocked_by_a_404_in_leaving_docs(&listing).await;
    std::fs::remove_file(w.path("docs/f.txt")).unwrap();
    w.graph.with(|c| c.trash("F"));
    w.cycle(&listing).await;
    assert_eq!(w.not_found_rows().await, 0, "the blocked row went");
}

/// Fifth review, point 2: a large delta, reconciled Full, is no whole
/// listing of the drive: a `leaving-not-found` row stays blocked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_delta_is_no_whole_listing_for_blocked_rows() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    w.f_blocked_by_a_404_in_leaving_docs(&listing).await;
    let large = Listing::new(ListingContext { writes: Some(w.writes(None)), full_threshold: 1, ..w.context_parts() });
    w.graph.with(|c| {
        c.add_file("X1", ROOT, "x1.txt", b"1");
        c.add_file("X2", ROOT, "x2.txt", b"2");
    });
    let report = w.cycle(&large).await;
    assert!(report.full, "reconciled Full");
    assert_eq!(w.not_found_rows().await, 1, "still blocked: a large delta does not list F again");
}

/// Fifth review, point 3: `docs` is leaving and its copy is placed again at
/// `papers/docs`; the user removes the old `docs` and moves `papers/docs` to
/// `docs`. The move reaches OneDrive as a `PATCH`; nothing is removed from
/// disk and nothing downloaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moving_the_copy_placed_again_to_where_the_leaving_object_was_is_the_users_move() {
    use std::os::unix::fs::MetadataExt;
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    w.docs_leaving_and_placed_again_in_papers(&listing).await;
    std::fs::remove_dir_all(w.path("docs")).unwrap();
    std::fs::rename(w.path("papers/docs"), w.path("docs")).unwrap();
    let ino = std::fs::metadata(w.path("docs/f.txt")).unwrap().ino();
    let patches = w.patches_of("D");
    w.scan_and_upload().await;
    w.rounds(&listing, 2).await;
    w.scan_and_upload().await;
    w.rounds(&listing, 1).await;
    assert!(w.patches_of("D") > patches, "the user's move reached OneDrive");
    w.graph.with(|c| {
        let d = c.item("D").unwrap();
        assert_eq!((d.parent.as_deref(), d.name.as_str()), (Some(ROOT), "docs"));
    });
    assert_eq!(std::fs::metadata(w.path("docs/f.txt")).unwrap().ino(), ino, "nothing removed from disk");
    assert!(!w.path("papers/docs").exists(), "not placed again where it was");
    assert_eq!(w.graph.with(|c| c.count("GET", "dl/")), 0, "nothing downloaded");
    assert_eq!(w.deletes(), 0);
}

/// Fifth review, point 4: a hard link the user makes to a leaving file
/// carries its handle and its id, but is not taken for it: a Full scan
/// leaves the leaving object where it is recorded, and the link stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hard_link_to_a_leaving_file_is_not_taken_for_it() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    w.graph.with(|c| c.rename("F", "D", &long_name()));
    w.cycle(&listing).await;
    w.examine_handed().await;
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(w.path("docs/f.txt"), b"one, changed").unwrap();
    let mut batch = crate::local::Batch::new();
    batch.written(Path::new("docs"), std::ffi::OsStr::new("f.txt"), None);
    w.examine(batch).await;
    w.graph.with(|c| c.script("POST", "items/F/createUploadSession", wiremock::ResponseTemplate::new(404).set_body_json(serde_json::json!({"error": {"code": "itemNotFound"}})), 1));
    w.upload().await;
    assert_eq!(w.not_found_rows().await, 1, "held by its blocked row");
    assert_eq!(w.store.call(|s| s.leaving()).await.unwrap(), vec![("F".to_owned(), PathBuf::from("docs/f.txt"))], "leaving");
    std::fs::hard_link(w.path("docs/f.txt"), w.path("f-link.txt")).unwrap();
    w.scan_and_upload().await;
    assert_eq!(w.store.call(|s| s.leaving()).await.unwrap(), vec![("F".to_owned(), PathBuf::from("docs/f.txt"))], "after the scan");
    w.cycle(&listing).await;
    assert_eq!(w.store.call(|s| s.leaving()).await.unwrap(), vec![("F".to_owned(), PathBuf::from("docs/f.txt"))], "after the cycle");
    w.scan_and_upload().await;
    assert_eq!(w.store.call(|s| s.leaving()).await.unwrap(), vec![("F".to_owned(), PathBuf::from("docs/f.txt"))], "still where it is recorded");
    assert!(w.path("f-link.txt").exists() && w.path("docs/f.txt").exists(), "both names stay");
    assert_eq!(w.patches_of("F"), 0);
}

/// Copies every `user.konedrive.*` attribute of `from` onto `to`, as
/// `cp --preserve=xattr` or vim with `+xattr` does.
fn copy_konedrive_xattrs(from: &Path, to: &Path) {
    for name in xattr::list(from).unwrap() {
        if name.to_string_lossy().starts_with("user.konedrive.") {
            if let Some(value) = xattr::get(from, &name).unwrap() {
                xattr::set(to, &name, &value).unwrap();
            }
        }
    }
}

/// Sixth review, point 1: `docs/f.txt` is leaving (F renamed in OneDrive to
/// a name over 255 bytes) and an editor saves it by writing a new file that
/// copies its attributes and renaming it over. The new inode at its place is
/// the leaving object: no `PATCH`, the name in OneDrive stays long, the
/// change goes up as content, and the file goes after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_save_by_rename_over_a_leaving_file_uploads_content_only() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    let long = long_name();
    w.graph.with(|c| c.rename("F", "D", &long));
    w.cycle(&listing).await;
    w.examine_handed().await;
    assert_eq!(w.store.call(|s| s.leaving()).await.unwrap(), vec![("F".to_owned(), PathBuf::from("docs/f.txt"))]);
    std::thread::sleep(std::time::Duration::from_millis(10));
    let temp = w.path("docs/.f.txt.swp");
    std::fs::write(&temp, b"one, saved by rename").unwrap();
    copy_konedrive_xattrs(&w.path("docs/f.txt"), &temp);
    std::fs::rename(&temp, w.path("docs/f.txt")).unwrap();
    for _ in 0..3 {
        let mut batch = crate::local::Batch::new();
        batch.name(Path::new("docs"), std::ffi::OsStr::new("f.txt"));
        w.examine(batch).await;
        w.upload().await;
        w.cycle(&listing).await;
    }
    assert_eq!(w.patches_of("F"), 0, "never renamed in OneDrive");
    w.graph.with(|c| {
        let f = c.item("F").unwrap();
        assert_eq!(f.name, long, "its name in OneDrive stays");
        assert_eq!(f.content, b"one, saved by rename", "the change went up as content");
    });
    assert!(!w.path("docs/f.txt").exists(), "and it went from the disk after");
}

/// Sixth review, point 2: a hard link the user made to a leaving file
/// outlives it. When the daemon takes the leaving name off the disk, the
/// inode loses its item id first: the link is the user's own file, uploaded
/// as new — never the item, renamed in OneDrive to the link's name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hard_link_left_by_a_leaving_file_is_the_users_own_file() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    let long = long_name();
    w.graph.with(|c| c.rename("F", "D", &long));
    w.cycle(&listing).await;
    w.examine_handed().await;
    std::fs::hard_link(w.path("docs/f.txt"), w.path("f-link.txt")).unwrap();
    w.rounds(&listing, 2).await;
    assert!(!w.path("docs/f.txt").exists(), "the leaving name went");
    w.scan_and_upload().await;
    w.rounds(&listing, 1).await;
    assert_eq!(w.patches_of("F"), 0, "never renamed in OneDrive");
    assert_eq!(w.graph.with(|c| c.item("F").unwrap().name.clone()), long);
    assert!(w.graph.with(|c| c.items.values().any(|i| i.name == "f-link.txt" && i.id != "F" && i.content == b"one")), "uploaded as new: {:?}", w.graph.with(|c| c.paths()));
    assert_eq!(std::fs::read(w.path("f-link.txt")).unwrap(), b"one");
}

/// Sixth review, point 3: a hard link the user makes to the copy placed
/// again of a leaving file — the same item id, another inode — is a hard
/// link of that copy, not of the leaving object: the copy is still the item,
/// and its change goes up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hard_link_to_the_copy_placed_again_leaves_the_copy_the_item() {
    use std::io::Write;
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    w.graph.with(|c| c.rename("F", "D", &long_name()));
    w.cycle(&listing).await;
    w.examine_handed().await;
    // What keeps the leaving file: a state that cannot be read.
    xattr::set(w.path("docs/f.txt"), placeholder::XATTR_STATE, b"garbage").unwrap();
    w.rounds(&listing, 2).await;
    assert!(w.path("docs/f.txt").exists(), "held");
    w.graph.with(|c| c.rename("F", "D", "g.txt"));
    w.rounds(&listing, 2).await;
    assert_eq!(id_at(&w.path("docs/g.txt")).as_deref(), Some("F"), "placed again");
    write_version(&w.path("docs/g.txt"), b"one", &w.cloud_ctag("F"));
    std::fs::hard_link(w.path("docs/g.txt"), w.path("h.txt")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::OpenOptions::new().append(true).open(w.path("docs/g.txt")).unwrap().write_all(b", changed").unwrap();
    w.scan_and_upload().await;
    assert_eq!(w.graph.with(|c| c.item("F").unwrap().content.clone()), b"one, changed", "the copy's change went up");
    assert_eq!(w.patches_of("F"), 0, "never renamed or moved by the daemon");
}
