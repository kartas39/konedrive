//! What the daemon takes off the disk itself is never deleted or moved in
//! OneDrive: an item removed in OneDrive, and one OneDrive
//! still has and the folder can no longer hold, which goes only when
//! nothing in it waits and is an item like any other until then. Against
//! the fake OneDrive, in a
//! read-write folder, with the examination's "where is it now?" answered
//! from where objects really are ([`Scanning`]), so that a stale local
//! object would be proved gone and turned into a `DELETE`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_fs::placeholder::{self, State};
use konedrive_tree::WaitsFor;

use crate::remote::listing::Listing;
use super::Scanning;
use crate::remote::testing::{now, write_version, Says, World};
use crate::folder::disk::Disk;
use crate::fake_onedrive::ROOT;
use crate::local::{Examined, IgnoreList};

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
    crate::local::testing::examine(&disk, &store, &liveness, &IgnoreList::default(), &locks, now(), &crate::local::Batch::full())
        .map_err(|e| assert!(matches!(e, crate::local::ExamineError::NoBase), "{e}"))
        .ok()
}

impl World {
    /// What OneDrive says now, reconciled with a Full local scan between the
    /// folder and the swap: the reconcile's two steps, one after the other.
    async fn reconcile_scanning_before_swap(&self) {
        let mut step = self.step(Says::Fetched).await;
        step.apply().await.unwrap();
        konedrive_tree::off_runtime(|| scan_now(self));
        step.commit().await.unwrap();
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
/// has it skipped. Reached by running the reconcile's two steps with the
/// scan between them: in the daemon the watcher holds the tree lock while it
/// examines, so this window is closed there too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_an_item_skipped_and_back_with_a_scan_before_each_swap_is_never_deleted() {
    let w = Arc::new(World::read_write().await);
    w.listed().await;
    assert_eq!(id_at(&w.path("docs/f.txt")).as_deref(), Some("F"));

    w.graph.with(|c| c.rename("F", "D", &long_name()));
    w.reconcile_scanning_before_swap().await;
    w.examine_handed().await;
    w.scan_and_upload().await;
    w.nothing_deleted_or_moved("skipped").await;
    w.reconcile_scanning_before_swap().await;
    w.scan_and_upload().await;
    w.nothing_deleted_or_moved("skipped, a cycle later").await;
    assert!(!w.path("docs/f.txt").exists(), "a skipped item is not on disk");

    w.graph.with(|c| c.rename("F", "D", "f.txt"));
    w.reconcile_scanning_before_swap().await;
    w.reconcile_scanning_before_swap().await;
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
    let w = Arc::new(World::read_write().await);
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
    let w = Arc::new(World::read_write().await);
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
/// program, an empty ignored `*.tmp` and a symlink: all of it goes in the cycle,
/// and a Full local scan after it deletes nothing. Reached by holding the
/// file open across the cycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d_a_folder_removed_in_onedrive_with_an_open_file_an_ignored_name_and_a_symlink_goes_whole() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    std::fs::write(w.path("docs/scratch.tmp"), b"").unwrap();
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
    let w = Arc::new(World::read_write().await);
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

/// The item id at `path`, if anything is there.
fn id_at(path: &Path) -> Option<String> {
    if std::fs::symlink_metadata(path).is_err() {
        return None;
    }
    super::id_at(path)
}

fn folder_item(id: &str, parent: &str, name: &str) -> crate::fake_onedrive::FakeItem {
    crate::fake_onedrive::FakeItem {
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

    /// A new file whose name OneDrive refuses in `dir`: a blocked `create`,
    /// which keeps a folder that can no longer be placed on disk.
    async fn blocked_file_in(&self, dir: &str) {
        std::fs::write(self.path(&format!("{dir}/n:ew.txt")), b"new").unwrap();
        let mut batch = crate::local::Batch::new();
        batch.name(Path::new(dir), std::ffi::OsStr::new("n:ew.txt"));
        self.examine(batch).await;
    }

    /// Cycles, each followed by what the watcher and the outbox worker do
    /// after it.
    async fn rounds(&self, listing: &Arc<Listing>, n: usize) {
        for _ in 0..n {
            self.cycle(listing).await;
            self.examine_handed().await;
            self.upload().await;
        }
    }

    /// What the items that are still here and cannot stay wait for, as
    /// `Skipped()` says it.
    async fn waits(&self) -> Vec<WaitsFor> {
        self.store.call(|s| s.skipped()).await.unwrap().into_iter().filter_map(|line| line.waits).collect()
    }

    /// `docs` gets a name too long in OneDrive while a blocked row keeps it
    /// on disk.
    async fn docs_waiting(&self, listing: &Arc<Listing>) -> String {
        let long = long_name();
        self.blocked_file_in("docs").await;
        self.graph.with(|c| c.rename("D", ROOT, &long));
        self.rounds(listing, 2).await;
        assert!(self.path("docs").exists(), "kept by its blocked row");
        assert_eq!(self.waits().await, [WaitsFor::Uploads(1)]);
        long
    }
}

/// `docs` gets a name too long in OneDrive and nothing in it waits: it
/// leaves the disk whole in the cycle that learns of it, and is listed as
/// not in the folder, with nothing said to keep it. Nothing is deleted or
/// moved in OneDrive, by that cycle or by a scan after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_can_no_longer_be_placed_and_holds_nothing_goes_in_that_cycle() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    let long = long_name();
    w.graph.with(|c| c.rename("D", ROOT, &long));
    w.cycle(&listing).await;
    assert!(!w.path("docs").exists(), "gone in the cycle");
    let skipped = w.store.call(|s| s.skipped()).await.unwrap();
    assert_eq!(skipped, [konedrive_tree::Skipped { rel: PathBuf::from(&long), reason: konedrive_tree::SkipReason::NameTooLong, waits: None, here: None }]);
    assert_eq!(w.recorded_handle("D").await, None);
    w.scan_and_upload().await;
    w.nothing_deleted_or_moved("after it left").await;
    assert!(w.graph.with(|c| c.item("F").is_some() && c.bin.is_empty()));
}

/// A new file written in `docs` whose batch was not handed over yet (within
/// the watcher's quiet spell), and a download changed there, when `docs`
/// gets a name too long in OneDrive: nothing of the folder is touched, it is
/// listed with what it waits for, and the place is handed to the watcher.
/// Both files go up into the item in OneDrive, under its new name, and only
/// then does the folder go. No name and no folder is sent for anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_file_and_a_changed_one_reach_the_item_before_their_folder_goes() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(w.path("docs/f.txt"), b"one, changed").unwrap();
    std::fs::write(w.path("docs/new.txt"), b"new").unwrap();
    let long = long_name();
    w.graph.with(|c| c.rename("D", ROOT, &long));
    w.cycle(&listing).await;
    assert!(w.path("docs/new.txt").exists() && w.path("docs/f.txt").exists(), "nothing goes while anything waits");
    let waits = w.waits().await;
    assert!(matches!(waits.as_slice(), [WaitsFor::Changes(at)] if at.starts_with("docs/")), "{waits:?}");
    assert_eq!(w.base("D").map(|row| row.name).as_deref(), Some("docs"), "still an item of the folder");
    assert!(w.recorded_handle("D").await.is_some());
    w.examine_handed().await;
    w.cycle(&listing).await;
    assert!(w.path("docs").exists(), "its rows still wait");
    assert_eq!(w.waits().await, [WaitsFor::Uploads(2)]);
    w.upload().await;
    assert!(w.graph.with(|c| c.items.values().any(|i| i.name == "new.txt" && i.parent.as_deref() == Some("D"))), "{:?}", w.graph.with(|c| c.paths()));
    assert_eq!(w.graph.with(|c| c.item("F").unwrap().content.clone()), b"one, changed");
    w.cycle(&listing).await;
    assert!(!w.path("docs").exists(), "removed once nothing in it waits");
    assert!(w.waits().await.is_empty());
    w.scan_and_upload().await;
    w.nothing_deleted_or_moved("after it left").await;
    assert!(w.graph.with(|c| c.item("D").is_some_and(|d| d.name == long) && c.bin.is_empty()));
}

/// `docs/f.txt` is changed here, and OneDrive renames it to a name over 255
/// bytes: before the change is recorded, and then with its row recorded
/// first, against the old name (the worker meets a `412`). Its content
/// reaches the item; no `PATCH` with a name or a folder is sent, and the
/// item keeps OneDrive's name. The file stays the item's object, where it
/// is, until the upload is done, and then goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_to_an_item_no_longer_placed_never_renames_it_in_onedrive() {
    for recorded_first in [false, true] {
        let w = Arc::new(World::read_write().await);
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
        assert!(w.path("docs/f.txt").exists(), "recorded_first={recorded_first}: it waits");
        w.examine_handed().await;
        w.upload().await;
        let patches: Vec<String> = w.graph.with(|c| c.log.iter().filter(|(m, _)| m == "PATCH").map(|(_, p)| p.clone()).collect());
        assert!(patches.is_empty(), "recorded_first={recorded_first}: {patches:?}");
        w.graph.with(|c| {
            let f = c.item("F").unwrap();
            assert_eq!(f.content, b"one, changed", "recorded_first={recorded_first}");
            assert_eq!(f.name, long, "recorded_first={recorded_first}: OneDrive's name stands");
        });
        assert_eq!(w.base("F").map(|row| row.name).as_deref(), Some("f.txt"), "the commit keeps the place the disk has");
        assert_eq!(w.recorded_handle("F").await, Some(handle_of(&w.path("docs/f.txt"))));
        w.cycle(&listing).await;
        assert!(!w.path("docs/f.txt").exists(), "recorded_first={recorded_first}: removed once uploaded");
        w.scan_and_upload().await;
        w.nothing_deleted_or_moved("after").await;
    }
}

