use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use fuser::Errno;
use konedrive_tree::Kind;

use super::{plan_remove, RemovePlace, RemovePlan};
use crate::fuse::backing_directory::{DiskMetadata, EntryKind};
use crate::fuse::detections::tests::{directory, file, handle, in_root, local_only};
use crate::fuse::directory_contents::tests::row;
use crate::fuse::directory_contents::{DirectoryEntry, DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::directory_reference::DirectoryReference;

fn root() -> DirectoryReference {
    DirectoryReference { onedrive_id: Some("R".into()), path: PathBuf::new() }
}

fn copy() -> DiskMetadata {
    DiskMetadata { kind: EntryKind::File, size: 1, blocks: 8, modified: UNIX_EPOCH }
}

fn onedrive_file(state: fn(konedrive_tree::Row) -> FileSyncState) -> DirectoryEntry {
    DirectoryEntry { name: "a.txt".into(), state: EntryState::File(state(row("A", Some("R"), "a.txt", Kind::File))) }
}

fn onedrive_directory() -> DirectoryEntry {
    DirectoryEntry { name: "docs".into(), state: EntryState::Directory(DirectorySyncState::InOneDrive(row("D", Some("R"), "docs", Kind::Folder))) }
}

fn local_only_file() -> DirectoryEntry {
    DirectoryEntry::from_local_only(local_only(&file(1, 3), &in_root("new.txt")))
}

fn local_only_directory() -> DirectoryEntry {
    DirectoryEntry::from_local_only(local_only(&directory(2), &in_root("new")))
}

fn holding(entry: DirectoryEntry) -> RemovePlace {
    RemovePlace { parent_directory: Some(root()), entry: Some(entry), holds_entries: false }
}

#[test]
fn a_onedrive_file_is_removed_by_its_id_whatever_its_sync_state() {
    let states: [fn(konedrive_tree::Row) -> FileSyncState; 3] = [FileSyncState::RemoteOnly, |row| FileSyncState::Synced(row, copy()), |row| FileSyncState::ModifiedHere(row, copy())];
    for state in states {
        let plan = plan_remove(EntryKind::File, "a.txt", holding(onedrive_file(state)));
        assert_eq!(plan, Ok(RemovePlan::OneDriveFile { onedrive_id: "A".into(), path: "a.txt".into() }));
    }
}

#[test]
fn a_onedrive_directory_that_holds_nothing_is_removed_by_its_id() {
    let plan = plan_remove(EntryKind::Directory, "docs", holding(onedrive_directory()));
    assert_eq!(plan, Ok(RemovePlan::OneDriveDirectory { onedrive_id: "D".into(), path: "docs".into() }));
}

#[test]
fn a_local_only_file_is_removed_by_its_file_handle() {
    let plan = plan_remove(EntryKind::File, "new.txt", holding(local_only_file()));
    assert_eq!(plan, Ok(RemovePlan::LocalOnlyFile { handle: handle(1), path: "new.txt".into() }));
}

#[test]
fn a_local_only_directory_is_removed_by_its_file_handle() {
    let plan = plan_remove(EntryKind::Directory, "new", holding(local_only_directory()));
    assert_eq!(plan, Ok(RemovePlan::LocalOnlyDirectory { handle: handle(2), path: "new".into() }));
}

#[test]
fn the_path_is_the_name_in_the_directory_the_node_stands_for() {
    let docs = DirectoryReference { onedrive_id: Some("D".into()), path: PathBuf::from("docs") };
    let place = RemovePlace { parent_directory: Some(docs), entry: Some(local_only_file()), holds_entries: false };
    assert_eq!(plan_remove(EntryKind::File, "new.txt", place), Ok(RemovePlan::LocalOnlyFile { handle: handle(1), path: "docs/new.txt".into() }));
}

#[test]
fn a_node_that_stands_for_no_directory_and_a_name_the_directory_does_not_hold_are_no_entry() {
    let no_directory = RemovePlace { parent_directory: None, entry: Some(local_only_file()), holds_entries: false };
    assert_eq!(plan_remove(EntryKind::File, "new.txt", no_directory), Err(Errno::ENOENT));
    let no_entry = RemovePlace { parent_directory: Some(root()), entry: None, holds_entries: false };
    assert_eq!(plan_remove(EntryKind::File, "none.txt", no_entry), Err(Errno::ENOENT));
}

#[test]
fn a_directory_is_not_removed_as_a_file() {
    assert_eq!(plan_remove(EntryKind::File, "docs", holding(onedrive_directory())), Err(Errno::EISDIR));
    assert_eq!(plan_remove(EntryKind::File, "new", holding(local_only_directory())), Err(Errno::EISDIR));
}

#[test]
fn a_file_is_not_removed_as_a_directory() {
    assert_eq!(plan_remove(EntryKind::Directory, "a.txt", holding(onedrive_file(FileSyncState::RemoteOnly))), Err(Errno::ENOTDIR));
    assert_eq!(plan_remove(EntryKind::Directory, "new.txt", holding(local_only_file())), Err(Errno::ENOTDIR));
}

#[test]
fn a_directory_that_holds_something_is_not_removed() {
    for entry in [onedrive_directory(), local_only_directory()] {
        let place = RemovePlace { holds_entries: true, ..holding(entry) };
        assert_eq!(plan_remove(EntryKind::Directory, "docs", place), Err(Errno::ENOTEMPTY));
    }
}

/// The kind is told before the directory is looked into.
#[test]
fn a_directory_that_holds_something_and_is_removed_as_a_file_is_a_directory() {
    let place = RemovePlace { holds_entries: true, ..holding(onedrive_directory()) };
    assert_eq!(plan_remove(EntryKind::File, "docs", place), Err(Errno::EISDIR));
}
