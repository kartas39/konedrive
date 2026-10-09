use std::time::UNIX_EPOCH;

use konedrive_tree::Kind;

use super::{decide_free_up, FreeUpDecision};
use crate::fuse::backing_directory::{DiskMetadata, EntryKind};
use crate::fuse::detections::tests::{file, in_root, local_only};
use crate::fuse::directory_contents::tests::row;
use crate::fuse::directory_contents::{DirectorySyncState, EntryState, FileSyncState};

fn copy() -> DiskMetadata {
    DiskMetadata { kind: EntryKind::File, size: 1, blocks: 8, modified: UNIX_EPOCH }
}

fn synced() -> EntryState {
    EntryState::File(FileSyncState::Synced(row("A", Some("R"), "a.txt", Kind::File), copy()))
}

fn modified_here() -> EntryState {
    EntryState::File(FileSyncState::ModifiedHere(row("A", Some("R"), "a.txt", Kind::File), copy()))
}

fn remote_only() -> EntryState {
    EntryState::File(FileSyncState::RemoteOnly(row("A", Some("R"), "a.txt", Kind::File)))
}

fn directory() -> EntryState {
    EntryState::Directory(DirectorySyncState::InOneDrive(row("D", Some("R"), "docs", Kind::Folder)))
}

#[test]
fn a_synced_file_that_nothing_keeps_is_freed() {
    assert_eq!(decide_free_up(&synced(), false, false, false), FreeUpDecision::Free);
}

#[test]
fn a_pinned_file_is_left_whatever_else_holds() {
    for state in [synced(), modified_here(), remote_only(), directory()] {
        assert_eq!(decide_free_up(&state, true, false, false), FreeUpDecision::LeavePinned);
        assert_eq!(decide_free_up(&state, true, true, true), FreeUpDecision::LeavePinned);
    }
}

#[test]
fn a_file_modified_here_is_left() {
    assert_eq!(decide_free_up(&modified_here(), false, false, false), FreeUpDecision::LeaveModifiedHere);
    // Its writer is not what keeps it: the change that is not sent is.
    assert_eq!(decide_free_up(&modified_here(), false, true, true), FreeUpDecision::LeaveModifiedHere);
}

/// The outbox's row was dropped under the writer: the sync state no longer says the file is
/// modified here, and the open handles still say it is written.
#[test]
fn a_file_open_for_writing_is_left() {
    assert_eq!(decide_free_up(&synced(), false, false, true), FreeUpDecision::LeaveOpenForWriting);
    assert_eq!(decide_free_up(&synced(), false, true, true), FreeUpDecision::LeaveOpenForWriting);
}

#[test]
fn a_file_being_downloaded_is_left() {
    assert_eq!(decide_free_up(&remote_only(), false, true, false), FreeUpDecision::LeaveDownloading);
    assert_eq!(decide_free_up(&synced(), false, true, false), FreeUpDecision::LeaveDownloading);
}

#[test]
fn what_has_no_downloaded_copy_has_nothing_to_free() {
    assert_eq!(decide_free_up(&remote_only(), false, false, false), FreeUpDecision::NothingToFree);
    assert_eq!(decide_free_up(&directory(), false, false, false), FreeUpDecision::NothingToFree);
    let local_only = EntryState::File(FileSyncState::LocalOnly(local_only(&file(1, 3), &in_root("new.txt"))));
    assert_eq!(decide_free_up(&local_only, false, false, false), FreeUpDecision::NothingToFree);
}
