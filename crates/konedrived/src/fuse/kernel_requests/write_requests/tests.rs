//! The write requests over the parts of a mount built without a mount: what is refused
//! before it is put on the write-operation thread, and what is put there.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::mpsc;
use std::time::Duration;

use fuser::Errno;

use super::ErrnoReply;
use crate::fuse::mount::tests::build_mount;

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
