//! The requests that change a file's content or its time, over the parts of a mount built
//! without a mount.

use std::sync::{mpsc, Arc};
use std::time::Duration;

use fuser::Errno;
use konedrive_tree::outbox::{OutboxKind, OutboxRow, Reason};

use crate::fuse::kernel_requests::write_requests::tests::{Answer, ToldReply};
use crate::fuse::mount::tests::{build_mount, BuiltMount};
use crate::fuse::node_table::NodeId;
use crate::fuse::open_handles::{Fh, OpenFile};

/// The time-only `setattr` is a write operation like every other: it waits for the tree
/// lock on the write-operation thread, and is not made on the mount's own thread.
#[test]
fn a_time_set_on_a_file_runs_on_the_write_operation_thread() {
    let built = build_mount();
    let write_requests = &built.kernel_requests.write_requests;
    write_requests.access_mode_switch.switch_to_read_write();
    let held = built.tree_lock.try_lock().unwrap();
    let (tell, told) = mpsc::channel();
    let answered = tell.clone();
    write_requests.run_set_modified_time(NodeId::ROOT, None, ToldReply(tell), move |_, node_id, _| answered.send(Answer::OperationRan(format!("{node_id:?}"))).unwrap());
    assert_eq!(told.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout));
    drop(held);
    assert_eq!(told.recv_timeout(Duration::from_secs(5)), Ok(Answer::OperationRan(format!("{:?}", NodeId::ROOT))));
}

/// A folder that is switched to read-only between the request and its turn on the thread
/// takes no time: the operation asks the mode there, where the switch runs.
#[test]
fn a_time_set_on_a_file_is_refused_when_the_folder_was_switched_to_read_only_before_its_turn() {
    let built = build_mount();
    let write_requests = &built.kernel_requests.write_requests;
    write_requests.access_mode_switch.switch_to_read_write();
    let held = built.tree_lock.try_lock().unwrap();
    let (tell, told) = mpsc::channel();
    let switched = write_requests.access_mode_switch.queue_switch_to_read_only(&write_requests.write_operation_thread);
    write_requests.run_set_modified_time(NodeId::ROOT, None, ToldReply(tell), |_, _, _| panic!("no time is set in a read-only folder"));
    drop(held);
    assert_eq!(switched.blocking_recv(), Ok(true));
    assert_eq!(told.recv_timeout(Duration::from_secs(5)), Ok(Answer::Refused(Errno::EROFS)));
}

#[test]
fn a_time_set_on_a_file_of_a_read_only_folder_is_refused_without_waiting_for_the_thread() {
    let built = build_mount();
    let held = built.tree_lock.try_lock().unwrap();
    let (tell, told) = mpsc::channel();
    built.kernel_requests.write_requests.run_set_modified_time(NodeId::ROOT, None, ToldReply(tell), |_, _, _| {});
    assert_eq!(told.try_recv(), Ok(Answer::Refused(Errno::EROFS)));
    drop(held);
}

/// The parts of a read-write mount whose `a.txt` is downloaded, with the node id of `a.txt`.
fn build_mount_with_downloaded_file() -> (BuiltMount, NodeId) {
    let built = build_mount();
    built.kernel_requests.write_requests.access_mode_switch.switch_to_read_write();
    built.download("a.txt", "A", b"content");
    let node_id = built.node_id_in_root("a.txt");
    (built, node_id)
}

/// A program opens the node `node_id` for writing: the open is put on the write-operation
/// thread, and its `fh` comes once it ran.
fn queue_open_to_write(built: &BuiltMount, node_id: NodeId) -> tokio::sync::oneshot::Receiver<Result<Fh, Errno>> {
    let write_requests = &built.kernel_requests.write_requests;
    let write_operations = Arc::clone(&write_requests.write_operations);
    write_requests.write_operation_thread.run_and_wait(move || write_operations.open_to_write(node_id))
}

fn outbox_rows(built: &BuiltMount) -> Vec<OutboxRow> {
    built.store.call_blocking(|s| s.outbox_rows()).unwrap()
}

/// A cycle holds the tree lock; another program's open of the file waits on the thread; the
/// first program closes its writer, with nothing written. What that close means is decided
/// on the thread, after the open that came before it: the file is still open for writing,
/// and the outbox keeps holding it.
#[test]
fn a_writer_closed_behind_another_programs_open_leaves_the_outbox_holding_the_file() {
    let (built, node_id) = build_mount_with_downloaded_file();
    let write_requests = &built.kernel_requests.write_requests;
    let first = queue_open_to_write(&built, node_id).blocking_recv().unwrap().unwrap();
    let held = built.tree_lock.blocking_lock();
    let second = queue_open_to_write(&built, node_id);
    assert_eq!(write_requests.close_file(first), Some(node_id));
    drop(held);
    let second = second.blocking_recv().unwrap().unwrap();
    built.run_on_write_operation_thread(|| {});
    assert!(write_requests.open_handles.find_file_to_write(second).is_some());
    let rows = outbox_rows(&built);
    assert_eq!(rows.iter().map(|row| (row.kind, row.item_id.as_deref(), row.reason.clone())).collect::<Vec<_>>(), [(OutboxKind::Update, Some("A"), Some(Reason::OpenForWriting))]);
}

