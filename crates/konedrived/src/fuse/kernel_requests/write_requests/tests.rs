//! The write requests over the parts of a mount built without a mount: what is refused
//! before it is put on the write-operation thread, and what is put there.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::mpsc;
use std::time::Duration;

use fuser::Errno;

use super::ErrnoReply;
use crate::fuse::mount::tests::build_mount;
use crate::fuse::node_table::NodeId;

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
