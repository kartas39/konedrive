use std::fs::File;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::State;
use konedrive_tree::Kind;
use tokio::sync::oneshot;

use super::{reaches_download_threshold, to_hydration_state, DownloadStart, DownloadStates, UNFINISHED_FILES_KEPT, WRITTEN_BEHIND};
use crate::fuse::backing_directory::{DiskEntry, DiskMetadata, EntryKind};
use crate::fuse::detections::{FolderPlace, LocalOnlyEntry};
use crate::fuse::directory_contents::tests::row;
use crate::fuse::directory_contents::FileSyncState;

/// Once 100 bytes of a file were read as ranges, it is downloaded whole.
fn download_states() -> DownloadStates {
    DownloadStates::new(100)
}

fn unnamed_file() -> Arc<File> {
    Arc::new(tempfile::tempfile().unwrap())
}

/// A download of `id` is asked for by nobody who waits. The unfinished file it is to
/// continue from, when it was started; `None` when one runs already.
fn start(download_states: &DownloadStates, id: &str) -> Option<Option<Arc<File>>> {
    match download_states.start_or_join(id, None) {
        DownloadStart::Started { unfinished_file } => Some(unfinished_file),
        DownloadStart::Joined => None,
    }
}

/// A download of `id` that got its file and failed: the file it left.
fn fail_download(download_states: &DownloadStates, id: &str) -> Arc<File> {
    let file = start(download_states, id).expect("no download of it runs").unwrap_or_else(unnamed_file);
    download_states.begin_writing(id, Arc::clone(&file));
    download_states.finish(id, Err(libc::EIO));
    file
}

fn copy() -> DiskMetadata {
    DiskMetadata { kind: EntryKind::File, size: 10, blocks: 8, modified: UNIX_EPOCH }
}

#[test]
fn the_first_to_ask_starts_the_download_and_the_next_joins_it() {
    let download_states = download_states();
    assert!(!download_states.is_downloading("A"));
    assert!(matches!(start(&download_states, "A"), Some(None)));
    assert!(download_states.is_downloading("A"));
    assert!(start(&download_states, "A").is_none());
    // Another file's download is its own.
    assert!(matches!(start(&download_states, "B"), Some(None)));
}

#[test]
fn everyone_who_waits_is_told_how_the_download_ended() {
    let download_states = download_states();
    let ((first, first_told), (second, second_told)) = (oneshot::channel(), oneshot::channel());
    download_states.start_or_join("A", Some(first));
    download_states.start_or_join("A", Some(second));
    download_states.finish("A", Ok(()));
    assert_eq!((first_told.blocking_recv(), second_told.blocking_recv()), (Ok(Ok(())), Ok(Ok(()))));
    assert!(!download_states.is_downloading("A"));

    let (waiter, told) = oneshot::channel();
    download_states.start_or_join("A", Some(waiter));
    download_states.finish("A", Err(libc::ENOSPC));
    assert_eq!(told.blocking_recv(), Ok(Err(libc::ENOSPC)));
}

#[test]
fn a_read_is_answered_from_a_running_download_only_well_below_what_it_has_written() {
    let download_states = download_states();
    download_states.start_or_join("A", None);
    // Waiting for its turn in the pool: it has no file yet.
    assert!(download_states.find_written_file("A", 0).is_none());
    let file = unnamed_file();
    let written_up_to = download_states.begin_writing("A", Arc::clone(&file));
    assert!(download_states.find_written_file("A", 10).is_none());
    written_up_to.store(WRITTEN_BEHIND + 10, Ordering::SeqCst);
    assert!(download_states.find_written_file("A", 10).is_some_and(|found| Arc::ptr_eq(&found, &file)));
    assert!(download_states.find_written_file("A", 11).is_none());
    // Not from another file's, and not once it ended.
    assert!(download_states.find_written_file("B", 10).is_none());
    download_states.finish("A", Ok(()));
    assert!(download_states.find_written_file("A", 10).is_none());
}