/// A cycle holds the tree lock; a truncate of the file by its path waits on the thread; the
/// program closes its writer, with nothing written. The truncate finds the file open and marks it
/// written, and the close that follows it on the thread leaves the truncated content to be sent.
#[test]
fn a_writer_closed_behind_a_truncate_of_its_file_leaves_the_truncated_content_to_be_sent() {
    let (built, node_id) = build_mount_with_downloaded_file();
    let write_requests = &built.kernel_requests.write_requests;
    let writer = queue_open_to_write(&built, node_id).blocking_recv().unwrap().unwrap();
    let held = built.tree_lock.blocking_lock();
    let write_operations = Arc::clone(&write_requests.write_operations);
    let truncated = write_requests.write_operation_thread.run_and_wait(move || write_operations.truncate(node_id, 0, None));
    assert_eq!(write_requests.close_file(writer), Some(node_id));
    drop(held);
    assert_eq!(truncated.blocking_recv().unwrap(), Ok(()));
    built.run_on_write_operation_thread(|| {});
    assert_eq!(std::fs::metadata(built.disk.join("a.txt")).unwrap().len(), 0);
    let rows = outbox_rows(&built);
    assert_eq!(rows.iter().map(|row| (row.kind, row.item_id.as_deref(), row.reason.clone())).collect::<Vec<_>>(), [(OutboxKind::Update, Some("A"), Some(Reason::Changed))]);
}

/// What the outbox holds: each row's kind, OneDrive id and reason.
fn list_rows(built: &BuiltMount) -> Vec<(OutboxKind, Option<String>, Option<Reason>)> {
    outbox_rows(built).into_iter().map(|row| (row.kind, row.item_id, row.reason)).collect()
}

/// The bytes go into the file in the backing directory, and the node is marked written: its
/// close leaves the content to be sent.
#[test]
fn bytes_written_through_a_writer_go_into_the_backing_file_and_its_close_leaves_them_to_be_sent() {
    let (built, node_id) = build_mount_with_downloaded_file();
    let write_requests = &built.kernel_requests.write_requests;
    let fh = queue_open_to_write(&built, node_id).blocking_recv().unwrap().unwrap();
    assert_eq!(write_requests.write(fh, 0, b"MINE"), Ok(4));
    assert_eq!(std::fs::read(built.disk.join("a.txt")).unwrap(), b"MINEent");
    assert_eq!(write_requests.sync_file(fh), Ok(()));
    assert_eq!(write_requests.close_file(fh), Some(node_id));
    built.run_on_write_operation_thread(|| {});
    assert!(!write_requests.open_handles.is_open_for_writing_by_node_id(node_id));
    assert_eq!(list_rows(&built), [(OutboxKind::Update, Some("A".into()), Some(Reason::Changed))]);
}

/// Nothing is written through a handle that may not write, or through one that holds
/// nothing open.
#[test]
fn a_write_through_what_is_not_open_for_writing_is_refused() {
    let (built, _) = build_mount_with_downloaded_file();
    let write_requests = &built.kernel_requests.write_requests;
    let reader = write_requests.open_handles.add_file(OpenFile::ReadingBackingFile { backing_file: Arc::new(std::fs::File::open(built.disk.join("a.txt")).unwrap()) });
    assert_eq!(write_requests.write(reader, 0, b"MINE"), Err(Errno::EBADF));
    assert_eq!(write_requests.write(Fh(99), 0, b"MINE"), Err(Errno::EBADF));
    assert_eq!(write_requests.sync_file(reader), Ok(()));
    assert_eq!(std::fs::read(built.disk.join("a.txt")).unwrap(), b"content");
}

/// A file opened to be read is closed at once, also while a cycle holds the tree lock; one
/// opened for writing stays open for writing until its close ran on the thread.
#[test]
fn a_reader_is_closed_at_once_and_a_writer_on_the_write_operation_thread() {
    let (built, node_id) = build_mount_with_downloaded_file();
    let write_requests = &built.kernel_requests.write_requests;
    let reader = write_requests.open_handles.add_file(OpenFile::ReadingBackingFile { backing_file: Arc::new(std::fs::File::open(built.disk.join("a.txt")).unwrap()) });
    let writer = queue_open_to_write(&built, node_id).blocking_recv().unwrap().unwrap();
    let held = built.tree_lock.blocking_lock();
    assert_eq!(write_requests.close_file(reader), None);
    assert!(write_requests.open_handles.find_file(reader).is_none());
    assert_eq!(write_requests.close_file(writer), Some(node_id));
    assert!(write_requests.open_handles.is_open_for_writing_by_node_id(node_id), "until its close runs on the thread");
    drop(held);
    built.run_on_write_operation_thread(|| {});
    assert!(!write_requests.open_handles.is_open_for_writing_by_node_id(node_id));
    // Closed with nothing written: the outbox let go of the file.
    assert!(list_rows(&built).is_empty());
}
