use std::path::PathBuf;

use fuser::Errno;
use konedrive_tree::outbox::{OutboxKind, OutboxState, Reason};
use konedrive_tree::Kind;

use super::{describe_file_write, describe_last_close};
use crate::fuse::access_mode_switch::AccessMode;
use crate::fuse::detections::tests::{file, in_root, local_only};
use crate::fuse::detections::{FileWritten, LocalOnlyFileWrite, OneDriveFileWrite};
use crate::fuse::directory_contents::tests::row;
use crate::fuse::directory_contents::{DirectoryEntry, EntryState, FileSyncState};
use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::entry_place::EntryPlace;
use crate::fuse::node_table::NodeId;
use crate::fuse::open_handles::{CloseOutcome, OpenFile};
use crate::fuse::upload_rules::UploadStatus;
use crate::fuse::write_operations::tests::{at, folder, folder_that_records_nothing};
use crate::fuse::write_operations::EntryToMake;

fn root() -> DirectoryReference {
    DirectoryReference { onedrive_id: Some("R".into()), path: PathBuf::new() }
}

/// The OneDrive file `A`, shown as `a.txt` in the root, with its downloaded copy.
fn onedrive_file() -> DirectoryEntry {
    DirectoryEntry { name: "a.txt".into(), state: EntryState::File(FileSyncState::Synced(row("A", Some("R"), "a.txt", Kind::File), file(9, 1).metadata)) }
}

/// A local-only file `new.txt` in the root.
fn local_only_file() -> DirectoryEntry {
    DirectoryEntry::from_local_only(local_only(&file(1, 3), &in_root("new.txt")))
}

/// What the close of the last writer of `entry` is recorded as, in a read-write folder
/// whose backing directory holds the copy `file(9, 1)` and uploads from the place.
fn last_close(outcome: CloseOutcome, entry: &DirectoryEntry, node_stands_for_file: bool) -> Option<FileWritten> {
    describe_last_close(outcome, AccessMode::ReadWrite, entry, &root(), Some(file(9, 1)), UploadStatus::Uploaded, node_stands_for_file)
}

fn onedrive_write(write: OneDriveFileWrite) -> Option<FileWritten> {
    Some(FileWritten::InOneDrive { onedrive_id: "A".into(), place: EntryPlace::new(&root(), "a.txt"), write })
}

#[test]
fn a_close_that_is_not_the_last_writers_records_nothing() {
    assert_eq!(last_close(CloseOutcome::StillOpenElsewhere, &onedrive_file(), true), None);
    assert_eq!(last_close(CloseOutcome::StillOpenElsewhere, &local_only_file(), false), None);
}

#[test]
fn the_last_close_in_a_read_only_folder_records_nothing() {
    for outcome in [CloseOutcome::ContentChanged, CloseOutcome::ContentUntouched] {
        assert_eq!(describe_last_close(outcome, AccessMode::ReadOnly, &onedrive_file(), &root(), Some(file(9, 1)), UploadStatus::Uploaded, true), None);
        assert_eq!(describe_last_close(outcome, AccessMode::ReadOnly, &local_only_file(), &root(), Some(file(1, 3)), UploadStatus::Uploaded, false), None);
    }
}

#[test]
fn a_onedrive_file_closed_changed_is_due_with_its_copy() {
    assert_eq!(last_close(CloseOutcome::ContentChanged, &onedrive_file(), true), onedrive_write(OneDriveFileWrite::ClosedChanged(file(9, 1))));
}

#[test]
fn a_onedrive_file_closed_untouched_is_let_go_of() {
    assert_eq!(last_close(CloseOutcome::ContentUntouched, &onedrive_file(), true), onedrive_write(OneDriveFileWrite::ClosedUntouched));
}

/// Its name was taken over by a file saved over it: the node says nothing of the file's
/// content, and the row that holds the new content stays.
#[test]
fn a_node_that_no_longer_stands_for_the_file_closed_untouched_records_nothing() {
    assert_eq!(last_close(CloseOutcome::ContentUntouched, &onedrive_file(), false), None);
}

/// It was never sent: it is due whether or not anything was written.
#[test]
fn a_local_only_file_closed_is_due_whatever_was_written() {
    let due = Some(FileWritten::LocalOnly { file: local_only(&file(1, 3), &in_root("new.txt")), write: LocalOnlyFileWrite::Closed });
    assert_eq!(last_close(CloseOutcome::ContentChanged, &local_only_file(), false), due);
    assert_eq!(last_close(CloseOutcome::ContentUntouched, &local_only_file(), false), due);
}

#[test]
fn a_write_of_an_ignored_local_only_file_records_nothing() {
    for write in [LocalOnlyFileWrite::Opened, LocalOnlyFileWrite::Closed, LocalOnlyFileWrite::Cut] {
        assert_eq!(describe_file_write(&local_only_file(), &root(), Some(file(1, 3)), UploadStatus::Ignored, write), None);
    }
}

