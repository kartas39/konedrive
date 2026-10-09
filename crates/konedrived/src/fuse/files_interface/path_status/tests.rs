use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use konedrive_fs::placeholder::State;
use konedrive_tree::Kind;

use super::PathStatus;
use crate::fuse::backing_directory::{DiskMetadata, EntryKind};
use crate::fuse::directory_contents::tests::row;
use crate::fuse::directory_contents::{DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::free_up_decision::FreeUpDecision;

fn copy() -> DiskMetadata {
    DiskMetadata { kind: EntryKind::File, size: 1, blocks: 8, modified: UNIX_EPOCH }
}

fn status(entry_state: EntryState) -> PathStatus {
    PathStatus { id: "A".into(), own_pin: false, pinned_above: None, entry_state, is_downloading: false, is_open_for_writing: false }
}

fn synced() -> PathStatus {
    status(EntryState::File(FileSyncState::Synced(row("A", Some("R"), "a.txt", Kind::File), copy())))
}

fn remote_only() -> PathStatus {
    status(EntryState::File(FileSyncState::RemoteOnly(row("A", Some("R"), "a.txt", Kind::File))))
}

fn modified_here() -> PathStatus {
    status(EntryState::File(FileSyncState::ModifiedHere(row("A", Some("R"), "a.txt", Kind::File), copy())))
}

fn directory() -> PathStatus {
    status(EntryState::Directory(DirectorySyncState::InOneDrive(row("D", Some("R"), "docs", Kind::Folder))))
}

#[test]
fn what_is_shown_of_a_file_is_its_state_attribute_and_a_directory_has_none() {
    assert_eq!(synced().state_attribute(), Some(State::Hydrated));
    assert_eq!(remote_only().state_attribute(), Some(State::OnlineOnly));
    assert_eq!(PathStatus { is_downloading: true, ..remote_only() }.state_attribute(), Some(State::Hydrating));
    assert_eq!(directory().state_attribute(), None);
    assert!(directory().is_directory() && !synced().is_directory());
}

#[test]
fn a_file_is_modified_here_by_its_sync_state_or_by_a_writer() {
    assert!(modified_here().is_modified_here());
    assert!(PathStatus { is_open_for_writing: true, ..synced() }.is_modified_here());
    assert!(!synced().is_modified_here() && !directory().is_modified_here());
}

#[test]
fn a_free_up_is_decided_with_the_pin_of_its_own_and_the_one_above() {
    assert_eq!(synced().decide_free_up(), FreeUpDecision::Free);
    assert_eq!(PathStatus { own_pin: true, ..synced() }.decide_free_up(), FreeUpDecision::LeavePinned);
    assert_eq!(PathStatus { pinned_above: Some(PathBuf::from("/f/docs")), ..synced() }.decide_free_up(), FreeUpDecision::LeavePinned);
}

/// `FreeUp` takes a path's own pin off, and then only a directory above keeps it.
#[test]
fn once_its_own_pin_is_off_only_a_pin_above_keeps_a_path() {
    assert_eq!(PathStatus { own_pin: true, ..synced() }.decide_free_up_once_unpinned(), FreeUpDecision::Free);
    assert_eq!(PathStatus { own_pin: true, ..modified_here() }.decide_free_up_once_unpinned(), FreeUpDecision::LeaveModifiedHere);
    let pinned_above = PathStatus { own_pin: true, pinned_above: Some(PathBuf::from("/f/docs")), ..synced() };
    assert_eq!(pinned_above.decide_free_up_once_unpinned(), FreeUpDecision::LeavePinned);
}
