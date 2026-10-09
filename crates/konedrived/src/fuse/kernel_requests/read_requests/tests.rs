//! Where a read's bytes come from, found on the parts of a mount built without a mount:
//! what the handler does besides is reply.

use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use fuser::Errno;
use konedrive_tree::{Change, Kind, Row};

use super::super::read_origin::{ReadOrigin, WRITTEN_BEHIND};
use crate::fuse::directory_contents::tests::row;
use crate::fuse::mount::tests::{build_mount, build_mount_with, BuiltMount};
use crate::fuse::node_table::NodeId;
use crate::fuse::open_handles::{Fh, OpenFile};

/// The parts of a mount whose drive has `a.txt`, the OneDrive file `A`, with 100 bytes.
fn mount_with_a_file_of_100_bytes() -> BuiltMount {
    build_mount_with(&[Change::Upsert(Row { size: 100, ..row("A", Some("R"), "a.txt", Kind::File) })], &[])
}

/// An `fh` that holds the OneDrive file `id` open, opened at `path` while it was not
/// downloaded.
fn open_remote_only(built: &BuiltMount, id: &str, path: &str) -> Fh {
    built.kernel_requests.read_requests.open_handles.add_file(OpenFile::ReadingRemoteOnly { onedrive_id: id.into(), path: path.into() })
}

#[test]
fn an_fh_that_holds_no_file_open_is_refused() {
    let built = build_mount();
    let read_requests = &built.kernel_requests.read_requests;
    assert_eq!(read_requests.find_read_origin(Fh(77), 0, 10).err(), Some(Errno::EBADF));
    // A directory is no file.
    let directory = read_requests.open_handles.add_directory();
    assert_eq!(read_requests.find_read_origin(directory, 0, 10).err(), Some(Errno::EBADF));
}

#[test]
fn a_file_opened_from_the_backing_directory_is_read_from_the_file_it_was_opened_from() {
    let built = build_mount();
    let read_requests = &built.kernel_requests.read_requests;
    let backing_file = Arc::new(tempfile::tempfile().unwrap());
    let reading = read_requests.open_handles.add_file(OpenFile::ReadingBackingFile { backing_file: Arc::clone(&backing_file) });
    assert!(matches!(read_requests.find_read_origin(reading, 0, 10), Ok(ReadOrigin::BackingFile(file)) if Arc::ptr_eq(&file, &backing_file)));
    let writing = read_requests.open_handles.add_writer(NodeId(9), None, Arc::clone(&backing_file)).fh;
    assert!(matches!(read_requests.find_read_origin(writing, 500, 10), Ok(ReadOrigin::BackingFile(file)) if Arc::ptr_eq(&file, &backing_file)));
}

#[test]
fn a_file_that_is_not_downloaded_is_read_from_onedrive_as_a_range_of_the_stores_row() {
    let built = mount_with_a_file_of_100_bytes();
    let fh = open_remote_only(&built, "A", "a.txt");
    let origin = built.kernel_requests.read_requests.find_read_origin(fh, 95, 10).unwrap();
    assert!(matches!(&origin, ReadOrigin::OneDriveRange { row, path, from: 95, end: 100 } if row.id == "A" && path == &PathBuf::from("a.txt")), "{origin:?}");
}

/// Its copy is opened, and the handle reads it from now on, whatever becomes of the copy's
/// name.
#[test]
fn a_file_downloaded_since_it_was_opened_is_read_from_its_copy_from_then_on() {
    let built = mount_with_a_file_of_100_bytes();
    let read_requests = &built.kernel_requests.read_requests;
    let fh = open_remote_only(&built, "A", "a.txt");
    built.download("a.txt", "A", b"content");
    let Ok(ReadOrigin::BackingFile(copy)) = read_requests.find_read_origin(fh, 0, 10) else { panic!("read from the copy") };
    assert_eq!(copy.metadata().unwrap().len(), 7);
    assert!(matches!(read_requests.open_handles.find_file(fh), Some(OpenFile::ReadingBackingFile { backing_file }) if Arc::ptr_eq(&backing_file, &copy)));
    // The copy is removed from its name: the handle still reads the file it holds.
    std::fs::remove_file(built.disk.join("a.txt")).unwrap();
    assert!(matches!(read_requests.find_read_origin(fh, 0, 10), Ok(ReadOrigin::BackingFile(file)) if Arc::ptr_eq(&file, &copy)));
}

/// A file at the path that is not the file's whole downloaded copy is not read as it.
#[test]
fn a_file_at_the_path_that_is_not_the_downloaded_copy_is_not_read() {
    let built = mount_with_a_file_of_100_bytes();
    let fh = open_remote_only(&built, "A", "a.txt");
    std::fs::write(built.disk.join("a.txt"), b"the user's own").unwrap();
    assert!(matches!(built.kernel_requests.read_requests.find_read_origin(fh, 0, 10), Ok(ReadOrigin::OneDriveRange { .. })));
}

#[test]
fn a_read_well_below_what_a_running_download_has_written_comes_from_the_downloads_file() {
    let built = mount_with_a_file_of_100_bytes();
    let read_requests = &built.kernel_requests.read_requests;
    let fh = open_remote_only(&built, "A", "a.txt");
    let download_file: Arc<File> = Arc::new(tempfile::tempfile().unwrap());
    read_requests.download_states.start_or_join("A", None);
    // A download that has no file yet answers no read.
    assert!(matches!(read_requests.find_read_origin(fh, 0, 10), Ok(ReadOrigin::OneDriveRange { .. })));
    let written_up_to = read_requests.download_states.begin_writing("A", Arc::clone(&download_file));
    written_up_to.store(WRITTEN_BEHIND + 10, Ordering::SeqCst);
    assert!(matches!(read_requests.find_read_origin(fh, 0, 10), Ok(ReadOrigin::RunningDownload(file)) if Arc::ptr_eq(&file, &download_file)));
    assert!(matches!(read_requests.find_read_origin(fh, 5, 10), Ok(ReadOrigin::OneDriveRange { from: 5, end: 15, .. })));
}

#[test]
fn a_file_that_is_not_downloaded_and_is_gone_from_the_store_is_refused() {
    let built = build_mount();
    let fh = open_remote_only(&built, "gone", "gone.txt");
    assert_eq!(built.kernel_requests.read_requests.find_read_origin(fh, 0, 10).err(), Some(Errno::ENOENT));
}
