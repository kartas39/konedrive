//! The write requests over the parts of a mount built without a mount: what is refused
//! before it is put on the write-operation thread, and what is put there.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use fuser::{Errno, RenameFlags};
use konedrive_tree::outbox::{OutboxKind, OutboxRow, Reason};

use super::{check_mknod_mode, ErrnoReply};
use crate::fuse::mount::tests::{build_mount, BuiltMount};
use crate::fuse::node_table::NodeId;
use crate::fuse::open_handles::Fh;
use crate::fuse::write_operations::RenameMode;

/// What a request was answered: the errno it was refused with, or that its operation ran.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    Refused(Errno),
    OperationRan(String),
}

/// A reply that tells the test what it was answered.
struct ToldReply(mpsc::Sender<Answer>);

impl ErrnoReply for ToldReply {
    fn refuse(self, errno: Errno) {
        self.0.send(Answer::Refused(errno)).unwrap();
    }
}

/// A cycle of the delta listing holds the tree lock, so the write-operation thread runs
/// nothing: a program that changes a read-only folder is told at once all the same.
#[test]
fn a_change_to_a_read_only_folder_is_refused_without_waiting_for_the_thread() {
    let built = build_mount();
    let held = built.tree_lock.try_lock().unwrap();
    let (tell, told) = mpsc::channel();
    built.kernel_requests.write_requests.run_named_operation(OsStr::new("new.txt"), Errno::EINVAL, ToldReply(tell), |_, name, reply| {
        reply.0.send(Answer::OperationRan(name.to_owned())).unwrap();
    });
    assert_eq!(told.try_recv(), Ok(Answer::Refused(Errno::EROFS)));
    drop(held);
    // And its operation was never put on the thread.
    assert_eq!(told.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Disconnected));
}

/// In a folder that takes changes the operation runs on the thread, under the tree lock.
#[test]
fn a_change_to_a_read_write_folder_runs_on_the_write_operation_thread() {
    let built = build_mount();
    built.kernel_requests.write_requests.access_mode_switch.switch_to_read_write();
    let held = built.tree_lock.try_lock().unwrap();
    let (tell, told) = mpsc::channel();
    built.kernel_requests.write_requests.run_named_operation(OsStr::new("new.txt"), Errno::EINVAL, ToldReply(tell), |_, name, reply| {
        reply.0.send(Answer::OperationRan(name.to_owned())).unwrap();
    });
    assert_eq!(told.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout));
    drop(held);
    assert_eq!(told.recv_timeout(Duration::from_secs(5)), Ok(Answer::OperationRan("new.txt".into())));
}

/// A name that is not text is no name of the folder: refused with the errno the request
/// gives for it, whatever the mode.
#[test]
fn a_name_that_is_not_text_is_refused_before_the_mode_is_asked() {
    let built = build_mount();
    let (tell, told) = mpsc::channel();
    let not_text = OsStr::from_bytes(&[0x66, 0xff, 0x6f]);
    built.kernel_requests.write_requests.run_named_operation(not_text, Errno::ENOENT, ToldReply(tell), |_, _, _| {});
    assert_eq!(told.try_recv(), Ok(Answer::Refused(Errno::ENOENT)));
}

#[test]
fn a_change_the_mount_never_takes_is_refused_as_read_only_in_a_read_only_folder() {
    let built = build_mount();
    let write_requests = &built.kernel_requests.write_requests;
    assert_eq!(write_requests.refuse_unsupported(Errno::ENOTSUP), Errno::EROFS);
    write_requests.access_mode_switch.switch_to_read_write();
    assert_eq!(write_requests.refuse_unsupported(Errno::ENOTSUP), Errno::ENOTSUP);
    assert_eq!(write_requests.refuse_unsupported(Errno::EPERM), Errno::EPERM);
}

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

/// A device, a pipe or a socket has nothing OneDrive could hold.
#[test]
fn mknod_makes_a_regular_file_and_nothing_else() {
    assert_eq!(check_mknod_mode(libc::S_IFREG | 0o644), Ok(()));
    for kind in [libc::S_IFIFO, libc::S_IFSOCK, libc::S_IFCHR, libc::S_IFBLK, libc::S_IFDIR, libc::S_IFLNK] {
        assert_eq!(check_mknod_mode(kind | 0o644), Err(Errno::EPERM));
    }
}

#[test]
fn a_rename_replaces_what_is_at_the_new_place_unless_it_is_told_not_to() {
    let built = build_mount();
    let write_requests = &built.kernel_requests.write_requests;
    assert_eq!(write_requests.find_rename_mode(RenameFlags::empty()), Ok(RenameMode::Replace));
    assert_eq!(write_requests.find_rename_mode(RenameFlags::RENAME_NOREPLACE), Ok(RenameMode::NoReplace));
}

