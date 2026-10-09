
use fuser::Errno;
use konedrive_tree::outbox::{OutboxKind, OutboxState, Reason};

use crate::fuse::directory_contents::FileSyncState;
use crate::fuse::open_writers::AfterClose;
use crate::fuse::write_operations::tests::{at, folder};

#[test]
fn a_local_only_file_written_waits_for_its_writer_and_is_due_once_closed() {
    let f = folder(Vec::new());
    f.ops.make(1, "new.txt", false, false).unwrap();
    let node_id = f.node_id(1, "new.txt");
    let opened = f.ops.open_to_write(node_id, false).unwrap();
    assert!(opened.node_id == node_id && opened.onedrive_id.is_none() && opened.may_write && f.open_writers.is_open(node_id));
    assert_eq!(f.rows()[0].reason, Some(Reason::OpenForWriting));
    f.open_writers.mark_written(node_id);
    assert_eq!(f.open_writers.remove_writer(node_id), AfterClose::ContentChanged);
    f.ops.close_last_writer(node_id, true);
    let row = &f.rows()[0];
    assert_eq!((row.kind, row.state, row.reason.clone()), (OutboxKind::Create, OutboxState::Waiting, Some(Reason::Changed)));
}

/// From its first writer on the outbox holds the file, so its sync state says it is modified
/// here with nothing asked of the open writers; closed with nothing written, the outbox lets
/// go of it again and its copy matches.
#[test]
fn a_onedrive_file_is_modified_here_from_its_first_writer_until_it_is_closed_untouched() {
    let f = folder(Vec::new());
    f.download("a.txt", "A", b"a");
    let node_id = f.node_id(1, "a.txt");
    assert!(matches!(f.file_state("a.txt"), FileSyncState::Synced(..)));
    let opened = f.ops.open_to_write(node_id, false).unwrap();
    assert_eq!(opened.onedrive_id.as_deref(), Some("A"));
    assert_eq!(f.said(), [(OutboxKind::Update, Some("A".into()), at("a.txt"))]);
    assert!(matches!(f.file_state("a.txt"), FileSyncState::ModifiedHere(..)));
    assert_eq!(f.open_writers.remove_writer(node_id), AfterClose::ContentUntouched);
    f.ops.close_last_writer(node_id, false);
    assert!(f.rows().is_empty());
    assert!(matches!(f.file_state("a.txt"), FileSyncState::Synced(..)));
}

/// A second writer finds the file modified here: it records nothing more, and the last close
/// leaves the content changed, to be sent.
#[test]
fn a_second_writer_of_a_onedrive_file_records_nothing_more_and_its_content_counts_as_changed() {
    let f = folder(Vec::new());
    f.download("a.txt", "A", b"a");
    let node_id = f.node_id(1, "a.txt");
    f.ops.open_to_write(node_id, false).unwrap();
    f.ops.open_to_write(node_id, false).unwrap();
    assert_eq!(f.rows().len(), 1);
    assert_eq!(f.open_writers.remove_writer(node_id), AfterClose::StillOpenElsewhere);
    assert!(matches!(f.file_state("a.txt"), FileSyncState::ModifiedHere(..)));
    assert_eq!(f.open_writers.remove_writer(node_id), AfterClose::ContentChanged);
    f.ops.close_last_writer(node_id, true);
    let row = &f.rows()[0];
    assert_eq!((row.kind, row.state, row.reason.clone()), (OutboxKind::Update, OutboxState::Waiting, Some(Reason::Changed)));
}

#[test]
fn a_onedrive_file_that_is_not_downloaded_is_downloaded_before_it_is_written() {
    let f = folder(Vec::new());
    let node_id = f.node_id(1, "a.txt");
    assert_eq!(f.ops.needs_download(node_id, false).map(|(row, rel)| (row.id, rel)), Some(("A".into(), at("a.txt"))));
    assert!(f.ops.needs_download(node_id, true).is_none());
    assert_eq!(f.ops.open_to_write(node_id, false).err(), Some(Errno::EIO));
    f.download("a.txt", "A", b"a");
    assert!(f.ops.needs_download(node_id, false).is_none());
}

/// Cut to nothing it needs no download: the copy is made empty, and is the file's new
/// content, modified here.
#[test]
fn a_onedrive_file_cut_to_nothing_is_its_empty_copy() {
    let f = folder(Vec::new());
    let node_id = f.node_id(1, "a.txt");
    f.ops.truncate(node_id, 0).unwrap();
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
    assert_eq!(f.ops.truncate(f.docs(), 0).err(), Some(Errno::EISDIR));
    f.access_mode.switch_to_read_only();
    assert_eq!(f.ops.open_to_write(f.node_id(1, "a.txt"), true).err(), Some(Errno::EROFS));
    assert!(f.rows().is_empty());
}
