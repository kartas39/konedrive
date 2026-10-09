use std::time::UNIX_EPOCH;

use konedrive_fs::handle::FileHandle;
use konedrive_tree::Kind;

use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::entry_place::EntryPlace;
use crate::fuse::local_only_entry::LocalOnlyEntry;
use super::{DirectoryEntry, DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::backing_directory::{DiskEntry, DiskMetadata, EntryKind};
use crate::fuse::directory_contents::tests::row;

fn copy() -> DiskMetadata {
    DiskMetadata { kind: EntryKind::File, size: 10, blocks: 8, modified: UNIX_EPOCH }
}

fn local_only(kind: EntryKind) -> LocalOnlyEntry {
    let on_disk = DiskEntry { handle: FileHandle { kind: 1, bytes: vec![1, 0, 0, 0, 1, 0, 0, 0] }, metadata: DiskMetadata { kind, ..copy() } };
    LocalOnlyEntry { on_disk, place: EntryPlace { directory: DirectoryReference { onedrive_id: Some("R".into()), path: "".into() }, name: "own".into() } }
}

#[test]
fn a_onedrive_file_without_a_copy_is_remote_only_whatever_the_outbox_holds() {
    let file = row("A", Some("R"), "a.txt", Kind::File);
    assert_eq!(FileSyncState::of_onedrive_file(file.clone(), None, false), FileSyncState::RemoteOnly(file.clone()));
    assert_eq!(FileSyncState::of_onedrive_file(file.clone(), None, true), FileSyncState::RemoteOnly(file));
}

#[test]
fn a_onedrive_file_with_a_copy_is_synced_until_its_content_is_changed_here() {
    let file = row("A", Some("R"), "a.txt", Kind::File);
    assert_eq!(FileSyncState::of_onedrive_file(file.clone(), Some(copy()), false), FileSyncState::Synced(file.clone(), copy()));
    assert_eq!(FileSyncState::of_onedrive_file(file.clone(), Some(copy()), true), FileSyncState::ModifiedHere(file, copy()));
}

#[test]
fn a_state_carries_the_row_and_the_copy_only_where_they_exist() {
    let file = row("A", Some("R"), "a.txt", Kind::File);
    let remote_only = FileSyncState::RemoteOnly(file.clone());
    assert!(remote_only.row().is_some() && remote_only.downloaded_copy().is_none());
    let synced = FileSyncState::Synced(file.clone(), copy());
    assert!(synced.row().is_some() && synced.downloaded_copy() == Some(&copy()));
    let modified_here = FileSyncState::ModifiedHere(file, copy());
    assert!(modified_here.row().is_some() && modified_here.downloaded_copy() == Some(&copy()));
    let local = FileSyncState::LocalOnly(local_only(EntryKind::File));
    assert!(local.row().is_none() && local.downloaded_copy().is_none());
}

#[test]
fn a_local_only_entry_is_a_file_or_a_directory_by_what_it_is_on_disk() {
    let file = DirectoryEntry::from_local_only(local_only(EntryKind::File));
    assert!(file.name == "own" && matches!(file.state, EntryState::File(FileSyncState::LocalOnly(_))));
    assert!(file.onedrive_id().is_none() && file.local_only().is_some());
    let directory = DirectoryEntry::from_local_only(local_only(EntryKind::Directory));
    assert!(matches!(directory.state, EntryState::Directory(DirectorySyncState::LocalOnly(_))));
}

#[test]
fn an_entry_in_onedrive_has_its_id_and_no_local_only_entry() {
    let docs = row("D", Some("R"), "docs", Kind::Folder);
    let entry = DirectoryEntry { name: "docs".into(), state: EntryState::Directory(DirectorySyncState::InOneDrive(docs)) };
    assert_eq!((entry.onedrive_id(), entry.local_only()), (Some("D"), None));
}
