//! The requests that change a name, over the parts of a mount built without a mount.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::mpsc;
use std::time::Duration;

use fuser::{Errno, RenameFlags};

use super::check_mknod_mode;
use crate::fuse::kernel_requests::write_requests::tests::{Answer, ToldReply};
use crate::fuse::mount::tests::build_mount;
use crate::fuse::write_operations::RenameMode;

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
