//! The write requests over the parts of a mount built without a mount: how a request gets
//! onto the write-operation thread, and what is refused before it does.

use std::ffi::OsStr;
use std::sync::mpsc;
use std::time::Duration;

use fuser::Errno;

use super::ErrnoReply;
use crate::fuse::mount::tests::build_mount;
use crate::fuse::node_table::NodeId;

/// What a request was answered: the errno it was refused with, or that its operation ran.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Answer {
    Refused(Errno),
    OperationRan(String),
}

/// A reply that tells the test what it was answered.
pub(super) struct ToldReply(pub(super) mpsc::Sender<Answer>);

impl ErrnoReply for ToldReply {
    fn refuse(self, errno: Errno) {
        self.0.send(Answer::Refused(errno)).unwrap();
    }
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
    write_requests.run_in_read_write_folder(ToldReply(tell), |_, _| panic!("no write in a read-only folder"));
    assert_eq!(told.try_recv(), Ok(Answer::Refused(Errno::EROFS)));
    assert!(!write_requests.file_downloader.is_downloading("A"));
    drop(held);
}

/// A file truncated to nothing needs nothing of its content: nothing is downloaded for it.
#[test]
fn a_write_that_truncates_its_file_to_nothing_runs_without_a_download() {
    let built = build_mount();
    let write_requests = &built.kernel_requests.write_requests;
    write_requests.access_mode_switch.switch_to_read_write();
    let (tell, told) = mpsc::channel();
    write_requests.run_in_read_write_folder(ToldReply(tell), |_, reply| reply.0.send(Answer::OperationRan("without content".into())).unwrap());
    assert_eq!(told.recv_timeout(Duration::from_secs(5)), Ok(Answer::OperationRan("without content".into())));
    assert!(!write_requests.file_downloader.is_downloading("A"));
}
