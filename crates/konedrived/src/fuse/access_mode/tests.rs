use fuser::Errno;

use super::AccessMode;

#[test]
fn a_folder_starts_read_only_and_refuses_a_change_as_a_read_only_filesystem() {
    let access_mode = AccessMode::read_only();
    assert!(!access_mode.is_read_write());
    assert_eq!(access_mode.check_read_write().err(), Some(Errno::EROFS));
}

#[test]
fn a_read_write_folder_takes_a_change() {
    let access_mode = AccessMode::read_write();
    assert!(access_mode.is_read_write());
    assert!(access_mode.check_read_write().is_ok());
}

#[test]
fn a_switch_says_whether_it_changed_the_mode() {
    let access_mode = AccessMode::read_only();
    assert!(access_mode.switch_to_read_write());
    assert!(access_mode.is_read_write());
    // Switched again, it changes nothing.
    assert!(!access_mode.switch_to_read_write());
    assert!(access_mode.switch_to_read_only());
    assert!(!access_mode.is_read_write());
    assert!(!access_mode.switch_to_read_only());
}
