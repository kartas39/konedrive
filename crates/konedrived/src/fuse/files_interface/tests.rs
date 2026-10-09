//! What the `Files` interface answers, on the parts of a mount built without a mount: the
//! fixture's drive (`docs` with `c.txt`; `a.txt` and `b.txt` in the root), whose source
//! gives every file no bytes, and a bridge whose thread is not started.

use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use konedrive_tree::{Change, Kind, Row};

use super::{DownloadedCopies, FilesInterface, FilesRefusal};
use crate::fuse::directory_contents::tests::row;
use crate::fuse::inotify_bridge::Announcement;
use crate::fuse::mount::tests::{build_mount, build_mount_with, BuiltMount};
use crate::fuse::node_table::NodeId;

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(future)
}

fn shown_changed(path: &str, onedrive_id: &str) -> Announcement {
    Announcement::FileShownChanged { path: path.into(), onedrive_id: onedrive_id.into() }
}

/// The fixture's three files downloaded: `a.txt`, `b.txt` and `docs/c.txt`.
fn download_all(built: &BuiltMount) {
    built.make_marked_directory("docs", "D");
    built.download("a.txt", "A", b"aaaa");
    built.download("b.txt", "B", b"bbbb");
    built.download("docs/c.txt", "C", b"cccc");
}

/// The bytes the file at `rel` takes on disk.
fn bytes_on_disk(built: &BuiltMount, rel: &str) -> u64 {
    std::fs::metadata(built.disk.join(rel)).unwrap().blocks() * 512
}

/// A program holds the OneDrive file `id` open for writing.
fn open_for_writing(files_interface: &FilesInterface, id: &str) {
    files_interface.open_handles.add_writer(NodeId(900), Some(id.to_owned()), Arc::new(tempfile::tempfile().unwrap()));
}

#[test]
fn a_path_outside_the_folder_is_refused() {
    let built = build_mount();
    let files_interface = built.mount.files_interface();
    let outside = Path::new("/elsewhere/a.txt");
    assert!(matches!(block_on(files_interface.download(outside)), Err(FilesRefusal::OutsideFolder)));
    assert!(matches!(block_on(files_interface.free_up(outside)), Err(FilesRefusal::OutsideFolder)));
    assert!(matches!(block_on(files_interface.set_pin(outside, true)), Err(FilesRefusal::OutsideFolder)));
    assert!(matches!(files_interface.count_downloaded_below(outside), Err(FilesRefusal::OutsideFolder)));
}

/// A local-only entry is not OneDrive's to download, free or pin; neither is a name the
/// folder does not have.
#[test]
fn what_is_not_in_onedrive_is_refused() {
    let built = build_mount();
    let files_interface = built.mount.files_interface();
    std::fs::write(built.disk.join("new.txt"), b"mine").unwrap();
    for rel in ["new.txt", "none.txt"] {
        let path = built.path(rel);
        assert!(matches!(block_on(files_interface.download(&path)), Err(FilesRefusal::NotInOneDrive)), "{rel}");
        assert!(matches!(block_on(files_interface.free_up(&path)), Err(FilesRefusal::NotInOneDrive)), "{rel}");
        assert!(matches!(block_on(files_interface.set_pin(&path, true)), Err(FilesRefusal::NotInOneDrive)), "{rel}");
        assert!(matches!(files_interface.find_path_status(&path), Err(FilesRefusal::NotInOneDrive)), "{rel}");
        assert!(matches!(files_interface.count_downloaded_below(&path), Err(FilesRefusal::NotInOneDrive)), "{rel}");
    }
    assert_eq!(files_interface.count_pins(), 0);
}

/// A directory has no content of its own to download.
#[test]
fn a_directory_is_not_downloaded() {
    let built = build_mount();
    assert!(matches!(block_on(built.mount.files_interface().download(&built.path("docs"))), Err(FilesRefusal::NotInOneDrive)));
}

#[test]
fn a_file_that_is_downloaded_already_is_not_downloaded_again() {
    let built = build_mount();
    let files_interface = built.mount.files_interface();
    built.download("a.txt", "A", b"content");
    assert!(block_on(files_interface.download(&built.path("a.txt"))).is_ok());
    assert!(!files_interface.download_states.is_downloading("A"));
    assert!(files_interface.inotify_bridge.list_queued_announcements().is_empty(), "nothing changed of what is shown");
    assert_eq!(std::fs::read(built.disk.join("a.txt")).unwrap(), b"content");
}

/// The download is waited for: when it returns, the file's copy is in the backing directory.
#[test]
fn a_file_that_is_not_downloaded_is_downloaded_and_waited_for() {
    let built = build_mount_with(&[Change::Upsert(Row { size: 0, ..row("A", Some("R"), "a.txt", Kind::File) })], &[]);
    let files_interface = built.mount.files_interface();
    assert!(block_on(files_interface.download(&built.path("a.txt"))).is_ok());
    assert!(files_interface.backing_directory.find_downloaded_copy(Path::new("a.txt"), "A").is_some());
    assert!(!files_interface.download_states.is_downloading("A"));
    assert!(files_interface.inotify_bridge.list_queued_announcements().contains(&shown_changed("a.txt", "A")));
}

