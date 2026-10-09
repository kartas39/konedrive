use crate::fuse::write_operations::EntryToMake;
use crate::fuse::write_operations::RenameMode;
use fuser::Errno;
use konedrive_tree::outbox::{OutboxKind, Reason};

use crate::fuse::node_table::NodeId;
use crate::fuse::directory_contents::FileSyncState;
use crate::fuse::write_operations::tests::{at, folder, folder_that_records_nothing};

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
    // What was at the new place first: it frees the name.
    assert_eq!(f.said(), [(OutboxKind::Delete, Some("B".into()), at("b.txt")), (OutboxKind::Move, Some("A".into()), at("b.txt"))]);
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
fn a_local_only_file_moved_into_an_ignored_directory_leaves_no_row() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "cache", EntryToMake::Directory).unwrap();
    f.ops.make(NodeId::ROOT, "x.txt", EntryToMake::File).unwrap();
    let cache = f.node_id(NodeId::ROOT, "cache");
    f.ops.rename(NodeId::ROOT, "x.txt", cache, "x.txt", RenameMode::Replace).unwrap();
    assert!(f.disk.join("cache/x.txt").is_file());
    assert!(f.rows().is_empty(), "{:?}", f.said());
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

/// The directory moves in OneDrive, and the rows of what the user changed below it are found
/// at their new place.
#[test]
fn a_onedrive_directory_moved_takes_the_rows_below_it_along() {
    let f = folder(Vec::new());
    f.ops.make(f.docs(), "mine.txt", EntryToMake::File).unwrap();
    f.ops.rename(NodeId::ROOT, "docs", NodeId::ROOT, "papers", RenameMode::Replace).unwrap();
    assert!(f.disk.join("papers/mine.txt").is_file() && !f.disk.join("docs").exists());
    assert_eq!(f.said(), [(OutboxKind::Create, None, at("papers/mine.txt")), (OutboxKind::Move, Some("D".into()), at("papers"))]);
    assert_eq!((f.shown("papers"), f.shown("docs")), (Some(Some("D".into())), None));
    assert_eq!(f.shown("papers/c.txt"), Some(Some("C".into())));
}

/// What was at the new place is gone, and nothing is ever that entry again: its node is
/// dropped, and the node of what moved has the name.
#[test]
fn a_local_only_entry_that_is_displaced_loses_its_node() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "x.txt", EntryToMake::File).unwrap();
    f.ops.make(NodeId::ROOT, "y.txt", EntryToMake::File).unwrap();
    std::fs::write(f.disk.join("x.txt"), b"x").unwrap();
    let (x, y) = (f.node_id(NodeId::ROOT, "x.txt"), f.node_id(NodeId::ROOT, "y.txt"));
    f.ops.rename(NodeId::ROOT, "x.txt", NodeId::ROOT, "y.txt", RenameMode::Replace).unwrap();
    assert_eq!(std::fs::read(f.disk.join("y.txt")).unwrap(), b"x");
    assert!(f.node_resolver.find_entry(y).is_none());
    assert_eq!(f.node_resolver.find_entry(x).map(|(entry, rel)| (entry.name, rel)), Some(("y.txt".to_owned(), at("y.txt"))));
    assert_eq!(f.node_id(NodeId::ROOT, "y.txt"), x);
}

/// Its row follows it to the new name and still waits for the writer, who goes on writing
/// the same node.
#[test]
fn a_file_renamed_while_it_is_open_for_writing_still_waits_for_its_writer() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "new.txt", EntryToMake::FileOpenedForWriting).unwrap();
    let node_id = f.node_id(NodeId::ROOT, "new.txt");
    f.ops.rename(NodeId::ROOT, "new.txt", NodeId::ROOT, "other.txt", RenameMode::Replace).unwrap();
    assert_eq!(f.said(), [(OutboxKind::Create, None, at("other.txt"))]);
    assert_eq!(f.rows()[0].reason, Some(Reason::OpenForWriting));
    assert!(f.open_handles.is_open_for_writing_by_node_id(node_id));
    assert_eq!(f.node_id(NodeId::ROOT, "other.txt"), node_id);
}

/// Nothing of it is uploaded from there: its own row and the rows of what it holds leave the
/// outbox.
#[test]
fn a_local_only_directory_moved_into_an_ignored_directory_leaves_no_row() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "cache", EntryToMake::Directory).unwrap();
    f.ops.make(NodeId::ROOT, "new", EntryToMake::Directory).unwrap();
    f.ops.make(f.node_id(NodeId::ROOT, "new"), "x.txt", EntryToMake::File).unwrap();
    let cache = f.node_id(NodeId::ROOT, "cache");
    f.ops.rename(NodeId::ROOT, "new", cache, "new", RenameMode::Replace).unwrap();
    assert!(f.disk.join("cache/new/x.txt").is_file());
    assert!(f.rows().is_empty(), "{:?}", f.said());
}

