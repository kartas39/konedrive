
use fuser::Errno;
use konedrive_tree::outbox::{OutboxKind, OutboxState, Reason};

use crate::fuse::node_table::NodeId;
use crate::fuse::directory_contents::tests::of;
use crate::fuse::directory_contents::{DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::write_operations::tests::{at, folder};

#[test]
fn a_file_made_is_on_disk_local_only_and_waits_in_the_outbox_to_be_created() {
    let f = folder(Vec::new());
    let (entry, opened) = f.ops.make(NodeId::ROOT, "new.txt", false, false).unwrap();
    assert!(opened.is_none() && f.disk.join("new.txt").is_file());
    assert!(entry.name == "new.txt" && matches!(entry.state, EntryState::File(FileSyncState::LocalOnly(_))));
    let rows = f.rows();
    assert_eq!(f.said(), [(OutboxKind::Create, None, at("new.txt"))]);
    assert_eq!((rows[0].state, rows[0].reason.clone(), rows[0].target_parent.as_deref()), (OutboxState::Waiting, Some(Reason::Changed), Some("R")));
    assert!(matches!(f.file_state("new.txt"), FileSyncState::LocalOnly(_)));
}

#[test]
fn a_file_created_open_is_handed_back_and_waits_for_its_writer() {
    let f = folder(Vec::new());
    let opened = f.ops.make(NodeId::ROOT, "new.txt", false, true).unwrap().1.unwrap();
    assert!(f.open_writers.is_open(opened.node_id) && opened.onedrive_id.is_none() && opened.rel == at("new.txt") && opened.may_write && opened.file_on_disk.is_some());
    assert_eq!(f.rows()[0].reason, Some(Reason::OpenForWriting));
}

#[test]
fn a_directory_made_is_made_in_onedrive_inside_the_directory_it_is_in() {
    let f = folder(Vec::new());
    let (entry, _) = f.ops.make(f.docs(), "new", true, false).unwrap();
    assert!(matches!(entry.state, EntryState::Directory(DirectorySyncState::LocalOnly(_))));
    assert!(f.disk.join("docs/new").is_dir());
    assert_eq!(f.backing_directory.read_onedrive_id(&at("docs")).as_deref(), Some("D"));
    assert_eq!(f.said(), [(OutboxKind::Mkdir, None, at("docs/new"))]);
    assert_eq!(f.rows()[0].target_parent.as_deref(), Some("D"));
}

#[test]
fn what_may_not_be_made_is_refused_and_nothing_is_made() {
    let f = folder(Vec::new());
    assert_eq!(f.ops.make(NodeId::ROOT, "a.txt", false, false).err(), Some(Errno::EEXIST));
    assert_eq!(f.ops.make(NodeId::ROOT, crate::folder::disk::HOLDING, true, false).err(), Some(Errno::EPERM));
    assert_eq!(f.ops.make(NodeId::ROOT, ".Trash-1000", true, false).err(), Some(Errno::EPERM));
    assert_eq!(f.ops.make(NodeId(99), "new.txt", false, false).err(), Some(Errno::ENOENT));
    f.access_mode.switch_to_read_only();
    assert_eq!(f.ops.make(NodeId::ROOT, "new.txt", false, false).err(), Some(Errno::EROFS));
    assert_eq!(std::fs::read_dir(&f.disk).unwrap().count(), 0);
    assert!(f.rows().is_empty());
}

#[test]
fn a_name_that_stays_here_is_made_and_not_recorded() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "x.tmp", false, false).unwrap();
    assert!(f.disk.join("x.tmp").is_file() && f.rows().is_empty());
}

/// Programs replace a file by removing it and making it anew: the file keeps its OneDrive
/// id, and is modified here.
#[test]
fn a_file_made_where_a_onedrive_file_was_removed_is_that_files_new_content() {
    let f = folder(vec![of(OutboxKind::Delete, "A", "a.txt", "a.txt", (None, None))]);
    let (entry, _) = f.ops.make(NodeId::ROOT, "a.txt", false, false).unwrap();
    assert!(matches!(entry.state, EntryState::File(FileSyncState::ModifiedHere(row, _)) if row.id == "A"));
    assert!(f.backing_directory.has_downloaded_copy(&at("a.txt"), "A"));
    assert_eq!(f.said(), [(OutboxKind::Update, Some("A".into()), at("a.txt"))]);
    assert_eq!(f.shown("a.txt"), Some(Some("A".into())));
}
