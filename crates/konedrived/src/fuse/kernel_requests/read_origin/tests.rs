use std::fs::File;
use std::sync::Arc;

use super::{choose_read_origin, ReadFacts, ReadOrigin, WRITTEN_BEHIND};
use crate::fuse::node_table::NodeId;
use crate::fuse::open_handles::OpenFile;

fn backing_file() -> Arc<File> {
    Arc::new(tempfile::tempfile().unwrap())
}

fn remote_only() -> OpenFile {
    OpenFile::ReadingRemoteOnly { onedrive_id: "A".into(), path: "a.txt".into() }
}

/// A file of 100 bytes that is not downloaded, and of which no download runs.
fn not_downloaded() -> ReadFacts {
    ReadFacts { downloaded_since_open: false, file_size: 100, download_written_up_to: None }
}

#[test]
fn a_file_opened_from_the_backing_directory_is_read_there() {
    let reading = OpenFile::ReadingBackingFile { backing_file: backing_file() };
    assert_eq!(choose_read_origin(&reading, &ReadFacts::default(), 0, 10), ReadOrigin::BackingFile);
    let writing = OpenFile::Writing { node_id: NodeId(2), onedrive_id: None, backing_file: backing_file() };
    assert_eq!(choose_read_origin(&writing, &ReadFacts::default(), 50, 10), ReadOrigin::BackingFile);
}

#[test]
fn a_file_downloaded_since_it_was_opened_is_read_from_its_copy() {
    let facts = ReadFacts { downloaded_since_open: true, ..not_downloaded() };
    assert_eq!(choose_read_origin(&remote_only(), &facts, 0, 10), ReadOrigin::BackingFile);
    // Whatever is asked for: the copy answers a read past its end itself.
    assert_eq!(choose_read_origin(&remote_only(), &facts, 500, 10), ReadOrigin::BackingFile);
}

#[test]
fn a_file_that_is_not_downloaded_is_read_from_onedrive_as_a_range() {
    assert_eq!(choose_read_origin(&remote_only(), &not_downloaded(), 20, 10), ReadOrigin::OneDriveRange { from: 20, end: 30 });
}

#[test]
fn a_range_is_cut_at_the_end_of_the_file() {
    assert_eq!(choose_read_origin(&remote_only(), &not_downloaded(), 95, 10), ReadOrigin::OneDriveRange { from: 95, end: 100 });
}

#[test]
fn a_read_at_or_past_the_end_of_the_file_comes_from_nowhere() {
    assert_eq!(choose_read_origin(&remote_only(), &not_downloaded(), 100, 10), ReadOrigin::Nothing);
    assert_eq!(choose_read_origin(&remote_only(), &not_downloaded(), 500, 10), ReadOrigin::Nothing);
    assert_eq!(choose_read_origin(&remote_only(), &not_downloaded(), 10, 0), ReadOrigin::Nothing);
}

#[test]
fn a_read_well_below_what_a_running_download_has_written_comes_from_its_file() {
    let facts = ReadFacts { download_written_up_to: Some(WRITTEN_BEHIND + 30), ..not_downloaded() };
    assert_eq!(choose_read_origin(&remote_only(), &facts, 20, 10), ReadOrigin::RunningDownload);
}

/// The download may still be writing the piece the read ends in.
#[test]
fn a_read_that_ends_too_near_what_a_running_download_has_written_comes_from_onedrive() {
    let facts = ReadFacts { download_written_up_to: Some(WRITTEN_BEHIND + 29), ..not_downloaded() };
    assert_eq!(choose_read_origin(&remote_only(), &facts, 20, 10), ReadOrigin::OneDriveRange { from: 20, end: 30 });
    let nothing_written = ReadFacts { download_written_up_to: Some(0), ..not_downloaded() };
    assert_eq!(choose_read_origin(&remote_only(), &nothing_written, 20, 10), ReadOrigin::OneDriveRange { from: 20, end: 30 });
}