/// A downloaded file of another account, moved into `docs` within the
/// watcher's quiet spell (its stamp unchanged, its id unknown here), and
/// `docs` then stops being placed: the file reaches OneDrive in this account
/// before the folder goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_moved_in_from_another_account_reaches_onedrive_before_its_folder_goes() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    std::fs::write(w.path("docs/theirs.txt"), b"theirs").unwrap();
    let file = std::fs::File::open(w.path("docs/theirs.txt")).unwrap();
    placeholder::write_item_id(&file, "OTHER-ACCOUNT-ITEM").unwrap();
    placeholder::write_state(&file, State::Hydrated).unwrap();
    placeholder::write_stamp(&file).unwrap();
    drop(file);
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    w.cycle(&listing).await;
    assert_eq!(w.waits().await, [WaitsFor::Changes("docs/theirs.txt".into())]);
    w.rounds(&listing, 2).await;
    w.cycle(&listing).await;
    assert!(w.graph.with(|c| c.items.values().any(|i| i.name == "theirs.txt" && i.parent.as_deref() == Some("D") && i.content == b"theirs")), "{:?}", w.graph.with(|c| c.paths()));
    assert!(!w.path("docs").exists(), "gone once it is up");
    assert_eq!(w.deletes(), 0);
}

/// A row the outbox cannot finish — a new file whose name OneDrive refuses —
/// keeps a folder that can no longer be placed on disk as long as it stays,
/// and the skipped list says so; renamed, the file goes up into the item,
/// and the folder goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_row_keeps_a_folder_that_can_no_longer_be_placed_and_it_is_shown() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    w.docs_waiting(&listing).await;
    w.rounds(&listing, 1).await;
    assert!(w.path("docs/n:ew.txt").exists() && w.path("docs/f.txt").exists(), "kept while its row is blocked");
    std::fs::rename(w.path("docs/n:ew.txt"), w.path("docs/new.txt")).unwrap();
    w.scan_and_upload().await;
    w.rounds(&listing, 2).await;
    assert!(w.graph.with(|c| c.items.values().any(|i| i.name == "new.txt" && i.parent.as_deref() == Some("D"))));
    assert!(!w.path("docs").exists(), "gone once nothing in it waits");
    assert_eq!((w.deletes(), w.patches_of("D")), (0, 0));
}

/// What cannot be told keeps the folder, and says why: a file of ours whose
/// state cannot be read, and a file under an ignored name that only this
/// computer has. Neither is removed by the daemon. Once the user removes
/// them the folder goes; the delete of the item is the user's, and is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_that_cannot_be_read_and_one_only_here_keep_the_folder_and_say_why() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    w.graph.with(|c| c.add_file("U", "D", "u.txt", b"u"));
    w.cycle(&listing).await;
    xattr::set(w.path("docs/u.txt"), placeholder::XATTR_STATE, b"garbage").unwrap();
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    w.rounds(&listing, 2).await;
    assert!(w.path("docs/u.txt").exists() && w.path("docs/f.txt").exists());
    assert_eq!(w.waits().await, [WaitsFor::UnknownState("docs/u.txt".into())]);
    std::fs::remove_file(w.path("docs/u.txt")).unwrap();
    std::fs::write(w.path("docs/draft.txt~"), b"only here").unwrap();
    w.rounds(&listing, 1).await;
    assert_eq!(w.waits().await, [WaitsFor::Changes("docs/u.txt".into())], "a delete the examination has still to record");
    w.rounds(&listing, 1).await;
    assert!(w.graph.with(|c| c.bin.contains_key("U")), "the user's delete reached OneDrive");
    assert_eq!(w.waits().await, [WaitsFor::LocalOnly("docs/draft.txt~".into())]);
    assert_eq!(std::fs::read(w.path("docs/draft.txt~")).unwrap(), b"only here");
    std::fs::remove_file(w.path("docs/draft.txt~")).unwrap();
    w.cycle(&listing).await;
    assert!(!w.path("docs").exists(), "gone once its cause is");
    assert_eq!((w.deletes(), w.patches_of("D")), (1, 0));
}

/// Another filesystem mounted inside a folder that can no longer be placed —
/// a Btrfs subvolume, which an unprivileged test can make — keeps it, and
/// the skipped list says so, until it is gone. Skipped where the test's
/// folder is not on Btrfs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_filesystem_mounted_inside_keeps_the_folder_and_says_so() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp-btrfs");
    std::fs::create_dir_all(&base).unwrap();
    let w = Arc::new(super::World::read_write_in(Some(&base)).await);
    let listing = w.listed().await;
    let made = std::process::Command::new("btrfs").arg("subvolume").arg("create").arg(w.path("docs/sub")).output();
    if !made.is_ok_and(|o| o.status.success()) {
        println!("NOT RUN: no Btrfs subvolume can be made here, so a mount inside the folder is not tested");
        return;
    }
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    w.rounds(&listing, 2).await;
    assert!(w.path("docs/sub").exists() && w.path("docs/f.txt").exists(), "kept while something is mounted inside");
    assert_eq!(w.waits().await, [WaitsFor::MountedInside("docs/sub".into())]);
    std::fs::remove_dir(w.path("docs/sub")).unwrap();
    w.cycle(&listing).await;
    assert!(!w.path("docs").exists(), "gone once nothing is mounted inside");
    w.nothing_deleted_or_moved("after").await;
}

