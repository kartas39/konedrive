use std::time::UNIX_EPOCH;

use konedrive_fs::placeholder::{XATTR_ITEM_ID, XATTR_PIN, XATTR_ROOT, XATTR_STATE, XATTR_SYNC};
use konedrive_tree::Kind;

use super::to_xattr_value;
use crate::fuse::backing_directory::{DiskMetadata, EntryKind};
use crate::fuse::detections::tests::{file, in_root, local_only};
use crate::fuse::directory_contents::tests::row;
use crate::fuse::directory_contents::{DirectoryEntry, DirectorySyncState, EntryState, FileSyncState};

fn copy() -> DiskMetadata {
    DiskMetadata { kind: EntryKind::File, size: 1, blocks: 8, modified: UNIX_EPOCH }
}

fn remote_only() -> DirectoryEntry {
    DirectoryEntry { name: "a.txt".into(), state: EntryState::File(FileSyncState::RemoteOnly(row("A", Some("R"), "a.txt", Kind::File))) }
}

fn synced() -> DirectoryEntry {
    DirectoryEntry { name: "a.txt".into(), state: EntryState::File(FileSyncState::Synced(row("A", Some("R"), "a.txt", Kind::File), copy())) }
}

fn modified_here() -> DirectoryEntry {
    DirectoryEntry { name: "a.txt".into(), state: EntryState::File(FileSyncState::ModifiedHere(row("A", Some("R"), "a.txt", Kind::File), copy())) }
}

fn directory() -> DirectoryEntry {
    DirectoryEntry { name: "docs".into(), state: EntryState::Directory(DirectorySyncState::InOneDrive(row("D", Some("R"), "docs", Kind::Folder))) }
}

fn local_only_file() -> DirectoryEntry {
    DirectoryEntry::from_local_only(local_only(&file(1, 3), &in_root("new.txt")))
}

#[test]
fn what_is_in_onedrive_shows_its_id() {
    assert_eq!(to_xattr_value(XATTR_ITEM_ID, &remote_only(), false, false).as_deref(), Some("A"));
    assert_eq!(to_xattr_value(XATTR_ITEM_ID, &directory(), false, false).as_deref(), Some("D"));
}

#[test]
fn a_file_shows_its_state_by_its_sync_state_and_its_download() {
    assert_eq!(to_xattr_value(XATTR_STATE, &remote_only(), false, false).as_deref(), Some("online-only"));
    assert_eq!(to_xattr_value(XATTR_STATE, &synced(), false, false).as_deref(), Some("hydrated"));
    assert_eq!(to_xattr_value(XATTR_STATE, &modified_here(), false, false).as_deref(), Some("hydrated"));
    assert_eq!(to_xattr_value(XATTR_STATE, &remote_only(), false, true).as_deref(), Some("hydrating"));
}

#[test]
fn a_directory_shows_no_state() {
    assert_eq!(to_xattr_value(XATTR_STATE, &directory(), false, false), None);
    assert_eq!(to_xattr_value(XATTR_STATE, &directory(), true, true), None);
}

#[test]
fn only_what_carries_a_pin_of_its_own_shows_one() {
    assert_eq!(to_xattr_value(XATTR_PIN, &remote_only(), true, false).as_deref(), Some("1"));
    assert_eq!(to_xattr_value(XATTR_PIN, &directory(), true, false).as_deref(), Some("1"));
    assert_eq!(to_xattr_value(XATTR_PIN, &remote_only(), false, false), None);
}

#[test]
fn a_local_only_entry_shows_none_of_them() {
    for name in [XATTR_ITEM_ID, XATTR_STATE, XATTR_PIN] {
        assert_eq!(to_xattr_value(name, &local_only_file(), true, true), None);
    }
}

/// The folder's own attributes and the upload mark are read from the backing directory.
#[test]
fn an_attribute_that_is_not_the_stores_has_no_value_here() {
    for name in [XATTR_ROOT, XATTR_SYNC, "user.other"] {
        assert_eq!(to_xattr_value(name, &synced(), true, true), None);
    }
}
