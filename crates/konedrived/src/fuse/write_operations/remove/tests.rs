use crate::fuse::write_operations::EntryToMake;
use crate::fuse::backing_directory::EntryKind;
use fuser::Errno;
use konedrive_tree::outbox::OutboxKind;

use crate::fuse::node_table::NodeId;
use crate::fuse::write_operations::tests::{at, folder, folder_that_records_nothing};

#[test]
fn a_onedrive_file_removed_is_deleted_in_onedrive_and_its_copy_is_removed() {
    let f = folder(Vec::new());
    f.download("a.txt", "A", b"a");
    f.ops.remove(NodeId::ROOT, "a.txt", EntryKind::File).unwrap();
    assert!(!f.disk.join("a.txt").exists());
    assert_eq!(f.said(), [(OutboxKind::Delete, Some("A".into()), at("a.txt"))]);
    assert_eq!(f.shown("a.txt"), None);
}

/// Never sent: its create is taken back, and its node is gone.
#[test]
fn a_local_only_file_removed_is_forgotten_by_the_outbox() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "new.txt", EntryToMake::File).unwrap();
    let node_id = f.node_id(NodeId::ROOT, "new.txt");
    f.ops.remove(NodeId::ROOT, "new.txt", EntryKind::File).unwrap();
    assert!(!f.disk.join("new.txt").exists() && f.rows().is_empty());
    assert!(f.node_resolver.find_entry(node_id).is_none() && f.node_resolver.find_parent(node_id).is_none());
}

#[test]
fn what_cannot_be_removed_is_refused() {
    let f = folder(Vec::new());
    assert_eq!(f.ops.remove(NodeId::ROOT, "none.txt", EntryKind::File).err(), Some(Errno::ENOENT));
    assert_eq!(f.ops.remove(NodeId::ROOT, "docs", EntryKind::File).err(), Some(Errno::EISDIR));
    assert_eq!(f.ops.remove(NodeId::ROOT, "a.txt", EntryKind::Directory).err(), Some(Errno::ENOTDIR));
    assert_eq!(f.ops.remove(NodeId::ROOT, "docs", EntryKind::Directory).err(), Some(Errno::ENOTEMPTY));
    assert!(f.rows().is_empty());
}

/// The directory is removed in OneDrive by the outbox, after what it held; the directory the
/// backing directory has for it goes at once.
#[test]
fn a_onedrive_directory_that_holds_nothing_is_deleted_in_onedrive_and_removed_from_disk() {
    let f = folder(Vec::new());
    f.directory_contents.make_marked_directories(&at("docs")).unwrap();
    f.ops.remove(f.docs(), "c.txt", EntryKind::File).unwrap();
    f.ops.remove(NodeId::ROOT, "docs", EntryKind::Directory).unwrap();
    assert!(!f.disk.join("docs").exists());
    assert_eq!(f.said(), [(OutboxKind::Delete, Some("C".into()), at("docs/c.txt")), (OutboxKind::Delete, Some("D".into()), at("docs"))]);
    assert_eq!(f.shown("docs"), None);
}

/// Never sent: its row is taken back, the directory is gone from disk, and so is its node.
#[test]
fn a_local_only_directory_removed_is_forgotten_by_the_outbox() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "new", EntryToMake::Directory).unwrap();
    let node_id = f.node_id(NodeId::ROOT, "new");
    f.ops.remove(NodeId::ROOT, "new", EntryKind::Directory).unwrap();
    assert!(!f.disk.join("new").exists() && f.rows().is_empty());
    assert!(f.node_resolver.find_entry(node_id).is_none() && f.node_resolver.find_directory(node_id).is_none());
}

#[test]
fn a_local_only_directory_that_holds_something_is_not_removed() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "new", EntryToMake::Directory).unwrap();
    f.ops.make(f.node_id(NodeId::ROOT, "new"), "x.txt", EntryToMake::File).unwrap();
    let before = f.rows();
    assert_eq!(f.ops.remove(NodeId::ROOT, "new", EntryKind::Directory).err(), Some(Errno::ENOTEMPTY));
    assert!(f.disk.join("new/x.txt").is_file());
    assert_eq!(f.rows(), before);
}

/// A removal is recorded first: one that cannot be recorded removes nothing.
#[test]
fn a_removal_that_cannot_be_recorded_removes_nothing_and_is_answered_as_an_io_error() {
    let f = folder_that_records_nothing();
    f.download("a.txt", "A", b"a");
    assert_eq!(f.ops.remove(NodeId::ROOT, "a.txt", EntryKind::File).err(), Some(Errno::EIO));
    assert!(f.disk.join("a.txt").is_file());
    assert_eq!(f.shown("a.txt"), Some(Some("A".into())));
}

#[test]
fn nothing_is_removed_from_a_read_only_folder() {
    let f = folder(Vec::new());
    f.access_mode_switch.switch_to_read_only();
    assert_eq!(f.ops.remove(NodeId::ROOT, "a.txt", EntryKind::File).err(), Some(Errno::EROFS));
    assert!(f.rows().is_empty());
}

/// The mount shows the directory as holding nothing, and the disk holds a name of the
/// daemon's own in it: it is not removed, and the outbox still holds it.
#[test]
fn a_local_only_directory_that_cannot_be_removed_from_disk_keeps_its_row() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "mine", EntryToMake::Directory).unwrap();
    std::fs::write(f.disk.join("mine").join(format!("{}x", crate::folder::disk::NEW_PREFIX)), b"").unwrap();
    assert_eq!(f.said(), [(OutboxKind::Mkdir, None, at("mine"))]);
    assert_eq!(f.ops.remove(NodeId::ROOT, "mine", EntryKind::Directory).err(), Some(Errno::ENOTEMPTY));
    assert!(f.disk.join("mine").is_dir());
    assert_eq!(f.said(), [(OutboxKind::Mkdir, None, at("mine"))]);
    assert!(f.node_resolver.find_entry(f.node_id(NodeId::ROOT, "mine")).is_some());
}