/// What the user does inside a folder that waits is the user's own act, and
/// reaches OneDrive: a delete, a rename, a folder made with a file in it,
/// and a placed file moved in. The folder's own name in OneDrive stands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delete_a_rename_and_a_move_made_inside_a_waiting_folder_reach_onedrive() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    w.graph.with(|c| c.add_file("G", "D", "g.txt", b"g"));
    w.cycle(&listing).await;
    let long = w.docs_waiting(&listing).await;
    std::fs::remove_file(w.path("docs/f.txt")).unwrap();
    std::fs::rename(w.path("docs/g.txt"), w.path("docs/h.txt")).unwrap();
    std::fs::create_dir(w.path("docs/sub")).unwrap();
    std::fs::write(w.path("docs/sub/new.txt"), b"new").unwrap();
    std::fs::rename(w.path("top.txt"), w.path("docs/top.txt")).unwrap();
    w.scan_and_upload().await;
    w.rounds(&listing, 2).await;
    w.graph.with(|c| {
        assert!(c.bin.contains_key("F"), "the delete");
        let g = c.item("G").unwrap();
        assert_eq!((g.parent.as_deref(), g.name.as_str()), (Some("D"), "h.txt"), "the rename");
        let t = c.item("T").unwrap();
        assert_eq!((t.parent.as_deref(), t.name.as_str()), (Some("D"), "top.txt"), "the move in");
        let sub = c.items.values().find(|i| i.name == "sub" && i.parent.as_deref() == Some("D")).unwrap_or_else(|| panic!("{:?}", c.paths()));
        assert!(c.items.values().any(|i| i.name == "new.txt" && i.parent.as_deref() == Some(sub.id.as_str())), "{:?}", c.paths());
        assert_eq!(c.item("D").unwrap().name, long);
    });
    assert_eq!((w.deletes(), w.patches_of("D")), (1, 0));
    assert!(w.path("docs/h.txt").exists() && !w.path("top.txt").exists(), "still waiting, and nothing placed again where it was");
}

/// Placed again where it is while it waits (its name short again in
/// OneDrive): nothing is removed and nothing is sent, and it is off the
/// skipped list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn placed_again_where_it_is_while_it_waits_nothing_is_removed_or_sent() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    let docs = handle_of(&w.path("docs"));
    w.docs_waiting(&listing).await;
    let sent = w.graph.with(|c| c.log.iter().filter(|(m, _)| m != "GET").count());
    w.graph.with(|c| c.rename("D", ROOT, "docs"));
    w.rounds(&listing, 2).await;
    assert_eq!(id_at(&w.path("docs/f.txt")).as_deref(), Some("F"));
    assert_eq!(w.recorded_handle("D").await, Some(docs), "the same object all along");
    assert!(w.store.call(|s| s.skipped()).await.unwrap().is_empty());
    assert_eq!(w.deferred("D"), None);
    assert_eq!(w.graph.with(|c| c.log.iter().filter(|(m, _)| m != "GET").count()), sent, "nothing sent");
}

/// Placed again elsewhere while it waits (moved in OneDrive into another
/// folder, under a name the folder can hold): the one object is moved there
/// with what waits in it, nothing is made anew here or in OneDrive, and what
/// waited goes up where the folder is now.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn placed_again_elsewhere_while_it_waits_is_one_object_moved() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    w.graph.with(|c| c.add(folder_item("P", ROOT, "papers")));
    w.cycle(&listing).await;
    let docs = handle_of(&w.path("docs"));
    w.docs_waiting(&listing).await;
    let items = w.graph.with(|c| c.items.len());
    w.graph.with(|c| c.rename("D", "P", "docs"));
    w.rounds(&listing, 2).await;
    assert!(!w.path("docs").exists(), "one object");
    assert_eq!(handle_of(&w.path("papers/docs")), docs, "moved, not made again");
    assert_eq!(id_at(&w.path("papers/docs/f.txt")).as_deref(), Some("F"));
    assert!(w.path("papers/docs/n:ew.txt").exists(), "what waits in it went along");
    assert_eq!(w.graph.with(|c| c.items.len()), items, "nothing created in OneDrive: {:?}", w.graph.with(|c| c.paths()));
    assert!(w.store.call(|s| s.skipped()).await.unwrap().is_empty());
    std::fs::rename(w.path("papers/docs/n:ew.txt"), w.path("papers/docs/new.txt")).unwrap();
    w.scan_and_upload().await;
    assert!(w.graph.with(|c| c.items.values().any(|i| i.name == "new.txt" && i.parent.as_deref() == Some("D"))), "{:?}", w.graph.with(|c| c.paths()));
    assert_eq!((w.deletes(), w.patches_of("D")), (0, 0));
}

/// A parent renamed while the folder below it waits — in OneDrive, where the
/// reconcile moves it with the folder inside, and then here, which is the
/// user's rename and is sent: the waiting folder is carried as any item is.
/// It is never renamed or moved in OneDrive, and goes once nothing in it
/// waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parent_renamed_in_onedrive_and_one_renamed_here_carry_what_waits_below_them() {
    let w = Arc::new(World::read_write().await);
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
    assert!(w.path("papers/docs").exists(), "kept by its blocked row");
    w.graph.with(|c| c.rename("P", ROOT, "archive"));
    // The row that waits in it is told its new place by the examination
    // the cycle after the move hands over.
    w.rounds(&listing, 3).await;
    assert!(w.path("archive/docs/n:ew.txt").exists(), "moved along with its parent, still waiting");
    assert_eq!(w.waits().await, [WaitsFor::Uploads(1)]);
    std::fs::rename(w.path("archive"), w.path("shelf")).unwrap();
    w.cycle(&listing).await;
    assert!(w.path("shelf/docs/f.txt").exists(), "a cycle before the examination leaves it where it is");
    w.scan_and_upload().await;
    w.rounds(&listing, 2).await;
    assert_eq!(w.graph.with(|c| c.item("P").unwrap().name.clone()), "shelf", "the user's rename is sent");
    assert_eq!(w.patches_of("D"), 0);
    std::fs::rename(w.path("shelf/docs/n:ew.txt"), w.path("shelf/docs/new.txt")).unwrap();
    w.scan_and_upload().await;
    w.rounds(&listing, 2).await;
    assert!(w.graph.with(|c| c.items.values().any(|i| i.name == "new.txt" && i.parent.as_deref() == Some("D"))), "{:?}", w.graph.with(|c| c.paths()));
    assert!(!w.path("shelf/docs").exists() && w.path("shelf").exists(), "gone once nothing in it waits");
    assert_eq!(w.graph.with(|c| c.item("D").unwrap().name.clone()), long);
    assert_eq!((w.deletes(), w.patches_of("D")), (0, 0));
}

/// `docs` is moved in OneDrive into a folder this folder does not place: it
/// is never moved back, and goes from the disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_moved_in_onedrive_into_a_folder_that_is_not_placed_is_never_moved_back() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    w.graph.with(|c| {
        c.add(folder_item("S", ROOT, &long_name()));
        c.rename("D", "S", "docs");
    });
    w.rounds(&listing, 2).await;
    w.scan_and_upload().await;
    w.rounds(&listing, 1).await;
    assert_eq!(w.patches_of("D"), 0);
    assert_eq!(w.graph.with(|c| c.item("D").unwrap().parent.clone()).as_deref(), Some("S"));
    assert!(!w.path("docs").exists());
    assert_eq!(w.deletes(), 0);
}