#[test]
fn a_write_of_a_onedrive_file_is_recorded_with_its_copy_as_it_is() {
    let described = |write| describe_file_write(&onedrive_file(), &root(), Some(file(9, 1)), UploadStatus::Uploaded, write);
    assert_eq!(described(LocalOnlyFileWrite::Opened), onedrive_write(OneDriveFileWrite::Opened(file(9, 1))));
    assert_eq!(described(LocalOnlyFileWrite::Closed), onedrive_write(OneDriveFileWrite::ClosedChanged(file(9, 1))));
    assert_eq!(described(LocalOnlyFileWrite::Cut), onedrive_write(OneDriveFileWrite::Cut(file(9, 1))));
}

/// There is no content of the user's own to send.
#[test]
fn a_write_of_a_onedrive_file_the_backing_directory_holds_no_copy_of_records_nothing() {
    assert_eq!(describe_file_write(&onedrive_file(), &root(), None, UploadStatus::Uploaded, LocalOnlyFileWrite::Closed), None);
}

#[test]
fn a_local_only_file_written_waits_for_its_writer_and_is_due_once_closed() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "new.txt", EntryToMake::File).unwrap();
    let node_id = f.node_id(NodeId::ROOT, "new.txt");
    let fh = f.ops.open_to_write(node_id).unwrap();
    assert!(matches!(f.open_handles.find_file(fh), Some(OpenFile::Writing { node_id: written, onedrive_id: None, .. }) if written == node_id));
    assert!(f.open_handles.is_open_for_writing_by_node_id(node_id));
    assert_eq!(f.rows()[0].reason, Some(Reason::OpenForWriting));
    f.open_handles.mark_written(node_id);
    f.ops.close_writer(fh);
    assert!(!f.open_handles.is_open_for_writing_by_node_id(node_id));
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
    let fh = f.ops.open_to_write(node_id).unwrap();
    assert!(f.open_handles.is_open_for_writing_by_onedrive_id("A"));
    assert_eq!(f.said(), [(OutboxKind::Update, Some("A".into()), at("a.txt"))]);
    assert!(matches!(f.file_state("a.txt"), FileSyncState::ModifiedHere(..)));
    f.ops.close_writer(fh);
    assert!(!f.open_handles.is_open_for_writing_by_onedrive_id("A"));
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
    let first = f.ops.open_to_write(node_id).unwrap();
    let second = f.ops.open_to_write(node_id).unwrap();
    assert_eq!(f.said(), [(OutboxKind::Update, Some("A".into()), at("a.txt"))]);
    // Nothing is recorded while another handle writes the file.
    f.ops.close_writer(first);
    assert!(f.open_handles.is_open_for_writing_by_node_id(node_id));
    assert_eq!(f.said(), [(OutboxKind::Update, Some("A".into()), at("a.txt"))]);
    assert_eq!(f.rows()[0].reason, Some(Reason::OpenForWriting));
    assert!(matches!(f.file_state("a.txt"), FileSyncState::ModifiedHere(..)));
    f.ops.close_writer(second);
    let row = &f.rows()[0];
    assert_eq!((row.kind, row.state, row.reason.clone()), (OutboxKind::Update, OutboxState::Waiting, Some(Reason::Changed)));
}

#[test]
fn a_onedrive_file_that_is_not_downloaded_is_downloaded_before_it_is_written() {
    let f = folder(Vec::new());
    let node_id = f.node_id(NodeId::ROOT, "a.txt");
    assert_eq!(f.ops.find_needed_download(node_id).unwrap().map(|needed| (needed.row.id, needed.path)), Some(("A".into(), at("a.txt"))));
    assert_eq!(f.ops.open_to_write(node_id).err(), Some(Errno::EIO));
    f.download("a.txt", "A", b"a");
    assert!(f.ops.find_needed_download(node_id).unwrap().is_none());
}

/// Cut to nothing it needs no download: the copy is made empty, and is the file's new
/// content, modified here.
#[test]
fn a_onedrive_file_cut_to_nothing_is_its_empty_copy() {
    let f = folder(Vec::new());
    let node_id = f.node_id(NodeId::ROOT, "a.txt");
    f.ops.truncate(node_id, 0, None).unwrap();
    assert!(f.backing_directory.find_downloaded_copy(&at("a.txt"), "A").is_some());
    assert_eq!(std::fs::metadata(f.disk.join("a.txt")).unwrap().len(), 0);
    let row = &f.rows()[0];
    assert_eq!((row.kind, row.item_id.as_deref(), row.reason.clone()), (OutboxKind::Update, Some("A"), Some(Reason::Changed)));
    assert!(matches!(f.file_state("a.txt"), FileSyncState::ModifiedHere(_, copy) if copy.size == 0));
}

