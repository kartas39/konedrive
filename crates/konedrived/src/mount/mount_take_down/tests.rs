use std::sync::Mutex;

use super::*;
use crate::mount::access_mode_switch::AccessMode;

/// A take-down of a read-write folder, and what was reported.
fn mount_take_down() -> (Arc<AccessModeSwitch>, Arc<Mutex<Vec<TakeDownCause>>>, MountTakeDown) {
    let access_mode_switch = Arc::new(AccessModeSwitch::new(AccessMode::ReadWrite));
    let reported = Arc::new(Mutex::new(Vec::new()));
    let (report_switch, report_reported) = (Arc::clone(&access_mode_switch), Arc::clone(&reported));
    let report: TakeDownReport = Arc::new(move |cause| {
        assert!(!report_switch.is_read_write(), "the folder is read-only before the owner is told");
        report_reported.lock().unwrap().push(cause);
    });
    let mount_take_down = MountTakeDown::new(Arc::clone(&access_mode_switch), report);
    (access_mode_switch, reported, mount_take_down)
}

#[test]
fn a_take_down_asked_for_switches_the_folder_to_read_only_and_reports_once() {
    let (access_mode_switch, reported, mount_take_down) = mount_take_down();
    assert!(!mount_take_down.is_asked() && access_mode_switch.is_read_write());
    assert!(reported.lock().unwrap().is_empty());
    let cause = TakeDownCause::UndoFailed { path: PathBuf::from("docs/a") };
    mount_take_down.ask(cause.clone());
    assert!(mount_take_down.is_asked());
    assert!(!access_mode_switch.is_read_write(), "no further change is taken");
    assert_eq!(*reported.lock().unwrap(), [cause]);
}

#[test]
fn a_second_cause_does_not_report_again() {
    let (access_mode_switch, reported, mount_take_down) = mount_take_down();
    let first = TakeDownCause::StoreFailed { detail: "disk I/O error".into() };
    mount_take_down.ask(first.clone());
    mount_take_down.ask(TakeDownCause::UndoFailed { path: PathBuf::from("docs/a") });
    mount_take_down.ask(first.clone());
    assert_eq!(*reported.lock().unwrap(), [first], "the first cause stands");
    assert!(mount_take_down.is_asked() && !access_mode_switch.is_read_write());
}