/// A change the mount never takes: refused as a read-only filesystem does in a read-only
/// folder, and as an argument it does not take in a read-write one.
#[test]
fn an_exchange_of_two_names_is_refused() {
    let built = build_mount();
    let write_requests = &built.kernel_requests.write_requests;
    assert_eq!(write_requests.find_rename_mode(RenameFlags::RENAME_EXCHANGE), Err(Errno::EROFS));
    write_requests.access_mode_switch.switch_to_read_write();
    assert_eq!(write_requests.find_rename_mode(RenameFlags::RENAME_EXCHANGE), Err(Errno::EINVAL));
}

/// The folder itself needs no download: the write is put on the thread at once, and waits
/// there for the tree lock.
#[test]
fn a_write_of_what_needs_no_download_runs_on_the_write_operation_thread() {
    let built = build_mount();
    let write_requests = &built.kernel_requests.write_requests;
    write_requests.access_mode_switch.switch_to_read_write();
    let held = built.tree_lock.try_lock().unwrap();
    let (tell, told) = mpsc::channel();
    write_requests.run_with_content(NodeId::ROOT, ToldReply(tell), |_, reply| reply.0.send(Answer::OperationRan("with content".into())).unwrap());
    assert_eq!(told.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout));
    drop(held);
    assert_eq!(told.recv_timeout(Duration::from_secs(5)), Ok(Answer::OperationRan("with content".into())));
}

/// Before anything is downloaded for a write the folder will not take.
#[test]
fn a_write_to_a_read_only_folder_is_refused_before_anything_is_downloaded() {
    let built = build_mount();
    let write_requests = &built.kernel_requests.write_requests;
    let held = built.tree_lock.try_lock().unwrap();
    let (tell, told) = mpsc::channel();
    let a = built.kernel_requests.lookup_requests.find_lookup_answer(false, NodeId::ROOT, OsStr::new("a.txt")).unwrap().0.ino;
    write_requests.run_with_content(NodeId::from(a), ToldReply(tell.clone()), |_, _| panic!("no write in a read-only folder"));
    assert_eq!(told.try_recv(), Ok(Answer::Refused(Errno::EROFS)));
    write_requests.run_without_content(ToldReply(tell), |_, _| panic!("no write in a read-only folder"));
    assert_eq!(told.try_recv(), Ok(Answer::Refused(Errno::EROFS)));
    assert!(!write_requests.file_downloader.is_downloading("A"));
    drop(held);
}

/// A file cut to nothing needs nothing of its content: nothing is downloaded for it.
#[test]
fn a_write_that_cuts_its_file_to_nothing_runs_without_a_download() {
    let built = build_mount();
    let write_requests = &built.kernel_requests.write_requests;
    write_requests.access_mode_switch.switch_to_read_write();
    let (tell, told) = mpsc::channel();
    write_requests.run_without_content(ToldReply(tell), |_, reply| reply.0.send(Answer::OperationRan("without content".into())).unwrap());
    assert_eq!(told.recv_timeout(Duration::from_secs(5)), Ok(Answer::OperationRan("without content".into())));
    assert!(!write_requests.file_downloader.is_downloading("A"));
}

/// The parts of a read-write mount whose `a.txt` is downloaded, with the node id of `a.txt`.
fn build_mount_with_downloaded_file() -> (BuiltMount, NodeId) {
    let built = build_mount();
    built.kernel_requests.write_requests.access_mode_switch.switch_to_read_write();
    built.download("a.txt", "A", b"content");
    let a = built.kernel_requests.lookup_requests.find_lookup_answer(false, NodeId::ROOT, OsStr::new("a.txt")).unwrap().0.ino;
    (built, NodeId::from(a))
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

/// A cycle holds the tree lock; a cut of the file by its path waits on the thread; the
/// program closes its writer, with nothing written. The cut finds the file open and marks it
/// written, and the close that follows it on the thread leaves the cut content to be sent.
#[test]
fn a_writer_closed_behind_a_cut_of_its_file_leaves_the_cut_content_to_be_sent() {
    let (built, node_id) = build_mount_with_downloaded_file();
    let write_requests = &built.kernel_requests.write_requests;
    let writer = queue_open_to_write(&built, node_id).blocking_recv().unwrap().unwrap();
    let held = built.tree_lock.blocking_lock();
    let write_operations = Arc::clone(&write_requests.write_operations);
    let cut = write_requests.write_operation_thread.run_and_wait(move || write_operations.truncate(node_id, 0, None));
    assert_eq!(write_requests.close_file(writer), Some(node_id));
    drop(held);
    assert_eq!(cut.blocking_recv().unwrap(), Ok(()));
    built.run_on_write_operation_thread(|| {});
    assert_eq!(std::fs::metadata(built.disk.join("a.txt")).unwrap().len(), 0);
    let rows = outbox_rows(&built);
    assert_eq!(rows.iter().map(|row| (row.kind, row.item_id.as_deref(), row.reason.clone())).collect::<Vec<_>>(), [(OutboxKind::Update, Some("A"), Some(Reason::Changed))]);
}