/// What a pin keeps and what a program writes is left, and counted as kept; what is freed
/// is announced, as what is shown of it changed.
#[test]
fn a_free_up_removes_the_copies_nothing_keeps_and_counts_the_ones_it_leaves() {
    let built = build_mount_with(&[], &["B"]);
    let files_interface = built.mount.files_interface();
    download_all(&built);
    open_for_writing(files_interface, "C");
    let bytes_of_a = bytes_on_disk(&built, "a.txt");
    let freed = block_on(files_interface.free_up(&built.path("")));
    let freed = freed.unwrap();
    assert_eq!((freed.files, freed.bytes, freed.kept), (1, bytes_of_a, 2));
    assert!(!built.disk.join("a.txt").exists() && built.disk.join("b.txt").is_file() && built.disk.join("docs/c.txt").is_file());
    assert_eq!(files_interface.inotify_bridge.list_queued_announcements(), [shown_changed("a.txt", "A")]);
}

/// Only a downloaded copy is counted: what is not downloaded takes no space, whatever keeps
/// it.
#[test]
fn what_is_not_downloaded_is_neither_freed_nor_counted_as_kept() {
    let built = build_mount_with(&[], &["B"]);
    let files_interface = built.mount.files_interface();
    built.download("a.txt", "A", b"aaaa");
    let freed = block_on(files_interface.free_up_all()).unwrap();
    assert_eq!((freed.files, freed.kept), (1, 0));
    // And a second free-up finds nothing left to free.
    let again = block_on(files_interface.free_up_all()).unwrap();
    assert_eq!((again.files, again.bytes, again.kept), (0, 0, 0));
}

/// Below a directory, the free-up is of what that directory holds.
#[test]
fn a_free_up_of_a_directory_frees_what_is_below_it_alone() {
    let built = build_mount();
    let files_interface = built.mount.files_interface();
    download_all(&built);
    let freed = block_on(files_interface.free_up(&built.path("docs"))).unwrap();
    assert_eq!((freed.files, freed.kept), (1, 0));
    assert!(!built.disk.join("docs/c.txt").exists() && built.disk.join("a.txt").is_file() && built.disk.join("b.txt").is_file());
}

#[test]
fn the_downloaded_copies_below_a_path_are_counted_with_their_bytes_on_disk() {
    let built = build_mount();
    let files_interface = built.mount.files_interface();
    assert_eq!(files_interface.count_downloaded_below(&built.path("")).unwrap(), DownloadedCopies::default());
    built.make_marked_directory("docs", "D");
    built.download("a.txt", "A", b"aaaa");
    built.download("docs/c.txt", "C", b"cccc");
    let (a, c) = (bytes_on_disk(&built, "a.txt"), bytes_on_disk(&built, "docs/c.txt"));
    assert_eq!(files_interface.count_downloaded_below(&built.path("")).unwrap(), DownloadedCopies { files: 2, bytes: a + c });
    assert_eq!(files_interface.count_downloaded_below(&built.path("docs")).unwrap(), DownloadedCopies { files: 1, bytes: c });
    assert_eq!(files_interface.count_downloaded_below(&built.path("a.txt")).unwrap(), DownloadedCopies { files: 1, bytes: a });
    assert_eq!(files_interface.count_downloaded_below(&built.path("b.txt")).unwrap(), DownloadedCopies::default());
    // A file at the path of a OneDrive file that is not its downloaded copy is not counted.
    std::fs::write(built.disk.join("b.txt"), b"the user's own").unwrap();
    assert_eq!(files_interface.count_downloaded_below(&built.path("b.txt")).unwrap(), DownloadedCopies::default());
}

/// What is shown of the file changed with its pin, and what the pin keeps is downloaded:
/// the sweep announces the file once more as its download starts.
#[test]
fn a_pin_put_on_a_file_is_announced_and_downloads_it() {
    let built = build_mount();
    let files_interface = built.mount.files_interface();
    block_on(files_interface.set_pin(&built.path("a.txt"), true)).unwrap();
    assert_eq!(files_interface.count_pins(), 1);
    assert!(files_interface.find_path_status(&built.path("a.txt")).unwrap().own_pin);
    let mut announced = files_interface.inotify_bridge.list_queued_announcements();
    assert_eq!(announced.first(), Some(&shown_changed("a.txt", "A")));
    // The sweep runs on the mount's runtime: its download of the file announces it again.
    let deadline = Instant::now() + Duration::from_secs(5);
    while announced.len() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        announced.extend(files_interface.inotify_bridge.list_queued_announcements());
    }
    assert!(announced.len() >= 2 && announced.iter().all(|announcement| *announcement == shown_changed("a.txt", "A")), "{announced:?}");
}