/// An item removed in OneDrive inside a folder that waits goes as anything
/// removed there does: a file not changed here leaves the disk in the
/// cycle, and one changed here stays and goes up as new, into the folder's
/// item. The folder waits on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_item_removed_in_onedrive_inside_a_waiting_folder_goes() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    w.graph.with(|c| c.add_file("G", "D", "g.txt", b"g"));
    w.cycle(&listing).await;
    write_version(&w.path("docs/g.txt"), b"g", &w.cloud_ctag("G"));
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(w.path("docs/g.txt"), b"g, changed").unwrap();
    w.docs_waiting(&listing).await;
    w.graph.with(|c| {
        c.trash("F");
        c.trash("G");
    });
    w.rounds(&listing, 2).await;
    assert!(!w.path("docs/f.txt").exists(), "what OneDrive had left the disk");
    assert_eq!(std::fs::read(w.path("docs/g.txt")).unwrap(), b"g, changed", "what it never had stays");
    assert!(w.graph.with(|c| c.items.values().any(|i| i.name == "g.txt" && i.id != "G" && i.parent.as_deref() == Some("D") && i.content == b"g, changed")), "{:?}", w.graph.with(|c| c.paths()));
    assert!(w.path("docs/n:ew.txt").exists(), "the folder still waits");
    assert_eq!((w.deletes(), w.patches_of("D")), (0, 0));
}

/// A file changed in a folder that waits, which OneDrive answers `404` for
/// (removed there, and no cycle has said so yet), is uploaded again as new,
/// as in any folder: into the folder's item.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_404_inside_a_waiting_folder_uploads_the_file_as_new() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    w.docs_waiting(&listing).await;
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(w.path("docs/f.txt"), b"one, changed").unwrap();
    let mut batch = crate::local::Batch::new();
    batch.written(Path::new("docs"), std::ffi::OsStr::new("f.txt"), None);
    w.examine(batch).await;
    w.graph.with(|c| c.trash("F"));
    w.upload().await;
    w.rounds(&listing, 2).await;
    assert!(w.graph.with(|c| c.items.values().any(|i| i.name == "f.txt" && i.id != "F" && i.parent.as_deref() == Some("D") && i.content == b"one, changed")), "{:?}", w.graph.with(|c| c.paths()));
    let rows = w.store.call(|s| s.outbox_rows()).await.unwrap();
    assert!(rows.iter().all(|row| row.state == konedrive_tree::outbox::OutboxState::Blocked && row.rel == Path::new("docs/n:ew.txt")), "{rows:?}");
    assert_eq!(w.deletes(), 0);
}

/// A file that can no longer be placed has a second name, a hard link the
/// user made: the daemon takes its own name off the disk, and the other one
/// is the user's own file, uploaded as new — never the item, renamed in
/// OneDrive to the link's name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_name_of_a_file_that_is_taken_off_is_the_users_own_file() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    std::fs::hard_link(w.path("docs/f.txt"), w.path("f-link.txt")).unwrap();
    let long = long_name();
    w.graph.with(|c| c.rename("F", "D", &long));
    w.rounds(&listing, 2).await;
    assert!(!w.path("docs/f.txt").exists(), "its own name went");
    w.scan_and_upload().await;
    w.rounds(&listing, 1).await;
    assert_eq!(w.patches_of("F"), 0, "never renamed in OneDrive");
    assert_eq!(w.graph.with(|c| c.item("F").unwrap().name.clone()), long);
    assert!(w.graph.with(|c| c.items.values().any(|i| i.name == "f-link.txt" && i.id != "F" && i.content == b"one")), "uploaded as new: {:?}", w.graph.with(|c| c.paths()));
    assert_eq!(std::fs::read(w.path("f-link.txt")).unwrap(), b"one");
    assert_eq!(w.deletes(), 0);
}

/// `f.txt` moved by the user out of `docs` before `docs` stops being placed
/// stays where the user put it, and its move reaches OneDrive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_moved_out_before_its_folder_stops_being_placed_keeps_its_move() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    std::fs::rename(w.path("docs/f.txt"), w.path("f2.txt")).unwrap();
    let mut batch = crate::local::Batch::new();
    batch.name(Path::new("docs"), std::ffi::OsStr::new("f.txt"));
    batch.name(Path::new(""), std::ffi::OsStr::new("f2.txt"));
    w.examine(batch).await;
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    w.rounds(&listing, 3).await;
    assert_eq!(id_at(&w.path("f2.txt")).as_deref(), Some("F"), "where the user put it");
    w.graph.with(|c| {
        let f = c.item("F").unwrap();
        assert_eq!((f.parent.as_deref(), f.name.as_str()), (Some(ROOT), "f2.txt"), "its move reached OneDrive");
    });
    assert!(!w.path("docs").exists());
    assert_eq!(w.deletes(), 0);
}

/// A store as a build before #104 left it — `docs` skipped (a name too
/// long) and off the disk, while it and `f.txt` below it kept the local
/// objects recorded before — gives no `DELETE` once `docs` is placed again,
/// and its children come back on disk. Reached by putting the old handles
/// back by hand after `docs` left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_left_with_stale_objects_below_a_skipped_folder_deletes_nothing_once_it_is_back() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    let (docs, f) = (handle_of(&w.path("docs")), handle_of(&w.path("docs/f.txt")));
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
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

/// `resyncChangesUploadDifferences` does not mean removed: a changed file
/// inside a folder that waits, which the new listing leaves out, is kept
/// and goes up again as new, as anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resync_upload_differences_keeps_local_changes_inside_a_waiting_folder() {
    let w = Arc::new(World::read_write().await);
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

/// A store a build at schema 7 left in the middle of a leave: `docs` not
/// placed by the base (a name too long in OneDrive), its object kept on
/// disk by a row of `leaving`, a changed file's content row and a new
/// file's row waiting inside it. Opened by this build, the folder is an
/// item of the base again where it stands, and waits: both files go up
/// into the item, once, nothing is deleted, renamed or moved in OneDrive,
/// and the folder then leaves the disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_of_version_7_with_a_leaving_folder_opens_and_nothing_is_deleted_or_uploaded_twice() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(w.path("docs/f.txt"), b"one, changed").unwrap();
    std::fs::write(w.path("docs/new.txt"), b"new").unwrap();
    let mut batch = crate::local::Batch::new();
    batch.tree(Path::new("docs"));
    assert_eq!(w.examine(batch).await.applied.queued.len(), 2);
    // What version 7 held once `docs` began to leave.
    let long = long_name();
    w.graph.with(|c| c.rename("D", ROOT, &long));
    let hex: String = handle_of(&w.path("docs")).encode().iter().map(|byte| format!("{byte:02X}")).collect();
    let version_7 = format!(
        "ALTER TABLE deferred DROP COLUMN waits;
         CREATE TABLE leaving (id TEXT PRIMARY KEY, rel BLOB NOT NULL, handle BLOB);
         CREATE TABLE leaving_items (id TEXT PRIMARY KEY, leaving TEXT NOT NULL);
         UPDATE items SET name = '{long}', placement = 'skipped:name-too-long', local_handle = NULL WHERE id = 'D';
         UPDATE items SET local_handle = NULL WHERE id = 'F';
         INSERT INTO leaving (id, rel, handle) VALUES ('D', CAST('docs' AS BLOB), X'{hex}');
         INSERT INTO leaving_items (id, leaving) VALUES ('D', 'D'), ('F', 'D');
         UPDATE meta SET value = '7' WHERE key = 'schema_version';"
    );
    w.store.call(move |s| {
        s.bench_sql(&version_7)?;
        s.upgrade_in_place()
    }).await.unwrap();
    assert_eq!(w.base("D").map(|row| (row.name, row.placement)), Some(("docs".to_owned(), konedrive_tree::Placement::Placed)));
    assert_eq!(w.waits().await, [WaitsFor::Cycle]);

    let items = w.graph.with(|c| c.items.len());
    w.scan_and_upload().await;
    w.rounds(&listing, 3).await;
    w.scan_and_upload().await;
    assert!(!w.path("docs").exists(), "gone once what waited in it is up");
    w.graph.with(|c| {
        assert_eq!(c.item("F").unwrap().content, b"one, changed");
        assert_eq!(c.items.values().filter(|i| i.name == "new.txt").map(|i| i.parent.clone()).collect::<Vec<_>>(), [Some("D".to_owned())], "once, in the item");
        assert_eq!(c.items.len(), items + 1, "{:?}", c.paths());
        assert_eq!(c.item("D").unwrap().name, long);
        assert!(c.bin.is_empty());
    });
    assert_eq!(w.graph.with(|c| c.count("PATCH", "items/")), 0);
    assert_eq!(w.deletes(), 0);
}

