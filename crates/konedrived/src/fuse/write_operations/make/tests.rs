use crate::fuse::write_operations::EntryToMake;

use fuser::Errno;
use konedrive_tree::outbox::{OutboxKind, OutboxState, Reason};

use crate::fuse::open_handles::OpenFile;
use crate::fuse::node_table::NodeId;
use crate::fuse::directory_contents::tests::of;
use crate::fuse::directory_contents::{DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::write_operations::tests::{at, folder, folder_that_records_nothing};

#[test]
fn a_file_made_is_on_disk_local_only_and_waits_in_the_outbox_to_be_created() {
    let f = folder(Vec::new());
    let (node_id, entry, opened) = f.ops.make(NodeId::ROOT, "new.txt", EntryToMake::File).unwrap();
    assert_eq!(node_id, f.node_id(NodeId::ROOT, "new.txt"), "the node id it was given is the one the name is found by");
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
    let (node_id, _, opened) = f.ops.make(NodeId::ROOT, "new.txt", EntryToMake::FileOpenedForWriting).unwrap();
    let fh = opened.unwrap();
    assert_eq!(node_id, f.node_id(NodeId::ROOT, "new.txt"));
    assert!(f.open_handles.is_open_for_writing_by_node_id(node_id));
    assert!(matches!(f.open_handles.find_file(fh), Some(OpenFile::Writing { node_id: written, .. }) if written == node_id));
    assert_eq!(f.rows()[0].reason, Some(Reason::OpenForWriting));
}

#[test]
fn a_directory_made_is_made_in_onedrive_inside_the_directory_it_is_in() {
    let f = folder(Vec::new());
    let (_, entry, _) = f.ops.make(f.docs(), "new", EntryToMake::Directory).unwrap();
    assert!(matches!(entry.state, EntryState::Directory(DirectorySyncState::LocalOnly(_))));
    assert!(f.disk.join("docs/new").is_dir());
    assert_eq!(f.backing_directory.read_onedrive_id(&at("docs")).as_deref(), Some("D"));
    assert_eq!(f.said(), [(OutboxKind::Mkdir, None, at("docs/new"))]);
    assert_eq!(f.rows()[0].target_parent.as_deref(), Some("D"));
}

#[test]
fn what_may_not_be_made_is_refused_and_nothing_is_made() {
    let f = folder(Vec::new());
    assert_eq!(f.ops.make(NodeId::ROOT, "a.txt", EntryToMake::File).err(), Some(Errno::EEXIST));
    assert_eq!(f.ops.make(NodeId::ROOT, crate::folder::disk::HOLDING, EntryToMake::Directory).err(), Some(Errno::EPERM));
    assert_eq!(f.ops.make(NodeId::ROOT, ".Trash-1000", EntryToMake::Directory).err(), Some(Errno::EPERM));
    assert_eq!(f.ops.make(NodeId(99), "new.txt", EntryToMake::File).err(), Some(Errno::ENOENT));
    f.access_mode_switch.switch_to_read_only();
    assert_eq!(f.ops.make(NodeId::ROOT, "new.txt", EntryToMake::File).err(), Some(Errno::EROFS));
    assert_eq!(std::fs::read_dir(&f.disk).unwrap().count(), 0);
    assert!(f.rows().is_empty());
}

#[test]
fn an_ignored_name_is_made_and_not_recorded() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "x.tmp", EntryToMake::File).unwrap();
    assert!(f.disk.join("x.tmp").is_file() && f.rows().is_empty());
}

/// Programs replace a file by removing it and making it anew: the file keeps its OneDrive
/// id, and is modified here.
#[test]
fn a_file_made_where_a_onedrive_file_was_removed_is_that_files_new_content() {
    let f = folder(vec![of(OutboxKind::Delete, "A", "a.txt", "a.txt", (None, None))]);
    let (_, entry, _) = f.ops.make(NodeId::ROOT, "a.txt", EntryToMake::File).unwrap();
    assert!(matches!(entry.state, EntryState::File(FileSyncState::ModifiedHere(row, _)) if row.id == "A"));
    assert!(f.backing_directory.find_downloaded_copy(&at("a.txt"), "A").is_some());
    assert_eq!(f.said(), [(OutboxKind::Update, Some("A".into()), at("a.txt"))]);
    assert_eq!(f.shown("a.txt"), Some(Some("A".into())));
}

/// The change is in the outbox before the program hears it was made: one that cannot be
/// recorded is taken back from the disk, and the program is told the disk failed.
#[test]
fn a_file_or_directory_that_cannot_be_recorded_is_unmade_and_answered_as_an_io_error() {
    let f = folder_that_records_nothing();
    assert_eq!(f.ops.make(NodeId::ROOT, "new.txt", EntryToMake::File).err(), Some(Errno::EIO));
    assert_eq!(f.ops.make(NodeId::ROOT, "open.txt", EntryToMake::FileOpenedForWriting).err(), Some(Errno::EIO));
    assert_eq!(f.ops.make(NodeId::ROOT, "new", EntryToMake::Directory).err(), Some(Errno::EIO));
    assert_eq!(std::fs::read_dir(&f.disk).unwrap().count(), 0);
    assert_eq!(f.shown("new.txt"), None);
}

/// What is ignored is not recorded, so it is made also where nothing can be recorded.
#[test]
fn an_ignored_name_is_made_without_the_outbox() {
    let f = folder_that_records_nothing();
    f.ops.make(NodeId::ROOT, "x.tmp", EntryToMake::File).unwrap();
    assert!(f.disk.join("x.tmp").is_file());
}
