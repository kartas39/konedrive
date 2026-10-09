use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use konedrive_tree::{Kind, Row};

use super::{choose_read_origin, ReadOrigin, WRITTEN_BEHIND};
use crate::fuse::directory_contents::tests::row;
use crate::fuse::download_states::RunningDownloadFile;

/// The OneDrive file `A` of 100 bytes.
fn file_of_100_bytes() -> Row {
    Row { size: 100, ..row("A", Some("R"), "a.txt", Kind::File) }
}

/// Where `size` bytes at `offset` of the file come from, which was opened at `a.txt`.
fn origin(running_download: Option<RunningDownloadFile>, offset: u64, size: u32) -> ReadOrigin {
    choose_read_origin(file_of_100_bytes(), PathBuf::from("a.txt"), running_download, offset, size)
}

/// A download that has written `written_up_to` bytes in order, into `file`.
fn running(file: &Arc<File>, written_up_to: u64) -> Option<RunningDownloadFile> {
    Some(RunningDownloadFile { file: Arc::clone(file), written_up_to })
}

fn is_range(origin: &ReadOrigin, from: u64, end: u64) -> bool {
    matches!(origin, ReadOrigin::OneDriveRange { row, path, from: f, end: e } if row.id == "A" && path == &PathBuf::from("a.txt") && (*f, *e) == (from, end))
}

#[test]
fn a_file_that_is_not_downloaded_is_read_from_onedrive_as_a_range_of_it() {
    let origin = origin(None, 20, 10);
    assert!(is_range(&origin, 20, 30), "{origin:?}");
}

#[test]
fn a_range_ends_at_the_end_of_the_file() {
    let origin = origin(None, 95, 10);
    assert!(is_range(&origin, 95, 100), "{origin:?}");
}

#[test]
fn a_read_at_or_past_the_end_of_the_file_comes_from_nowhere() {
    assert!(matches!(origin(None, 100, 10), ReadOrigin::Nothing));
    assert!(matches!(origin(None, 500, 10), ReadOrigin::Nothing));
    assert!(matches!(origin(None, 10, 0), ReadOrigin::Nothing));
    // Also while a download of it runs.
    let file = Arc::new(tempfile::tempfile().unwrap());
    assert!(matches!(origin(running(&file, WRITTEN_BEHIND + 500), 100, 10), ReadOrigin::Nothing));
}

#[test]
fn a_read_well_below_what_a_running_download_has_written_comes_from_its_file() {
    let file = Arc::new(tempfile::tempfile().unwrap());
    let origin = origin(running(&file, WRITTEN_BEHIND + 30), 20, 10);
    assert!(matches!(&origin, ReadOrigin::RunningDownload(read_from) if Arc::ptr_eq(read_from, &file)), "{origin:?}");
}

/// The download may still be writing the piece the read ends in.
#[test]
fn a_read_that_ends_too_near_what_a_running_download_has_written_comes_from_onedrive() {
    let file = Arc::new(tempfile::tempfile().unwrap());
    let too_near = origin(running(&file, WRITTEN_BEHIND + 29), 20, 10);
    assert!(is_range(&too_near, 20, 30), "{too_near:?}");
    let nothing_written = origin(running(&file, 0), 20, 10);
    assert!(is_range(&nothing_written, 20, 30), "{nothing_written:?}");
}