/// `docs/f.txt` is changed here while OneDrive moves it into a folder this
/// folder does not place (as into the Personal Vault): its content goes
/// into the item where OneDrive has it, it is never moved back, and the
/// file leaves the disk once the upload is done.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_to_an_item_moved_in_onedrive_out_of_what_the_folder_holds_never_moves_it_back() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    w.graph.with(|c| c.add(folder_item("S", ROOT, &long_name())));
    w.cycle(&listing).await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    std::thread::sleep(std::time::Duration::from_millis(10));
    std::fs::write(w.path("docs/f.txt"), b"one, changed").unwrap();
    let mut batch = crate::local::Batch::new();
    batch.written(Path::new("docs"), std::ffi::OsStr::new("f.txt"), None);
    assert_eq!(w.examine(batch).await.applied.queued.len(), 1);
    w.graph.with(|c| c.rename("F", "S", "f.txt"));
    w.upload().await;
    w.graph.with(|c| {
        let f = c.item("F").unwrap();
        assert_eq!((f.parent.as_deref(), f.content.as_slice()), (Some("S"), b"one, changed".as_slice()));
    });
    assert_eq!(w.patches_of("F"), 0);
    assert_eq!(w.base("F").and_then(|row| row.parent_id).as_deref(), Some("D"), "the commit keeps the place the disk has");
    assert!(w.path("docs/f.txt").exists());
    w.rounds(&listing, 2).await;
    assert!(!w.path("docs/f.txt").exists(), "gone once nothing waits");
    w.scan_and_upload().await;
    assert_eq!((w.patches_of("F"), w.deletes()), (0, 0));
}

/// Another filesystem mounted inside a folder OneDrive removed — a Btrfs
/// subvolume — cannot be removed: the folder's removal waits, nothing of the
/// user's in the mount is touched, the cycle goes through, and the rest of
/// the folder syncs. Once it is gone the folder goes. Skipped where the
/// test's folder is not on Btrfs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_filesystem_mounted_inside_a_folder_removed_in_onedrive_makes_its_removal_wait() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp-btrfs");
    std::fs::create_dir_all(&base).unwrap();
    let w = Arc::new(super::World::read_write_in(Some(&base)).await);
    let listing = w.listed().await;
    let made = std::process::Command::new("btrfs").arg("subvolume").arg("create").arg(w.path("docs/sub")).output();
    if !made.is_ok_and(|o| o.status.success()) {
        println!("NOT RUN: no Btrfs subvolume can be made here, so a mount inside the folder is not tested");
        return;
    }
    std::fs::write(w.path("docs/sub/mine.txt"), b"mine").unwrap();
    w.graph.with(|c| {
        c.trash("D");
        c.add_file("N", ROOT, "next.txt", b"next");
    });
    w.rounds(&listing, 2).await;
    assert_eq!(std::fs::read(w.path("docs/sub/mine.txt")).unwrap(), b"mine");
    assert_eq!(id_at(&w.path("docs")).as_deref(), Some("D"), "not touched: still the item's folder");
    assert_eq!(id_at(&w.path("next.txt")).as_deref(), Some("N"), "the rest of the folder syncs");
    assert!(w.base("D").is_some(), "its removal waits");
    assert!(w.graph.with(|c| c.items.values().all(|i| i.name != "docs")), "nothing is made again in OneDrive");
    std::fs::remove_file(w.path("docs/sub/mine.txt")).unwrap();
    std::fs::remove_dir(w.path("docs/sub")).unwrap();
    w.rounds(&listing, 1).await;
    assert!(!w.path("docs").exists() && w.base("D").is_none(), "gone once nothing is mounted inside");
    assert_eq!(w.deletes(), 0);
}

/// A rename of the item made here while OneDrive has it where the folder
/// cannot hold it is the user's act, and is sent — as a rename, and nothing
/// more. Under a name too long in OneDrive, the item gets the user's name.
/// Moved in OneDrive into a folder that is not placed (as into the Personal
/// Vault), it gets the user's name there: it is never moved back out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rename_made_here_of_an_item_that_waits_is_sent_as_a_rename_and_never_moves_it_back() {
    for moved_away in [false, true] {
        let w = Arc::new(World::read_write().await);
        let listing = w.listed().await;
        w.graph.with(|c| c.add(folder_item("S", ROOT, &long_name())));
        w.cycle(&listing).await;
        std::fs::rename(w.path("docs/f.txt"), w.path("docs/g.txt")).unwrap();
        let mut batch = crate::local::Batch::new();
        batch.name(Path::new("docs"), std::ffi::OsStr::new("f.txt"));
        batch.name(Path::new("docs"), std::ffi::OsStr::new("g.txt"));
        assert_eq!(w.examine(batch).await.applied.queued.len(), 1);
        w.graph.with(|c| if moved_away { c.rename("F", "S", "f.txt") } else { c.rename("F", "D", &long_name()) });
        w.upload().await;
        let in_onedrive = |w: &World| w.graph.with(|c| c.item("F").map(|f| (f.parent.clone().unwrap(), f.name.clone())).unwrap());
        assert_eq!(in_onedrive(&w), ((if moved_away { "S" } else { "D" }).to_owned(), "g.txt".to_owned()), "moved_away={moved_away}");
        // The base has the item where its object stands, under the name the
        // user gave it: nothing waits under a path where nothing is.
        assert_eq!(w.base("F").map(|row| (row.parent_id.unwrap(), row.name)), Some(("D".to_owned(), "g.txt".to_owned())), "moved_away={moved_away}");
        assert_eq!(w.recorded_handle("F").await, Some(handle_of(&w.path("docs/g.txt"))));
        w.rounds(&listing, 1).await;
        let sent = w.graph.with(|c| c.log.iter().filter(|(m, _)| m != "GET").count());
        if moved_away {
            assert!(!w.path("docs/g.txt").exists(), "nothing waits in it: it left");
            let waits: Vec<_> = w.store.call(|s| s.skipped()).await.unwrap().into_iter().filter(|line| line.waits.is_some()).collect();
            assert!(waits.is_empty(), "{waits:?}");
        }
        // A scan of the whole folder and more cycles find nothing to send.
        w.scan_and_upload().await;
        w.rounds(&listing, 2).await;
        assert_eq!(w.graph.with(|c| c.log.iter().filter(|(m, _)| m != "GET").count()), sent, "moved_away={moved_away}: {:?}", w.graph.with(|c| c.log.clone()));
        assert_eq!(in_onedrive(&w).1, "g.txt");
        assert_eq!(w.deletes(), 0);
        if !moved_away {
            assert_eq!(id_at(&w.path("docs/g.txt")).as_deref(), Some("F"), "a name the folder can hold: it stays");
            assert!(w.store.call(|s| s.skipped()).await.unwrap().iter().all(|line| line.waits.is_none()));
        }
    }
}

