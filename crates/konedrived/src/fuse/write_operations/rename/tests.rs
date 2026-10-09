use fuser::Errno;
use konedrive_tree::outbox::OutboxKind;

use crate::fuse::directory_contents::FileSyncState;
use crate::fuse::write_operations::tests::{at, folder};

#[test]
fn a_onedrive_file_renamed_is_moved_with_its_copy() {
    let f = folder(Vec::new());
    f.download("a.txt", "A", b"a");
    f.ops.rename(1, "a.txt", 1, "new.txt", false).unwrap();
    assert!(f.disk.join("new.txt").is_file() && !f.disk.join("a.txt").exists());
    assert_eq!(f.said(), [(OutboxKind::Move, Some("A".into()), at("new.txt"))]);
    assert!(f.rows()[0].object.is_some());
    assert_eq!((f.shown("new.txt"), f.shown("a.txt")), (Some(Some("A".into())), None));
    // Moved, not changed: its copy still matches.
    assert!(matches!(f.file_state("new.txt"), FileSyncState::Synced(..)));
}

#[test]
fn a_onedrive_file_moved_to_another_directory_is_there_at_once() {
    let f = folder(Vec::new());
    f.ops.rename(1, "a.txt", f.docs(), "a.txt", false).unwrap();
    assert_eq!(f.said(), [(OutboxKind::Move, Some("A".into()), at("docs/a.txt"))]);
    assert_eq!(f.rows()[0].target_parent.as_deref(), Some("D"));
    assert_eq!(f.shown("docs/a.txt"), Some(Some("A".into())));
}

#[test]
fn a_onedrive_file_renamed_over_another_deletes_that_one() {
    let f = folder(Vec::new());
    f.ops.rename(1, "a.txt", 1, "b.txt", false).unwrap();
    let said = f.said();
    assert!(said.contains(&(OutboxKind::Delete, Some("B".into()), at("b.txt"))) && said.contains(&(OutboxKind::Move, Some("A".into()), at("b.txt"))));
    assert_eq!(f.shown("b.txt"), Some(Some("A".into())));
}

/// Save by a temporary file: the file is the downloaded copy of the OneDrive file
/// afterwards, modified here; its own create is gone, and its node stands for that file.
#[test]
fn a_local_only_file_saved_over_a_onedrive_file_is_that_files_new_content() {
    let f = folder(Vec::new());
    f.ops.make(1, "a.new", false, false).unwrap();
    let node_id = f.node_id(1, "a.new");
    f.ops.rename(1, "a.new", 1, "a.txt", false).unwrap();
    assert!(f.backing_directory.has_downloaded_copy(&at("a.txt"), "A"));
    assert_eq!(f.said(), [(OutboxKind::Update, Some("A".into()), at("a.txt"))]);
    assert_eq!(f.node_resolver.find_node_id("A"), Some(node_id));
    assert_eq!(f.shown("a.txt"), Some(Some("A".into())));
    assert!(matches!(f.file_state("a.txt"), FileSyncState::ModifiedHere(..)));
}

#[test]
fn a_local_only_file_moved_into_an_ignored_directory_is_no_longer_uploaded() {
    let f = folder(Vec::new());
    f.ops.make(1, "cache", true, false).unwrap();
    f.ops.make(1, "x.txt", false, false).unwrap();
    let cache = f.node_id(1, "cache");
    f.ops.rename(1, "x.txt", cache, "x.txt", false).unwrap();
    assert!(f.disk.join("cache/x.txt").is_file());
    assert!(!f.rows().iter().any(|row| row.kind == OutboxKind::Create));
}

/// A program that copies it there and removes it here does what that means.
#[test]
fn a_onedrive_file_cannot_be_moved_where_nothing_is_uploaded() {
    let f = folder(Vec::new());
    f.ops.make(1, "cache", true, false).unwrap();
    let cache = f.node_id(1, "cache");
    assert_eq!(f.ops.rename(1, "a.txt", cache, "a.txt", false).err(), Some(Errno::EXDEV));
    assert!(f.rows().is_empty());
}

#[test]
fn a_local_only_directory_moved_takes_the_rows_below_it_along() {
    let f = folder(Vec::new());
    f.ops.make(1, "new", true, false).unwrap();
    let new = f.node_id(1, "new");
    f.ops.make(new, "x.txt", false, false).unwrap();
    f.ops.rename(1, "new", f.docs(), "new", false).unwrap();
    assert_eq!(f.said(), [(OutboxKind::Mkdir, None, at("docs/new")), (OutboxKind::Create, None, at("docs/new/x.txt"))]);
    assert_eq!(f.rows()[0].target_parent.as_deref(), Some("D"));
    assert!(f.disk.join("docs/new/x.txt").is_file());
}

#[test]
fn what_cannot_be_at_the_new_place_is_refused() {
    let f = folder(Vec::new());
    f.ops.make(1, "empty", true, false).unwrap();
    let before = f.rows();
    assert_eq!(f.ops.rename(1, "empty", 1, "docs", false).err(), Some(Errno::ENOTEMPTY));
    assert_eq!(f.ops.rename(1, "a.txt", 1, "b.txt", true).err(), Some(Errno::EEXIST));
    assert_eq!(f.ops.rename(1, "a.txt", 1, "docs", false).err(), Some(Errno::EISDIR));
    assert_eq!(f.ops.rename(1, "docs", 1, "a.txt", false).err(), Some(Errno::ENOTDIR));
    assert_eq!(f.ops.rename(1, "a.txt", 1, crate::folder::disk::HOLDING, false).err(), Some(Errno::EPERM));
    assert_eq!(f.ops.rename(1, "none.txt", 1, "x.txt", false).err(), Some(Errno::ENOENT));
    assert_eq!(f.rows(), before);
}