/// A pin that is there already changes nothing of what is shown; one taken off is announced
/// and downloads nothing.
#[test]
fn a_pin_is_announced_only_when_it_changed() {
    let built = build_mount_with(&[], &["D"]);
    let files_interface = built.mount.files_interface();
    built.make_marked_directory("docs", "D");
    built.download("docs/c.txt", "C", b"cccc");
    block_on(files_interface.set_pin(&built.path("docs"), true)).unwrap();
    assert!(files_interface.inotify_bridge.list_queued_announcements().is_empty());
    block_on(files_interface.set_pin(&built.path("docs"), false)).unwrap();
    assert_eq!(files_interface.count_pins(), 0);
    assert_eq!(files_interface.inotify_bridge.list_queued_announcements(), [shown_changed("docs", "D")]);
    assert!(!files_interface.download_states.is_downloading("C"));
}

/// The parts of a read-write mount whose `a.txt` is downloaded.
fn build_mount_with_downloaded_file() -> BuiltMount {
    let built = build_mount();
    assert!(built.mount.switch_to_read_write(Arc::new(|| {})));
    built.run_on_write_operation_thread(|| {});
    built.download("a.txt", "A", b"from OneDrive");
    built
}

/// Frees up `a.txt` while a cycle holds the tree lock and `queued` jobs wait for the
/// write-operation thread already: the free-up has listed the file, and its turn on the
/// thread comes after them. What it freed, once the lock is let go of.
fn free_up_behind_queued_jobs(built: &BuiltMount, held: tokio::sync::MutexGuard<'_, ()>, queued: usize) -> super::FreedSpace {
    let path = built.path("a.txt");
    std::thread::scope(|scope| {
        let free_up = scope.spawn(|| block_on(built.mount.files_interface().free_up(&path)));
        built.wait_for_queued_write_operations(queued + 1);
        drop(held);
        free_up.join().unwrap().unwrap()
    })
}

/// The file is listed while its copy matches; before its turn a program opens it, writes it
/// and closes it. Its copy holds the only content that is not sent: it is kept, and counted
/// as kept.
#[test]
fn a_file_modified_between_the_listing_and_its_turn_is_kept() {
    let built = build_mount_with_downloaded_file();
    let node_id = built.node_id_in_root("a.txt");
    let held = built.tree_lock.blocking_lock();
    let (write_operations, open_handles) = (Arc::clone(built.kernel_requests.write_operations()), Arc::clone(&built.mount.files_interface().open_handles));
    let _modified = built.queue_on_write_operation_thread(move || {
        let fh = write_operations.open_to_write(node_id).unwrap();
        open_handles.find_file_to_write(fh).unwrap().1.write_all_at(b"the user's own", 0).unwrap();
        open_handles.mark_written(node_id);
        write_operations.close_writer(fh);
    });
    let freed = free_up_behind_queued_jobs(&built, held, 1);
    assert_eq!((freed.files, freed.bytes, freed.kept), (0, 0, 1));
    assert_eq!(std::fs::read(built.disk.join("a.txt")).unwrap(), b"the user's own");
    assert!(built.mount.files_interface().inotify_bridge.list_queued_announcements().is_empty());
}

/// The file is listed while no program writes it; before its turn one opens it for writing.
/// Its copy is what that program writes into: it is kept.
#[test]
fn a_file_opened_for_writing_between_the_listing_and_its_turn_is_kept() {
    let built = build_mount_with_downloaded_file();
    let held = built.tree_lock.blocking_lock();
    let open_handles = Arc::clone(&built.mount.files_interface().open_handles);
    let _opened = built.queue_on_write_operation_thread(move || drop(open_handles.add_writer(NodeId(900), Some("A".into()), Arc::new(tempfile::tempfile().unwrap()))));
    let freed = free_up_behind_queued_jobs(&built, held, 1);
    assert_eq!((freed.files, freed.kept), (0, 1));
    assert!(built.disk.join("a.txt").is_file());
}

/// No write operation runs on a stopped thread, and nothing is freed beside one: the
/// free-up says it freed nothing.
#[test]
fn a_free_up_of_a_folder_whose_write_operation_thread_is_stopped_frees_nothing_and_is_refused() {
    let built = build_mount();
    download_all(&built);
    built.stop_write_operation_thread();
    assert!(matches!(block_on(built.mount.files_interface().free_up(&built.path(""))), Err(FilesRefusal::Io(_))));
    assert!(matches!(block_on(built.mount.files_interface().free_up_all()), Err(FilesRefusal::Io(_))));
    assert!(built.disk.join("a.txt").is_file() && built.disk.join("b.txt").is_file() && built.disk.join("docs/c.txt").is_file());
}