/// The look right before the unlink: a placed file the user moves into the
/// folder after the cycle looked at it and before it is removed is not the
/// folder's to remove. The removal stops, nothing of the file is lost, no
/// `DELETE` is sent for it, and its move reaches OneDrive before the folder
/// goes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_moved_in_between_the_look_and_the_removal_stops_the_removal() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    let (from, to) = (w.path("top.txt"), w.path("docs/top.txt"));
    crate::remote::materialize::before_the_next_removal(&w.root.path, move || std::fs::rename(from, to).unwrap());
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    w.cycle(&listing).await;
    assert_eq!(id_at(&w.path("docs/top.txt")).as_deref(), Some("T"), "not removed");
    assert_eq!(w.waits().await, [WaitsFor::Changes("docs/top.txt".into())]);
    w.scan_and_upload().await;
    w.rounds(&listing, 2).await;
    assert_eq!(w.graph.with(|c| c.item("T").and_then(|t| t.parent.clone())).as_deref(), Some("D"), "the user's move is sent");
    assert_eq!(w.deletes(), 0);
    assert!(!w.path("docs").exists(), "and then the folder goes");
}

/// An editor's save by rename inside a folder that waits is a change of the
/// item, as anywhere: the new content goes into the item, nothing is made
/// anew and nothing is deleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_save_by_rename_inside_a_waiting_folder_is_the_items_new_content() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    w.scan_and_upload().await;
    w.docs_waiting(&listing).await;
    let items = w.graph.with(|c| c.items.len());
    std::fs::write(w.path("docs/f.txt.new"), b"saved by rename").unwrap();
    std::fs::rename(w.path("docs/f.txt.new"), w.path("docs/f.txt")).unwrap();
    w.scan_and_upload().await;
    w.rounds(&listing, 1).await;
    assert_eq!(w.graph.with(|c| c.item("F").unwrap().content.clone()), b"saved by rename");
    assert_eq!((w.graph.with(|c| c.items.len()), w.deletes(), w.patches_of("F")), (items, 0, 0));
}

/// A download a program has open for writing keeps the folder, and the
/// skipped list says so; closed and unchanged, it goes with the folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_open_for_writing_keeps_the_folder_and_says_so() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    write_version(&w.path("docs/f.txt"), b"one", &w.cloud_ctag("F"));
    let open = std::fs::OpenOptions::new().append(true).open(w.path("docs/f.txt")).unwrap();
    w.graph.with(|c| c.rename("D", ROOT, &long_name()));
    w.rounds(&listing, 2).await;
    assert!(w.path("docs/f.txt").exists());
    assert_eq!(w.waits().await, [WaitsFor::OpenForWriting("docs/f.txt".into())]);
    drop(open);
    w.rounds(&listing, 2).await;
    assert!(!w.path("docs").exists(), "gone once it is closed");
    assert_eq!(w.deletes(), 0);
}

/// A reconcile of the whole folder: `docs` can no longer be placed, and in
/// the same listing OneDrive moved `g.txt` out of it. The file is moved, the
/// one object it was, and then the folder goes; with something waiting in
/// the folder, the file is moved all the same and the folder stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_a_full_reconcile_what_onedrive_moved_out_is_moved_before_the_folder_goes_or_waits() {
    for waiting in [false, true] {
        let w = Arc::new(World::read_write().await);
        let listing = w.listed().await;
        w.graph.with(|c| c.add_file("G", "D", "g.txt", b"g"));
        w.cycle(&listing).await;
        let g = handle_of(&w.path("docs/g.txt"));
        if waiting {
            std::fs::write(w.path("docs/new.txt"), b"new").unwrap();
        }
        w.graph.with(|c| {
            c.rename("D", ROOT, &long_name());
            c.rename("G", ROOT, "g.txt");
        });
        listing.request_full();
        let report = w.cycle(&listing).await;
        assert!(report.full, "waiting={waiting}");
        assert_eq!(handle_of(&w.path("g.txt")), g, "waiting={waiting}: moved, not made again");
        assert_eq!(w.path("docs").exists(), waiting);
        if waiting {
            assert!(w.path("docs/f.txt").exists() && !w.path("docs/g.txt").exists());
            assert_eq!(w.waits().await, [WaitsFor::Changes("docs/new.txt".into())]);
            w.rounds(&listing, 3).await;
            assert!(!w.path("docs").exists(), "gone once the new file is up");
            assert!(w.graph.with(|c| c.items.values().any(|i| i.name == "new.txt" && i.parent.as_deref() == Some("D"))));
        }
        w.scan_and_upload().await;
        assert_eq!((w.deletes(), w.graph.with(|c| c.count("PATCH", "items/"))), (0, 0), "waiting={waiting}");
    }
}

/// The two steps of a rename through a temporary name, while OneDrive has
/// the item in a folder that is not placed: the step to the temporary name
/// meets a `412` and is sent as a name alone, its commit keeps the item in
/// the folder its object stands in, and the move that follows sends the
/// final name and no folder. The item is never moved back out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_second_step_of_a_rename_through_a_temporary_name_never_moves_the_item_back() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    w.graph.with(|c| c.add(folder_item("S", ROOT, &long_name())));
    w.cycle(&listing).await;
    std::fs::rename(w.path("docs/f.txt"), w.path("docs/g.txt")).unwrap();
    let mut batch = crate::local::Batch::new();
    batch.name(Path::new("docs"), std::ffi::OsStr::new("f.txt"));
    batch.name(Path::new("docs"), std::ffi::OsStr::new("g.txt"));
    let seq = w.examine(batch).await.applied.queued[0];
    // As the worker leaves the row when the name it takes is still
    // another's: it goes through a temporary name first.
    let swap = format!("{}F", konedrive_tree::outbox::SWAP_PREFIX);
    w.store.call({ let swap = swap.clone(); move |s| s.outbox_set_target(seq, Some("D"), Some(&swap)) }).await.unwrap();
    w.graph.with(|c| c.rename("F", "S", "f.txt"));
    w.upload().await;
    let names: Vec<String> = w.graph.with(|c| c.log.iter().filter(|(m, p)| m == "PATCH" && p.ends_with("items/F")).map(|(_, p)| p.clone()).collect());
    assert!(names.len() >= 2, "the temporary step and the final one: {names:?}");
    w.graph.with(|c| {
        let f = c.item("F").unwrap();
        assert_eq!((f.parent.as_deref(), f.name.as_str()), (Some("S"), "g.txt"), "renamed where OneDrive has it");
    });
    assert_eq!(w.base("F").map(|row| (row.parent_id.unwrap(), row.name)), Some(("D".to_owned(), "g.txt".to_owned())));
    assert!(w.store.call(|s| s.outbox_rows()).await.unwrap().is_empty());
    // Cycles and a scan of the whole folder after it: the file leaves, and
    // nothing more is sent.
    let sent = w.graph.with(|c| c.log.iter().filter(|(m, _)| m != "GET").count());
    w.rounds(&listing, 1).await;
    assert!(!w.path("docs/g.txt").exists(), "nothing waited in it: it left");
    w.scan_and_upload().await;
    w.rounds(&listing, 2).await;
    assert_eq!(w.graph.with(|c| c.log.iter().filter(|(m, _)| m != "GET").count()), sent, "{:?}", w.graph.with(|c| c.log.clone()));
    assert!(w.waits().await.is_empty());
    assert_eq!(w.graph.with(|c| c.item("F").map(|f| (f.parent.clone().unwrap(), f.name.clone())).unwrap()), ("S".to_owned(), "g.txt".to_owned()));
    assert_eq!(w.deletes(), 0);
}

