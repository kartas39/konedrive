//! When the window of sent removals is over: a case each.

use super::*;

const LAST: i64 = 1_700_000_000;

fn window(last_removal_at: Option<i64>) -> SentRemovalWindow {
    SentRemovalWindow { last_removal_at, removal_count: 3 }
}

#[test]
fn a_window_in_which_nothing_is_counted_is_over() {
    assert!(SentRemovalWindow::EMPTY.is_over_at(LAST));
    assert!(window(None).is_over_at(0), "a count with no time is of no window");
}

#[test]
fn a_window_is_not_over_within_its_time_of_the_last_removal() {
    assert!(!window(Some(LAST)).is_over_at(LAST));
    assert!(!window(Some(LAST)).is_over_at(LAST + SENT_REMOVAL_WINDOW_SECONDS - 1));
}

#[test]
fn a_window_is_over_once_its_time_passed_since_the_last_removal() {
    assert!(window(Some(LAST)).is_over_at(LAST + SENT_REMOVAL_WINDOW_SECONDS));
    assert!(window(Some(LAST)).is_over_at(LAST + 86_400));
    assert!(window(Some(i64::MIN)).is_over_at(i64::MAX), "no overflow");
}

#[test]
fn a_window_whose_last_removal_is_after_now_is_not_over() {
    assert!(!window(Some(LAST)).is_over_at(LAST - 86_400), "a clock that was set back holds more, never less");
}