/// The name is its own: nothing moves and nothing is recorded, also when the rename is told
/// to keep what is at the new place.
#[test]
fn a_rename_onto_itself_does_nothing() {
    let f = folder(Vec::new());
    f.download("a.txt", "A", b"a");
    f.ops.rename(NodeId::ROOT, "a.txt", NodeId::ROOT, "a.txt", RenameMode::Replace).unwrap();
    f.ops.rename(NodeId::ROOT, "a.txt", NodeId::ROOT, "a.txt", RenameMode::NoReplace).unwrap();
    assert!(f.disk.join("a.txt").is_file() && f.rows().is_empty());
    assert_eq!(f.shown("a.txt"), Some(Some("A".into())));
}

/// What moved in the backing directory is moved back, and the program is told the disk
/// failed.
#[test]
fn a_rename_that_cannot_be_recorded_is_undone_and_answered_as_an_io_error() {
    let f = folder_that_records_nothing();
    f.download("a.txt", "A", b"a");
    assert_eq!(f.ops.rename(NodeId::ROOT, "a.txt", NodeId::ROOT, "new.txt", RenameMode::Replace).err(), Some(Errno::EIO));
    assert!(f.disk.join("a.txt").is_file() && !f.disk.join("new.txt").exists());
    assert_eq!((f.shown("a.txt"), f.shown("new.txt")), (Some(Some("A".into())), None));
}

#[test]
fn nothing_is_renamed_in_a_read_only_folder() {
    let f = folder(Vec::new());
    f.access_mode_switch.switch_to_read_only();
    assert_eq!(f.ops.rename(NodeId::ROOT, "a.txt", NodeId::ROOT, "new.txt", RenameMode::Replace).err(), Some(Errno::EROFS));
    assert!(f.rows().is_empty());
}

/// What the rename replaced at the new place is back there as well: a file a file moved
/// over, the downloaded copy under a file that is not downloaded, an empty directory under
/// a directory.
#[test]
fn a_rename_that_cannot_be_recorded_puts_back_what_was_at_the_new_place() {
    let f = folder_that_records_nothing();
    std::fs::write(f.disk.join("x.txt"), b"x").unwrap();
    std::fs::write(f.disk.join("y.txt"), b"y").unwrap();
    assert_eq!(f.ops.rename(NodeId::ROOT, "x.txt", NodeId::ROOT, "y.txt", RenameMode::Replace).err(), Some(Errno::EIO));
    assert_eq!((std::fs::read(f.disk.join("x.txt")).unwrap(), std::fs::read(f.disk.join("y.txt")).unwrap()), (b"x".to_vec(), b"y".to_vec()));

    // Nothing of `a.txt` is in the backing directory: only what was at the new place moved.
    f.download("b.txt", "B", b"b");
    assert_eq!(f.ops.rename(NodeId::ROOT, "a.txt", NodeId::ROOT, "b.txt", RenameMode::Replace).err(), Some(Errno::EIO));
    assert!(f.backing_directory.find_downloaded_copy(&at("b.txt"), "B").is_some() && !f.disk.join("a.txt").exists());

    std::fs::create_dir(f.disk.join("new")).unwrap();
    std::fs::create_dir(f.disk.join("empty")).unwrap();
    let (new, empty) = (f.backing_directory.find_file_handle(&at("new")), f.backing_directory.find_file_handle(&at("empty")));
    assert_eq!(f.ops.rename(NodeId::ROOT, "new", NodeId::ROOT, "empty", RenameMode::Replace).err(), Some(Errno::EIO));
    assert_eq!((f.backing_directory.find_file_handle(&at("new")), f.backing_directory.find_file_handle(&at("empty"))), (new, empty));
}

/// Once the rename is recorded, what it replaced is gone from the backing directory, and
/// nothing of it is left at the old place.
#[test]
fn what_a_rename_replaced_is_removed_once_the_rename_is_recorded() {
    let f = folder(Vec::new());
    std::fs::write(f.disk.join("x.txt"), b"x").unwrap();
    std::fs::write(f.disk.join("y.txt"), b"y").unwrap();
    f.ops.rename(NodeId::ROOT, "x.txt", NodeId::ROOT, "y.txt", RenameMode::Replace).unwrap();
    assert!(!f.disk.join("x.txt").exists());
    assert_eq!(std::fs::read(f.disk.join("y.txt")).unwrap(), b"x");

    // Nothing of `docs/c.txt` is in the backing directory, nor its directory.
    f.download("b.txt", "B", b"b");
    f.ops.rename(f.docs(), "c.txt", NodeId::ROOT, "b.txt", RenameMode::Replace).unwrap();
    assert!(!f.disk.join("b.txt").exists() && !f.disk.join("docs/c.txt").exists());
    assert_eq!(f.shown("b.txt"), Some(Some("C".into())));

    std::fs::create_dir(f.disk.join("new")).unwrap();
    std::fs::create_dir(f.disk.join("empty")).unwrap();
    let new = f.backing_directory.find_file_handle(&at("new"));
    f.ops.rename(NodeId::ROOT, "new", NodeId::ROOT, "empty", RenameMode::Replace).unwrap();
    assert!(!f.disk.join("new").exists());
    assert_eq!(f.backing_directory.find_file_handle(&at("empty")), new);
}
