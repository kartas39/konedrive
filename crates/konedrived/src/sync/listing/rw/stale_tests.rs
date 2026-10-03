//! What the daemon takes off the disk itself is never deleted or moved in
//! OneDrive (issue #104): an item removed in OneDrive, and one that stops
//! being placed while it is still there. Against the fake OneDrive, in a
//! read-write folder, with the examination's "where is it now?" answered
//! from where objects really are ([`Scanning`]), so that a stale local
//! object would be proved gone and turned into a `DELETE`.

use std::path::Path;
use std::sync::Arc;

use konedrive_fs::placeholder::{self, State};

use super::super::{Listing, ListingContext};
use super::tests::{now, world, write_version, Scanning, World};
use crate::sync::disk::Disk;
use crate::sync::upload::fake::ROOT;
use crate::sync::local::{Examined, Examiner, IgnoreList};

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
        .map_err(|e| assert!(matches!(e, crate::sync::local::ExamineError::NoBase), "{e}"))
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
            !rows.iter().any(|r| r.kind.removes() || r.kind == crate::tree::outbox::OutboxKind::Move),
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
    let key = crate::sync::InodeKey::of(&file).unwrap();
    let fill = {
        let (locks, source) = (w.locks.clone(), crate::sync::graph_source::GraphSource::new(w.graph.client()));
        tokio::spawn(async move {
            let guard = locks.lock(key).await;
            crate::sync::unless_removed(Some(&guard), crate::sync::source::hydrate_with(file.into(), &source, None)).await.map(|r| r.is_ok())
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
    assert!(skipped.iter().any(|(_, reason)| *reason == crate::tree::SkipReason::NameTooLong), "{skipped:?}");
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

/// Decision 3: a row the outbox cannot finish — blocked: `f.txt` renamed
/// here to a name OneDrive refuses — keeps a folder that stopped being
/// placed on disk as long as it stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_row_keeps_a_folder_that_stopped_being_placed() {
    let w = Arc::new(world().await);
    let listing = w.listed().await;
    std::fs::rename(w.path("docs/f.txt"), w.path("docs/f:txt")).unwrap();
    let mut batch = crate::sync::local::Batch::new();
    batch.name(Path::new("docs"), std::ffi::OsStr::new("f.txt"));
    batch.name(Path::new("docs"), std::ffi::OsStr::new("f:txt"));
    w.examine(batch).await;
    let rows = w.store.call(|s| s.outbox_rows()).await.unwrap();
    assert!(rows.iter().any(|r| r.state == crate::tree::outbox::OutboxState::Blocked), "{rows:?}");
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    for _ in 0..3 {
        w.cycle(&listing).await;
        w.examine_handed().await;
        w.upload().await;
    }
    assert!(w.path("docs/f:txt").exists(), "kept while its row is blocked");
    assert_eq!(w.deletes(), 0);
}

/// The item id at `path`, if anything is there.
fn id_at(path: &Path) -> Option<String> {
    if std::fs::symlink_metadata(path).is_err() {
        return None;
    }
    super::tests::id_at(path)
}
