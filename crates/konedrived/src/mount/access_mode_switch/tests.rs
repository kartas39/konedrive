use std::sync::Arc;

use fuser::Errno;

use super::{AccessMode, AccessModeSwitch};
use crate::mount::write_operation_thread::WriteOperationThread;

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
    // The switch is made, and the answer says that the thread is gone.
    assert!(runtime.block_on(access_mode_switch.switch_to_read_only_on_thread(&write_operation_thread)).is_err());
    assert_eq!(access_mode_switch.access_mode(), AccessMode::ReadOnly);
    // Switched to the mode it has, it changes nothing, and says again that the thread is gone.
    assert!(runtime.block_on(access_mode_switch.switch_to_read_only_on_thread(&write_operation_thread)).is_err());
}

#[test]
fn a_folder_held_read_only_for_a_take_down_is_never_switched_to_read_write() {
    // From either mode: the hold says whether the folder took changes before.
    for (mode, took_changes) in [(AccessMode::ReadWrite, true), (AccessMode::ReadOnly, false)] {
        let access_mode_switch = AccessModeSwitch::new(mode);
        assert!(!access_mode_switch.is_held_for_take_down());

        assert_eq!(access_mode_switch.hold_read_only_for_take_down(), took_changes, "{mode:?}");

        assert!(access_mode_switch.is_held_for_take_down());
        assert_eq!(access_mode_switch.access_mode(), AccessMode::ReadOnly);
        assert!(!access_mode_switch.switch_to_read_write(), "the switch is refused");
        assert_eq!(access_mode_switch.check_read_write().err(), Some(Errno::EROFS));
        assert!(!access_mode_switch.switch_to_read_only(), "and a switch to read-only changes nothing");
        assert!(access_mode_switch.is_held_for_take_down(), "nor takes the hold off");
        assert!(!access_mode_switch.hold_read_only_for_take_down(), "held twice, it took no change before the second");
    }
}

#[test]
fn a_switch_to_read_write_that_came_just_before_the_hold_is_undone_by_it() {
    let access_mode_switch = AccessModeSwitch::new(AccessMode::ReadOnly);
    assert!(access_mode_switch.switch_to_read_write());

    assert!(access_mode_switch.hold_read_only_for_take_down(), "it took changes");

    assert_eq!(access_mode_switch.access_mode(), AccessMode::ReadOnly);
    assert!(access_mode_switch.is_held_for_take_down());
}