/// In one listing OneDrive takes an item to where the folder cannot hold it
/// and puts another item at its name. The one that leaves is taken off, and
/// nothing else: the newcomer is placed at the name and stays, with its
/// record; nothing is deleted or renamed in OneDrive, and once both are
/// settled further cycles and a scan of the whole folder send nothing and
/// nothing is said to wait. In a Changed reconcile, in a Full one, and with
/// a folder with a file in it as the newcomer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_item_at_the_name_of_one_that_leaves_is_placed_and_never_taken_for_it() {
    for (full, folder) in [(false, false), (true, false), (false, true), (true, true)] {
        let case = format!("full={full} folder={folder}");
        let w = Arc::new(World::read_write().await);
        let listing = w.listed().await;
        w.graph.with(|c| {
            c.rename("T", ROOT, &long_name());
            if folder {
                c.add(folder_item("Y", ROOT, "top.txt"));
                c.add_file("C", "Y", "child.txt", b"child");
            } else {
                c.add_file("Y", ROOT, "top.txt", b"another");
            }
        });
        if full {
            listing.request_full();
        }
        w.rounds(&listing, 3).await;
        assert_eq!(id_at(&w.path("top.txt")).as_deref(), Some("Y"), "{case}: the newcomer has the name");
        assert_eq!(w.recorded_handle("Y").await, Some(handle_of(&w.path("top.txt"))), "{case}");
        if folder {
            assert_eq!(id_at(&w.path("top.txt/child.txt")).as_deref(), Some("C"), "{case}");
        }
        assert_eq!(crate::remote::testing::tree_of(&w.root.path).iter().filter(|name| name.starts_with("top")).count(), if folder { 2 } else { 1 }, "{case}: {:?}", crate::remote::testing::tree_of(&w.root.path));
        assert_eq!(w.recorded_handle("T").await, None, "{case}: the one that left is off the disk");
        let lines = w.store.call(|s| s.skipped()).await.unwrap();
        assert!(lines.iter().all(|line| line.waits.is_none()) && lines.len() == 1, "{case}: {lines:?}");
        assert!(w.store.call(|s| s.deferred_ids()).await.unwrap().is_empty(), "{case}: nothing waits");
        w.scan_and_upload().await;
        w.rounds(&listing, 2).await;
        assert_eq!(w.graph.with(|c| c.log.iter().filter(|(m, _)| m != "GET").count()), 0, "{case}: {:?}", w.graph.with(|c| c.log.clone()));
        assert_eq!(id_at(&w.path("top.txt")).as_deref(), Some("Y"), "{case}");
        assert!(w.graph.with(|c| c.bin.is_empty() && c.item("T").is_some()), "{case}");
    }
}

/// The user moves a file into a folder made here, not in OneDrive yet,
/// while OneDrive renames the file to a name the folder cannot hold. The
/// move is sent, the folder alone; the base then has the item where its
/// object stands, in the new folder. Nothing is said to wait where nothing
/// is, and a scan and further cycles send nothing more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_move_made_here_into_a_new_folder_is_kept_as_the_place_when_onedrive_renamed_the_item() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    std::fs::create_dir(w.path("n")).unwrap();
    std::fs::rename(w.path("docs/f.txt"), w.path("n/f.txt")).unwrap();
    let mut batch = crate::local::Batch::new();
    batch.name(Path::new(""), std::ffi::OsStr::new("n"));
    batch.tree(Path::new("n"));
    batch.name(Path::new("docs"), std::ffi::OsStr::new("f.txt"));
    w.examine(batch).await;
    let long = long_name();
    w.graph.with(|c| c.rename("F", "D", &long));
    w.upload().await;
    let n = w.graph.with(|c| c.items.values().find(|i| i.name == "n").map(|i| i.id.clone())).expect("the folder was made");
    w.graph.with(|c| {
        let f = c.item("F").unwrap();
        assert_eq!((f.parent.as_deref(), f.name.as_str()), (Some(n.as_str()), long.as_str()), "moved, and not renamed back");
    });
    assert_eq!(w.base("F").map(|row| (row.parent_id.unwrap(), row.name)), Some((n.clone(), "f.txt".to_owned())), "where its object stands");
    w.rounds(&listing, 1).await;
    let waits = w.waits().await;
    assert!(waits.is_empty(), "{waits:?}");
    assert!(!w.path("n/f.txt").exists() && w.path("n").exists(), "nothing waited in it: it left");
    let sent = w.graph.with(|c| c.log.iter().filter(|(m, _)| m != "GET").count());
    w.scan_and_upload().await;
    w.rounds(&listing, 2).await;
    assert_eq!(w.graph.with(|c| c.log.iter().filter(|(m, _)| m != "GET").count()), sent, "{:?}", w.graph.with(|c| c.log.clone()));
    assert_eq!(w.deletes(), 0);
}

/// Every object of the folder: its path, its id, its inode and its change
/// time (a rename changes that, also one that ends where it began).
fn objects(w: &World) -> Vec<(String, String, u64, (i64, i64))> {
    use std::os::unix::fs::MetadataExt;
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, String, u64, (i64, i64))>) {
        let mut paths: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(Result::ok).map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let id = super::id_at(&path).unwrap_or_default();
            // An object with no id is the worker's to mark, which the
            // fixture's worker does anew at each run: only its place counts.
            let stamp = if id.is_empty() { (0, 0) } else { (meta.ctime(), meta.ctime_nsec()) };
            out.push((path.strip_prefix(root).unwrap().display().to_string(), id, meta.ino(), stamp));
            if meta.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(&w.root.path, &w.root.path, &mut out);
    out
}

impl World {
    fn sent(&self) -> Vec<(String, String)> {
        self.graph.with(|c| c.log.iter().filter(|(m, _)| m != "GET").cloned().collect())
    }

    /// What OneDrive says now is applied, and then four more cycles, each
    /// followed by the watcher's and the worker's part, a scan of the whole
    /// folder and one more cycle. The folder settles: in the last cycles no
    /// object is moved, not even away and back, and no request is sent. And
    /// from the first to the last, nothing is sent to OneDrive that changes
    /// anything there. Where each object is at the end, with its id.
    async fn settles(&self, case: &str, listing: &Arc<Listing>, full: bool) -> Vec<(String, String)> {
        if full {
            listing.request_full();
        }
        let mut seen = Vec::new();
        for _ in 0..5 {
            self.rounds(listing, 1).await;
            seen.push((objects(self), self.sent().len()));
        }
        assert_eq!(seen[2], seen[3], "{case}: something moved, or was sent, in a cycle with nothing new");
        assert_eq!(seen[3], seen[4], "{case}: something moved, or was sent, in a cycle with nothing new");
        self.scan_and_upload().await;
        self.rounds(listing, 1).await;
        assert_eq!(self.sent(), [], "{case}: sent to OneDrive");
        assert!(self.graph.with(|c| c.bin.is_empty()), "{case}");
        let end = objects(self);
        assert_eq!(end.iter().map(|o| (&o.0, &o.1, o.2)).collect::<Vec<_>>(), seen[4].0.iter().map(|o| (&o.0, &o.1, o.2)).collect::<Vec<_>>(), "{case}: after a scan of the whole folder");
        end.into_iter().map(|(path, id, _, _)| (path, id)).collect()
    }

