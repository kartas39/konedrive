use std::sync::{mpsc, Arc};
use std::time::Duration;

use fuser::Errno;
use konedrive_tree::outbox::OutboxKind;

use super::{AccessMode, AccessModeSwitch};
use crate::fuse::node_table::NodeId;
use crate::fuse::write_operation_thread::WriteOperationThread;
use crate::fuse::write_operations::tests::{at, folder};
use crate::fuse::write_operations::EntryToMake;

#[test]
fn a_read_only_folder_refuses_a_change_as_a_read_only_filesystem() {
    let access_mode_switch = AccessModeSwitch::new(AccessMode::ReadOnly);
    assert_eq!(access_mode_switch.access_mode(), AccessMode::ReadOnly);
    assert_eq!(access_mode_switch.check_read_write().err(), Some(Errno::EROFS));
}

#[test]
fn a_read_write_folder_takes_a_change() {
    let access_mode_switch = AccessModeSwitch::new(AccessMode::ReadWrite);
    assert_eq!(access_mode_switch.access_mode(), AccessMode::ReadWrite);
    assert!(access_mode_switch.check_read_write().is_ok());
}

#[test]
fn a_switch_says_whether_it_changed_the_mode() {
    let access_mode_switch = AccessModeSwitch::new(AccessMode::ReadOnly);
    assert!(access_mode_switch.switch_to_read_write());
    assert_eq!(access_mode_switch.access_mode(), AccessMode::ReadWrite);
    assert!(access_mode_switch.check_read_write().is_ok());
    assert!(!access_mode_switch.switch_to_read_write());
    assert!(access_mode_switch.switch_to_read_only());
    assert_eq!(access_mode_switch.access_mode(), AccessMode::ReadOnly);
    assert!(!access_mode_switch.switch_to_read_only());
}

/// A cycle of the delta listing holds the tree lock: an operation asked for before the switch
/// is done and recorded before the switch is made, one asked for after it is refused, and
/// nothing is recorded once the switch is made.
#[test]
fn an_operation_queued_before_a_switch_to_read_only_is_done_before_it_and_one_after_it_is_refused() {
    let folder = Arc::new(folder(Vec::new()));
    let tree_lock = Arc::new(tokio::sync::Mutex::new(()));
    let write_operation_thread = WriteOperationThread::start(Arc::clone(&tree_lock)).unwrap();
    let held = tree_lock.try_lock().unwrap();
    let (tell, told) = mpsc::channel();
    let make = |name: &'static str| {
        let (folder, tell) = (Arc::clone(&folder), tell.clone());
        write_operation_thread.run(move || tell.send((name, folder.ops.make(NodeId::ROOT, name, EntryToMake::File).err())).unwrap());
    };
    make("before.txt");
    let switched = folder.access_mode_switch.queue_switch(&write_operation_thread, AccessMode::ReadOnly);
    make("after.txt");
    // While the cycle holds the lock nothing ran: the folder still takes changes.
    assert_eq!(folder.access_mode_switch.access_mode(), AccessMode::ReadWrite);
    assert!(folder.rows().is_empty());
    drop(held);
    assert_eq!(switched.blocking_recv(), Ok(true));
    // The switch is made: what was asked for before it is recorded already.
    assert_eq!(folder.access_mode_switch.access_mode(), AccessMode::ReadOnly);
    assert_eq!(folder.said(), [(OutboxKind::Create, None, at("before.txt"))]);
    assert_eq!(told.recv_timeout(Duration::from_secs(5)), Ok(("before.txt", None)));
    assert_eq!(told.recv_timeout(Duration::from_secs(5)), Ok(("after.txt", Some(Errno::EROFS))));
    // And nothing was recorded after it.
    assert_eq!(folder.said(), [(OutboxKind::Create, None, at("before.txt"))]);
    assert!(!folder.disk.join("after.txt").exists());
}

#[test]
fn a_switch_on_a_thread_that_is_stopped_is_made_at_once() {
    let access_mode_switch = Arc::new(AccessModeSwitch::new(AccessMode::ReadWrite));
    let write_operation_thread = WriteOperationThread::start(Arc::default()).unwrap();
    write_operation_thread.stop();
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    assert!(runtime.block_on(access_mode_switch.switch_on_thread(&write_operation_thread, AccessMode::ReadOnly)));
    assert_eq!(access_mode_switch.access_mode(), AccessMode::ReadOnly);
    // Switched to the mode it has, it changes nothing.
    assert!(!runtime.block_on(access_mode_switch.switch_on_thread(&write_operation_thread, AccessMode::ReadOnly)));
}
