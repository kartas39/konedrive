use std::sync::Arc;

use fuser::Errno;

use super::{AccessMode, AccessModeSwitch};
use crate::fuse::write_operation_thread::WriteOperationThread;

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

#[test]
fn a_switch_on_a_thread_that_is_stopped_is_made_at_once() {
    let access_mode_switch = Arc::new(AccessModeSwitch::new(AccessMode::ReadWrite));
    let write_operation_thread = WriteOperationThread::start(Arc::default()).unwrap();
    write_operation_thread.stop();
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    assert!(runtime.block_on(access_mode_switch.switch_to_read_only_on_thread(&write_operation_thread)));
    assert_eq!(access_mode_switch.access_mode(), AccessMode::ReadOnly);
    // Switched to the mode it has, it changes nothing.
    assert!(!runtime.block_on(access_mode_switch.switch_to_read_only_on_thread(&write_operation_thread)));
}
