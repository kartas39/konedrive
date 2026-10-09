use crate::fuse::write_operations::EntryToMake;
use crate::fuse::write_operations::RenameMode;
use fuser::Errno;
use konedrive_tree::outbox::OutboxKind;

use crate::fuse::node_table::NodeId;
use crate::fuse::directory_contents::FileSyncState;
use crate::fuse::write_operations::tests::{at, folder};

#[test]
fn a_onedrive_file_renamed_is_moved_with_its_copy() {
    let f = folder(Vec::new());
    f.download("a.txt", "A", b"a");
    f.ops.rename(NodeId::ROOT, "a.txt", NodeId::ROOT, "new.txt", RenameMode::Replace).unwrap();
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
    f.ops.rename(NodeId::ROOT, "a.txt", f.docs(), "a.txt", RenameMode::Replace).unwrap();
    assert_eq!(f.said(), [(OutboxKind::Move, Some("A".into()), at("docs/a.txt"))]);
    assert_eq!(f.rows()[0].target_parent.as_deref(), Some("D"));
    assert_eq!(f.shown("docs/a.txt"), Some(Some("A".into())));
}

#[test]
fn a_onedrive_file_renamed_over_another_deletes_that_one() {
    let f = folder(Vec::new());
    f.ops.rename(NodeId::ROOT, "a.txt", NodeId::ROOT, "b.txt", RenameMode::Replace).unwrap();
    let said = f.said();
    assert!(said.contains(&(OutboxKind::Delete, Some("B".into()), at("b.txt"))) && said.contains(&(OutboxKind::Move, Some("A".into()), at("b.txt"))));
    assert_eq!(f.shown("b.txt"), Some(Some("A".into())));
}

/// Save by a temporary file: the file is the downloaded copy of the OneDrive file
/// afterwards, modified here; its own create is gone, and its node stands for that file.
#[test]
fn a_local_only_file_saved_over_a_onedrive_file_is_that_files_new_content() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "a.new", EntryToMake::File).unwrap();
    let node_id = f.node_id(NodeId::ROOT, "a.new");
    f.ops.rename(NodeId::ROOT, "a.new", NodeId::ROOT, "a.txt", RenameMode::Replace).unwrap();
    assert!(f.backing_directory.find_downloaded_copy(&at("a.txt"), "A").is_some());
    assert_eq!(f.said(), [(OutboxKind::Update, Some("A".into()), at("a.txt"))]);
    assert_eq!(f.node_resolver.find_node_id("A"), Some(node_id));
    assert_eq!(f.shown("a.txt"), Some(Some("A".into())));
    assert!(matches!(f.file_state("a.txt"), FileSyncState::ModifiedHere(..)));
}

#[test]
fn a_local_only_file_moved_into_an_ignored_directory_is_no_longer_uploaded() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "cache", EntryToMake::Directory).unwrap();
    f.ops.make(NodeId::ROOT, "x.txt", EntryToMake::File).unwrap();
    let cache = f.node_id(NodeId::ROOT, "cache");
    f.ops.rename(NodeId::ROOT, "x.txt", cache, "x.txt", RenameMode::Replace).unwrap();
    assert!(f.disk.join("cache/x.txt").is_file());
    assert!(!f.rows().iter().any(|row| row.kind == OutboxKind::Create));
}

/// A program that copies it there and removes it here does what that means.
#[test]
fn a_onedrive_file_cannot_be_moved_where_nothing_is_uploaded() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "cache", EntryToMake::Directory).unwrap();
    let cache = f.node_id(NodeId::ROOT, "cache");
    assert_eq!(f.ops.rename(NodeId::ROOT, "a.txt", cache, "a.txt", RenameMode::Replace).err(), Some(Errno::EXDEV));
    assert!(f.rows().is_empty());
}

#[test]
fn a_local_only_directory_moved_takes_the_rows_below_it_along() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "new", EntryToMake::Directory).unwrap();
    let new = f.node_id(NodeId::ROOT, "new");
    f.ops.make(new, "x.txt", EntryToMake::File).unwrap();
    f.ops.rename(NodeId::ROOT, "new", f.docs(), "new", RenameMode::Replace).unwrap();
    assert_eq!(f.said(), [(OutboxKind::Mkdir, None, at("docs/new")), (OutboxKind::Create, None, at("docs/new/x.txt"))]);
    assert_eq!(f.rows()[0].target_parent.as_deref(), Some("D"));
    assert!(f.disk.join("docs/new/x.txt").is_file());
}

#[test]
fn what_cannot_be_at_the_new_place_is_refused() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "empty", EntryToMake::Directory).unwrap();
    let before = f.rows();
    assert_eq!(f.ops.rename(NodeId::ROOT, "empty", NodeId::ROOT, "docs", RenameMode::Replace).err(), Some(Errno::ENOTEMPTY));
    assert_eq!(f.ops.rename(NodeId::ROOT, "a.txt", NodeId::ROOT, "b.txt", RenameMode::NoReplace).err(), Some(Errno::EEXIST));
    assert_eq!(f.ops.rename(NodeId::ROOT, "a.txt", NodeId::ROOT, "docs", RenameMode::Replace).err(), Some(Errno::EISDIR));
    assert_eq!(f.ops.rename(NodeId::ROOT, "docs", NodeId::ROOT, "a.txt", RenameMode::Replace).err(), Some(Errno::ENOTDIR));
    assert_eq!(f.ops.rename(NodeId::ROOT, "a.txt", NodeId::ROOT, crate::folder::disk::HOLDING, RenameMode::Replace).err(), Some(Errno::EPERM));
    assert_eq!(f.ops.rename(NodeId::ROOT, "none.txt", NodeId::ROOT, "x.txt", RenameMode::Replace).err(), Some(Errno::ENOENT));
    assert_eq!(f.rows(), before);
}
