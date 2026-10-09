
use fuser::Errno;
use konedrive_tree::outbox::{OutboxKind, OutboxState, Reason};

use crate::fuse::node_table::NodeId;
use crate::fuse::directory_contents::FileSyncState;
use crate::fuse::open_handles::{CloseOutcome, OpenFile};
use crate::fuse::write_operations::tests::{at, folder};

#[test]
fn a_local_only_file_written_waits_for_its_writer_and_is_due_once_closed() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "new.txt", false, false).unwrap();
    let node_id = f.node_id(NodeId::ROOT, "new.txt");
    let fh = f.ops.open_to_write(node_id, false).unwrap();
    assert!(matches!(f.open_handles.find_file(fh), Some(OpenFile::Writing { node_id: written, onedrive_id: None, .. }) if written == node_id));
    assert!(f.open_handles.is_open_for_writing_by_node_id(node_id));
    assert_eq!(f.rows()[0].reason, Some(Reason::OpenForWriting));
    f.open_handles.mark_written(node_id);
    let outcome = f.open_handles.close_file(fh).1;
    assert_eq!(outcome, CloseOutcome::ContentChanged);
    f.ops.close_last_writer(node_id, outcome);
    let row = &f.rows()[0];
    assert_eq!((row.kind, row.state, row.reason.clone()), (OutboxKind::Create, OutboxState::Waiting, Some(Reason::Changed)));
}

/// From its first writer on the outbox holds the file, so its sync state says it is modified
/// here with nothing asked of the open handles; closed with nothing written, the outbox lets
/// go of it again and its copy matches.
#[test]
fn a_onedrive_file_is_modified_here_from_its_first_writer_until_it_is_closed_untouched() {
    let f = folder(Vec::new());
    f.download("a.txt", "A", b"a");
    let node_id = f.node_id(NodeId::ROOT, "a.txt");
    assert!(matches!(f.file_state("a.txt"), FileSyncState::Synced(..)));
    let fh = f.ops.open_to_write(node_id, false).unwrap();
    assert!(f.open_handles.is_open_for_writing_by_onedrive_id("A"));
    assert_eq!(f.said(), [(OutboxKind::Update, Some("A".into()), at("a.txt"))]);
    assert!(matches!(f.file_state("a.txt"), FileSyncState::ModifiedHere(..)));
    let outcome = f.open_handles.close_file(fh).1;
    assert_eq!(outcome, CloseOutcome::ContentUntouched);
    f.ops.close_last_writer(node_id, outcome);
    assert!(f.rows().is_empty());
    assert!(matches!(f.file_state("a.txt"), FileSyncState::Synced(..)));
}

/// A second writer finds the file modified here: it records nothing more, and the last close
/// leaves the content changed, to be sent.
#[test]
fn a_second_writer_of_a_onedrive_file_records_nothing_more_and_its_content_counts_as_changed() {
    let f = folder(Vec::new());
    f.download("a.txt", "A", b"a");
    let node_id = f.node_id(NodeId::ROOT, "a.txt");
    let first = f.ops.open_to_write(node_id, false).unwrap();
    let second = f.ops.open_to_write(node_id, false).unwrap();
    assert_eq!(f.rows().len(), 1);
    let still_open = f.open_handles.close_file(first).1;
    assert_eq!(still_open, CloseOutcome::StillOpenElsewhere);
    // Nothing is recorded while another handle writes the file.
    f.ops.close_last_writer(node_id, still_open);
    assert_eq!(f.rows().len(), 1);
    assert!(matches!(f.file_state("a.txt"), FileSyncState::ModifiedHere(..)));
    let outcome = f.open_handles.close_file(second).1;
    assert_eq!(outcome, CloseOutcome::ContentChanged);
    f.ops.close_last_writer(node_id, outcome);
    let row = &f.rows()[0];
    assert_eq!((row.kind, row.state, row.reason.clone()), (OutboxKind::Update, OutboxState::Waiting, Some(Reason::Changed)));
}