#[test]
fn a_directory_or_a_read_only_folder_is_not_written() {
    let f = folder(Vec::new());
    assert_eq!(f.ops.open_to_write(f.docs()).err(), Some(Errno::EISDIR));
    assert_eq!(f.ops.truncate(f.docs(), 0, None).err(), Some(Errno::EISDIR));
    f.access_mode_switch.switch_to_read_only();
    let node_id = f.node_id(NodeId::ROOT, "a.txt");
    assert_eq!(f.ops.open_to_overwrite(node_id).err(), Some(Errno::EROFS));
    assert_eq!(f.ops.truncate(node_id, 0, None).err(), Some(Errno::EROFS));
    // Nothing is downloaded for a write that is refused, and no time is set.
    assert_eq!(f.ops.find_needed_download(node_id).err(), Some(Errno::EROFS));
    assert_eq!(f.ops.set_modified_time(node_id, None).err(), Some(Errno::EROFS));
    assert!(f.rows().is_empty());
}

/// What a program still held open when the folder was switched to read-only is not sent: the
/// write operation asks the access mode, the recorder does not.
#[test]
fn the_last_writer_of_a_file_closed_in_a_folder_switched_to_read_only_records_nothing() {
    let f = folder(Vec::new());
    f.ops.make(NodeId::ROOT, "new.txt", EntryToMake::File).unwrap();
    let node_id = f.node_id(NodeId::ROOT, "new.txt");
    let fh = f.ops.open_to_write(node_id).unwrap();
    assert_eq!(f.rows()[0].reason, Some(Reason::OpenForWriting));
    f.access_mode_switch.switch_to_read_only();
    f.open_handles.mark_written(node_id);
    f.ops.close_writer(fh);
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

/// The first writer of a file is recorded before the program is told the file is open: an
/// open that cannot be recorded is taken back, and no handle is left writing the file.
#[test]
fn an_open_for_writing_that_cannot_be_recorded_is_closed_and_answered_as_an_io_error() {
    let f = folder_that_records_nothing();
    std::fs::write(f.disk.join("mine.txt"), b"mine").unwrap();
    let node_id = f.node_id(NodeId::ROOT, "mine.txt");
    assert_eq!(f.ops.open_to_write(node_id).err(), Some(Errno::EIO));
    assert!(!f.open_handles.is_open_for_writing_by_node_id(node_id));
    assert_eq!(std::fs::read(f.disk.join("mine.txt")).unwrap(), b"mine");
}

/// Recorded before it is cut: a cut that cannot be recorded cuts nothing.
#[test]
fn a_cut_that_cannot_be_recorded_cuts_nothing_and_is_answered_as_an_io_error() {
    let f = folder_that_records_nothing();
    std::fs::write(f.disk.join("mine.txt"), b"mine").unwrap();
    let node_id = f.node_id(NodeId::ROOT, "mine.txt");
    assert_eq!(f.ops.truncate(node_id, 1, None).err(), Some(Errno::EIO));
    assert_eq!(std::fs::read(f.disk.join("mine.txt")).unwrap(), b"mine");
}

/// A writer that is not of any file the handles hold — closed already, or never opened — is
/// closed as nothing: no row is touched.
#[test]
fn the_close_of_what_is_not_open_for_writing_records_nothing() {
    let f = folder(Vec::new());
    f.download("a.txt", "A", b"a");
    let fh = f.ops.open_to_write(f.node_id(NodeId::ROOT, "a.txt")).unwrap();
    f.ops.close_writer(crate::fuse::open_handles::Fh(fh.0 + 1));
    assert_eq!(f.said(), [(OutboxKind::Update, Some("A".into()), at("a.txt"))]);
    assert!(f.open_handles.is_open_for_writing_by_onedrive_id("A"));
}

/// A cut by its path of a file a program holds open is that program's write: the node is
/// marked written, and nothing is recorded — the outbox waits for the writer already. Its
/// close then leaves the content to be sent.
#[test]
fn a_cut_of_a_file_a_program_holds_open_marks_it_written_and_records_nothing() {
    let f = folder(Vec::new());
    f.download("a.txt", "A", b"content");
    let node_id = f.node_id(NodeId::ROOT, "a.txt");
    let fh = f.ops.open_to_write(node_id).unwrap();
    let opened = f.rows();
    f.ops.truncate(node_id, 3, None).unwrap();
    assert_eq!(std::fs::read(f.disk.join("a.txt")).unwrap(), b"con");
    assert_eq!(f.rows(), opened, "the row still waits for the writer");
    assert_eq!(f.rows()[0].reason, Some(Reason::OpenForWriting));
    f.ops.close_writer(fh);
    let row = &f.rows()[0];
    assert_eq!((row.kind, row.state, row.reason.clone()), (OutboxKind::Update, OutboxState::Waiting, Some(Reason::Changed)));
}
