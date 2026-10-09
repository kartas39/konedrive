use fuser::Errno;

use super::{AccessMode, AccessModeSwitch};

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