#[test]
fn a_onedrive_file_that_is_not_downloaded_is_downloaded_before_it_is_written() {
    let f = folder(Vec::new());
    let node_id = f.node_id(NodeId::ROOT, "a.txt");
    assert_eq!(f.ops.needs_download(node_id, false).unwrap().map(|(row, rel)| (row.id, rel)), Some(("A".into(), at("a.txt"))));
    assert!(f.ops.needs_download(node_id, true).unwrap().is_none());
    assert_eq!(f.ops.open_to_write(node_id, false).err(), Some(Errno::EIO));
    f.download("a.txt", "A", b"a");
    assert!(f.ops.needs_download(node_id, false).unwrap().is_none());
}

/// Cut to nothing it needs no download: the copy is made empty, and is the file's new
/// content, modified here.
#[test]
fn a_onedrive_file_cut_to_nothing_is_its_empty_copy() {
    let f = folder(Vec::new());
    let node_id = f.node_id(NodeId::ROOT, "a.txt");
    f.ops.truncate(node_id, 0, None).unwrap();
    assert!(f.backing_directory.has_downloaded_copy(&at("a.txt"), "A"));
    assert_eq!(std::fs::metadata(f.disk.join("a.txt")).unwrap().len(), 0);
    let row = &f.rows()[0];
    assert_eq!((row.kind, row.item_id.as_deref(), row.reason.clone()), (OutboxKind::Update, Some("A"), Some(Reason::Changed)));
    assert!(matches!(f.file_state("a.txt"), FileSyncState::ModifiedHere(_, copy) if copy.size == 0));
}

#[test]
fn a_directory_or_a_read_only_folder_is_not_written() {
    let f = folder(Vec::new());
    assert_eq!(f.ops.open_to_write(f.docs(), false).err(), Some(Errno::EISDIR));
    assert_eq!(f.ops.truncate(f.docs(), 0, None).err(), Some(Errno::EISDIR));
    f.access_mode_switch.switch_to_read_only();
    let node_id = f.node_id(NodeId::ROOT, "a.txt");
    assert_eq!(f.ops.open_to_write(node_id, true).err(), Some(Errno::EROFS));
    assert_eq!(f.ops.truncate(node_id, 0, None).err(), Some(Errno::EROFS));
    // Nothing is downloaded for a write that is refused, and no time is set.
    assert_eq!(f.ops.needs_download(node_id, false).err(), Some(Errno::EROFS));
    assert_eq!(f.ops.set_modified_time(node_id, None).err(), Some(Errno::EROFS));
    assert!(f.rows().is_empty());
}

/// What a program still held open when the folder was switched to read-only is not sent: the
/// write operation asks the access mode, the recorder does not.
#[test]
fn the_last_writer_of_a_file_closed_in_a_folder_switched_to_read_only_records_nothing() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "new.txt", false, false).unwrap();
    let node_id = f.node_id(NodeId::ROOT, "new.txt");
    let fh = f.ops.open_to_write(node_id, false).unwrap();
    assert_eq!(f.rows()[0].reason, Some(Reason::OpenForWriting));
    f.access_mode_switch.switch_to_read_only();
    f.open_handles.mark_written(node_id);
    let outcome = f.open_handles.close_file(fh).1;
    assert_eq!(outcome, CloseOutcome::ContentChanged);
    f.ops.close_last_writer(node_id, outcome);
    let rows = f.rows();
    assert_eq!((rows.len(), rows[0].reason.clone()), (1, Some(Reason::OpenForWriting)));
}

/// A time set on a file is written on its file in the backing directory.
#[test]
fn a_time_set_on_a_file_is_its_copys_own() {
    let f = folder(Vec::new());
    f.download("a.txt", "A", b"a");
    let node_id = f.node_id(NodeId::ROOT, "a.txt");
    let at_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    f.ops.set_modified_time(node_id, Some(fuser::TimeOrNow::SpecificTime(at_time))).unwrap();
    assert_eq!(std::fs::metadata(f.disk.join("a.txt")).unwrap().modified().unwrap(), at_time);
    // No time given: nothing is written, and nothing is refused.
    f.ops.set_modified_time(node_id, None).unwrap();
    assert_eq!(std::fs::metadata(f.disk.join("a.txt")).unwrap().modified().unwrap(), at_time);
}
