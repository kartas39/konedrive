use std::time::UNIX_EPOCH;

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::State;
use konedrive_tree::Kind;

use super::to_state_attribute;
use crate::fuse::backing_directory::{DiskEntry, DiskMetadata, EntryKind};
use crate::fuse::directory_contents::tests::row;
use crate::fuse::directory_contents::FileSyncState;
use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::entry_place::EntryPlace;
use crate::fuse::local_only_entry::LocalOnlyEntry;

fn copy() -> DiskMetadata {
    DiskMetadata { kind: EntryKind::File, size: 3, blocks: 8, modified: UNIX_EPOCH }
}

#[test]
fn a_file_being_downloaded_is_shown_as_downloading_whatever_is_on_disk() {
    let file = row("A", Some("R"), "a.txt", Kind::File);
    assert_eq!(to_state_attribute(&FileSyncState::RemoteOnly(file.clone()), true), Some(State::Hydrating));
    assert_eq!(to_state_attribute(&FileSyncState::Synced(file.clone(), copy()), true), Some(State::Hydrating));
    assert_eq!(to_state_attribute(&FileSyncState::ModifiedHere(file, copy()), true), Some(State::Hydrating));
}

#[test]
fn a_file_with_nothing_on_disk_is_shown_as_online_only() {
    let file = row("A", Some("R"), "a.txt", Kind::File);
    assert_eq!(to_state_attribute(&FileSyncState::RemoteOnly(file), false), Some(State::OnlineOnly));
}

#[test]
fn a_file_whose_downloaded_copy_matches_is_shown_as_downloaded() {
    let file = row("A", Some("R"), "a.txt", Kind::File);
    assert_eq!(to_state_attribute(&FileSyncState::Synced(file, copy()), false), Some(State::Hydrated));
}

#[test]
fn a_file_modified_here_is_shown_as_downloaded() {
    let file = row("A", Some("R"), "a.txt", Kind::File);
    assert_eq!(to_state_attribute(&FileSyncState::ModifiedHere(file, copy()), false), Some(State::Hydrated));
}

#[test]
fn a_local_only_file_has_no_state_to_show() {
    let on_disk = DiskEntry { handle: FileHandle { kind: 1, bytes: vec![1, 0, 0, 0, 1, 0, 0, 0] }, metadata: copy() };
    let local_only = FileSyncState::LocalOnly(LocalOnlyEntry { on_disk, place: EntryPlace { directory: DirectoryReference { onedrive_id: Some("R".into()), path: "".into() }, name: "own".into() } });
    assert_eq!(to_state_attribute(&local_only, false), None);
    assert_eq!(to_state_attribute(&local_only, true), None);
}