#[test]
fn a_download_that_ended_well_leaves_nothing() {
    let download_states = download_states();
    download_states.count_bytes_read("A", 100, 1_000);
    download_states.start_or_join("A", None);
    download_states.begin_writing("A", unnamed_file());
    download_states.finish("A", Ok(()));
    assert!(download_states.list_unfinished_files().is_empty());
    // What was read as ranges is counted from nothing again.
    assert!(!download_states.count_bytes_read("A", 99, 1_000));
    assert!(matches!(start(&download_states, "A"), Some(None)));
}

#[test]
fn a_download_that_failed_keeps_its_file_for_the_next_one_to_continue_from() {
    let download_states = download_states();
    let file = fail_download(&download_states, "A");
    assert!(!download_states.is_downloading("A"));
    assert_eq!(download_states.list_unfinished_files().len(), 1);
    let continued = start(&download_states, "A").expect("no download of it runs").expect("the unfinished file");
    assert!(Arc::ptr_eq(&continued, &file));
    // Taken: it is the running download's now.
    assert!(download_states.list_unfinished_files().is_empty());
}

#[test]
fn a_download_that_failed_before_it_had_a_file_keeps_none() {
    let download_states = download_states();
    download_states.start_or_join("A", None);
    download_states.finish("A", Err(libc::EIO));
    assert!(!download_states.is_downloading("A"));
    assert!(download_states.list_unfinished_files().is_empty());
    assert!(matches!(start(&download_states, "A"), Some(None)));
}

#[test]
fn only_the_latest_unfinished_files_are_kept() {
    let download_states = download_states();
    let files: Vec<Arc<File>> = (0..=UNFINISHED_FILES_KEPT).map(|n| fail_download(&download_states, &format!("F{n}"))).collect();
    let kept = download_states.list_unfinished_files();
    assert_eq!(kept.len(), UNFINISHED_FILES_KEPT);
    // The oldest went; the rest are in the order they failed.
    assert!(Arc::ptr_eq(&kept[0], &files[1]));
    assert!(Arc::ptr_eq(&kept[UNFINISHED_FILES_KEPT - 1], &files[UNFINISHED_FILES_KEPT]));
    assert!(matches!(start(&download_states, "F0"), Some(None)));
    assert!(matches!(start(&download_states, "F1"), Some(Some(_))));
}

#[test]
fn a_file_that_fails_again_is_the_latest_failure() {
    let download_states = download_states();
    let first = fail_download(&download_states, "A");
    fail_download(&download_states, "B");
    let again = fail_download(&download_states, "A");
    assert!(Arc::ptr_eq(&first, &again));
    let kept = download_states.list_unfinished_files();
    assert_eq!(kept.len(), 2);
    assert!(Arc::ptr_eq(&kept[1], &first));
}

#[test]
fn the_bytes_read_as_ranges_are_counted_through_a_download_that_fails() {
    let download_states = download_states();
    assert!(!download_states.count_bytes_read("A", 60, 1_000));
    download_states.start_or_join("A", None);
    download_states.begin_writing("A", unnamed_file());
    // Counted while it runs, too.
    assert!(!download_states.count_bytes_read("A", 30, 1_000));
    download_states.finish("A", Err(libc::EIO));
    // The next read asks for the whole file again.
    assert!(download_states.count_bytes_read("A", 10, 1_000));
}

#[test]
fn the_bytes_read_are_kept_when_the_unfinished_file_is_let_go() {
    let download_states = download_states();
    download_states.count_bytes_read("F0", 99, 1_000);
    (0..=UNFINISHED_FILES_KEPT).for_each(|n| drop(fail_download(&download_states, &format!("F{n}"))));
    assert!(download_states.count_bytes_read("F0", 1, 1_000));
}

#[test]
fn each_files_bytes_are_counted_apart() {
    let download_states = download_states();
    assert!(!download_states.count_bytes_read("A", 99, 1_000));
    assert!(!download_states.count_bytes_read("B", 99, 1_000));
    assert!(download_states.count_bytes_read("A", 1, 1_000));
}

#[test]
fn the_whole_file_is_downloaded_once_the_threshold_was_read() {
    assert!(!reaches_download_threshold(99, 100, 1_000));
    assert!(reaches_download_threshold(100, 100, 1_000));
    assert!(reaches_download_threshold(150, 100, 1_000));
}