    /// The lines of the skipped list that say an item is still here: where
    /// it is here, and what it waits for.
    async fn still_here(&self) -> Vec<(String, WaitsFor)> {
        let lines = self.store.call(|s| s.skipped()).await.unwrap();
        lines.into_iter().filter_map(|line| Some((line.here?.display().to_string(), line.waits?))).collect()
    }
}

fn places(end: &[(String, String)]) -> Vec<(&str, &str)> {
    end.iter().map(|(path, id)| (path.as_str(), id.as_str())).collect()
}

/// A folder gets a name too long in OneDrive, and in the same listing its
/// child is moved to the root under the folder's old name; or a new folder
/// takes that name and the child is moved into it. The folder that leaves
/// steps aside, what takes its name is placed at once, and the folder, with
/// nothing waiting in it, leaves. Nothing waits on anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_that_leaves_yields_its_name_to_its_own_child_or_to_the_childs_new_folder() {
    for (into_new_folder, full) in [(false, false), (false, true), (true, false), (true, true)] {
        let case = format!("into_new_folder={into_new_folder} full={full}");
        let w = Arc::new(World::read_write().await);
        let listing = w.listed().await;
        w.graph.with(|c| {
            c.add(folder_item("X", ROOT, "top"));
            c.add_file("C", "X", "a.txt", b"child");
        });
        w.cycle(&listing).await;
        let child = handle_of(&w.path("top/a.txt"));
        w.graph.with(|c| {
            c.rename("X", ROOT, &long_name());
            if into_new_folder {
                c.add(folder_item("Y", ROOT, "top"));
                c.rename("C", "Y", "a.txt");
            } else {
                c.rename("C", ROOT, "top");
            }
        });
        let end = w.settles(&case, &listing, full).await;
        let expected: Vec<(&str, &str)> = if into_new_folder {
            vec![("docs", "D"), ("docs/f.txt", "F"), ("top", "Y"), ("top/a.txt", "C"), ("top.txt", "T")]
        } else {
            vec![("docs", "D"), ("docs/f.txt", "F"), ("top", "C"), ("top.txt", "T")]
        };
        assert_eq!(places(&end), expected, "{case}");
        assert_eq!(handle_of(&w.path(if into_new_folder { "top/a.txt" } else { "top" })), child, "{case}: the child is moved, not made again");
        assert_eq!(w.still_here().await, [], "{case}");
        assert!(w.store.call(|s| s.deferred_ids()).await.unwrap().is_empty(), "{case}: nothing waits");
    }
}

/// A folder that cannot leave yet (a blocked new file in it) gets a name too
/// long, and a new folder with a file takes its name. The one that waits
/// steps aside under a copy name, on disk and in the base alike, so nothing
/// is sent for it; the newcomer is placed at once; and the skipped list
/// says where the folder that waits is now, and what keeps it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_folder_that_waits_steps_aside_for_a_newcomer_at_its_name_and_says_where_it_is() {
    for full in [false, true] {
        let case = format!("full={full}");
        let w = Arc::new(World::read_write().await);
        let listing = w.listed().await;
        w.blocked_file_in("docs").await;
        let docs = handle_of(&w.path("docs"));
        w.graph.with(|c| {
            c.rename("D", ROOT, &long_name());
            c.add(folder_item("Y", ROOT, "docs"));
            c.add_file("YC", "Y", "new-there.txt", b"there");
        });
        let end = w.settles(&case, &listing, full).await;
        assert_eq!(
            places(&end),
            [("docs", "Y"), ("docs/new-there.txt", "YC"), ("docs-fedora", "D"), ("docs-fedora/f.txt", "F"), ("docs-fedora/n:ew.txt", ""), ("top.txt", "T")],
            "{case}"
        );
        assert_eq!(handle_of(&w.path("docs-fedora")), docs, "{case}: the same folder, renamed");
        assert_eq!(std::fs::read(w.path("docs-fedora/n:ew.txt")).unwrap(), b"new", "{case}");
        assert_eq!(w.still_here().await, [("docs-fedora".to_owned(), WaitsFor::Uploads(1))], "{case}");
        assert_eq!(w.base("D").map(|row| row.name).as_deref(), Some("docs-fedora"), "{case}: the base has it where it stands");
        assert_eq!(w.store.call(|s| s.deferred_ids()).await.unwrap(), ["D"], "{case}: only the folder that waits");
        // A rename of it made here is the user's, and is sent; the file in
        // it then goes up, and under a name the folder can hold it stays.
        std::fs::rename(w.path("docs-fedora"), w.path("mine")).unwrap();
        std::fs::rename(w.path("mine/n:ew.txt"), w.path("mine/new.txt")).unwrap();
        w.scan_and_upload().await;
        w.rounds(&listing, 2).await;
        w.graph.with(|c| {
            assert_eq!(c.item("D").unwrap().name, "mine", "{case}");
            assert!(c.items.values().any(|i| i.name == "new.txt" && i.parent.as_deref() == Some("D")), "{case}: {:?}", c.paths());
        });
        assert_eq!((id_at(&w.path("mine")).as_deref(), w.still_here().await), (Some("D"), vec![]), "{case}");
        assert_eq!(w.deletes(), 0);
    }
}

/// A chain of names in one listing: `top.txt` gets a name too long,
/// `docs/f.txt` is renamed to `top.txt` in the root, and a new file takes
/// `docs/f.txt`. Each is placed at once, the one that leaves leaves, and
/// nothing is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chain_of_names_through_one_that_leaves_is_placed_at_once_and_sends_nothing() {
    for full in [false, true] {
        let case = format!("full={full}");
        let w = Arc::new(World::read_write().await);
        let listing = w.listed().await;
        let f = handle_of(&w.path("docs/f.txt"));
        w.graph.with(|c| {
            c.rename("T", ROOT, &long_name());
            c.rename("F", ROOT, "top.txt");
            c.add_file("Z", "D", "f.txt", b"another");
        });
        let end = w.settles(&case, &listing, full).await;
        assert_eq!(places(&end), [("docs", "D"), ("docs/f.txt", "Z"), ("top.txt", "F")], "{case}");
        assert_eq!(handle_of(&w.path("top.txt")), f, "{case}: moved, not made again");
        assert_eq!(w.still_here().await, [], "{case}");
        assert!(w.store.call(|s| s.deferred_ids()).await.unwrap().is_empty(), "{case}");
    }
}

/// A stop of the daemon right after it renamed a folder that waits aside,
/// before the base heard of it: the scan at the next start takes the new
/// name for a rename made here, and the cycle after it knows the name for
/// its own, takes it into the base and drops that rename. Nothing is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_between_the_step_aside_and_its_record_is_repaired_and_sends_nothing() {
    let w = Arc::new(World::read_write().await);
    let listing = w.listed().await;
    w.docs_waiting(&listing).await;
    std::fs::rename(w.path("docs"), w.path("docs-fedora")).unwrap();
    {
        let w = Arc::clone(&w);
        tokio::task::spawn_blocking(move || scan_now(&w)).await.unwrap();
    }
    // The first cycle after a start reconciles the whole folder.
    listing.request_full();
    w.rounds(&listing, 2).await;
    assert_eq!(w.sent(), []);
    assert_eq!(w.base("D").map(|row| row.name).as_deref(), Some("docs-fedora"));
    assert_eq!(w.still_here().await, [("docs-fedora".to_owned(), WaitsFor::Uploads(1))]);
    w.scan_and_upload().await;
    w.rounds(&listing, 1).await;
    assert_eq!(w.sent(), []);
}

/// Names taken and given in one listing: the shapes a review ran, and the
/// enumeration of the small ones.
mod names;
/// The same listings in a read-only folder, which ends as OneDrive has it.
mod read_only;