#[test]
fn a_file_smaller_than_the_threshold_is_downloaded_once_all_of_it_was_read() {
    assert!(!reaches_download_threshold(39, 100, 40));
    assert!(reaches_download_threshold(40, 100, 40));
    // An empty file has nothing to read first.
    assert!(reaches_download_threshold(0, 100, 0));
}

#[test]
fn an_abandoned_download_fails_its_waiters_and_keeps_no_file() {
    let download_states = download_states();
    let (waiter, told) = oneshot::channel();
    download_states.count_bytes_read("A", 100, 1_000);
    download_states.start_or_join("A", Some(waiter));
    download_states.begin_writing("A", unnamed_file());
    download_states.abandon("A");
    assert_eq!(told.blocking_recv(), Ok(Err(libc::EIO)));
    assert!(!download_states.is_downloading("A"));
    assert!(download_states.list_unfinished_files().is_empty());
    assert!(download_states.count_bytes_read("A", 0, 1_000));
}

/// A download that ended is not undone by the guard of the task that ran it.
#[test]
fn abandoning_a_download_that_ended_changes_nothing() {
    let download_states = download_states();
    fail_download(&download_states, "A");
    download_states.abandon("A");
    assert_eq!(download_states.list_unfinished_files().len(), 1);
    download_states.abandon("never asked for");
    download_states.finish("never asked for", Ok(()));
    assert!(!download_states.is_downloading("never asked for"));
}

#[test]
fn the_mounts_stop_fails_whoever_waits_and_the_download_still_ends() {
    let download_states = download_states();
    let ((first, first_told), (second, second_told)) = (oneshot::channel(), oneshot::channel());
    download_states.start_or_join("A", Some(first));
    download_states.start_or_join("B", Some(second));
    download_states.begin_writing("A", unnamed_file());
    download_states.fail_waiters();
    assert_eq!((first_told.blocking_recv(), second_told.blocking_recv()), (Ok(Err(libc::EIO)), Ok(Err(libc::EIO))));
    download_states.finish("A", Err(libc::EIO));
    assert_eq!(download_states.list_unfinished_files().len(), 1);
}

#[test]
fn a_file_being_downloaded_is_shown_as_hydrating_whatever_is_on_disk() {
    let file = row("A", Some("R"), "a.txt", Kind::File);
    assert_eq!(to_hydration_state(&FileSyncState::RemoteOnly(file.clone()), true), Some(State::Hydrating));
    assert_eq!(to_hydration_state(&FileSyncState::Synced(file.clone(), copy()), true), Some(State::Hydrating));
    assert_eq!(to_hydration_state(&FileSyncState::ModifiedHere(file, copy()), true), Some(State::Hydrating));
}

#[test]
fn a_file_with_nothing_on_disk_is_shown_as_online_only() {
    let file = row("A", Some("R"), "a.txt", Kind::File);
    assert_eq!(to_hydration_state(&FileSyncState::RemoteOnly(file), false), Some(State::OnlineOnly));
}

#[test]
fn a_file_with_its_downloaded_copy_is_shown_as_hydrated_also_when_modified_here() {
    let file = row("A", Some("R"), "a.txt", Kind::File);
    assert_eq!(to_hydration_state(&FileSyncState::Synced(file.clone(), copy()), false), Some(State::Hydrated));
    assert_eq!(to_hydration_state(&FileSyncState::ModifiedHere(file, copy()), false), Some(State::Hydrated));
}

#[test]
fn a_local_only_file_has_no_state_to_show() {
    let on_disk = DiskEntry { handle: FileHandle { kind: 1, bytes: vec![1, 0, 0, 0, 1, 0, 0, 0] }, metadata: copy() };
    let local_only = FileSyncState::LocalOnly(LocalOnlyEntry { on_disk, place: FolderPlace::new(Some("R"), Path::new(""), "own") });
    assert_eq!(to_hydration_state(&local_only, false), None);
    assert_eq!(to_hydration_state(&local_only, true), None);
}
