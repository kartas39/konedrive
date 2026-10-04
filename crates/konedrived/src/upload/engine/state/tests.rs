use std::time::Duration;

use super::{Throttle, Trouble};

const NOW: i64 = 1_700_000_000;

fn secs(n: u64) -> Option<Duration> {
    Some(Duration::from_secs(n))
}

/// §4.10: the wait is as long as OneDrive asked, at least a second and at most an hour,
/// and is over when its second has come.
#[test]
fn a_throttle_lasts_as_long_as_onedrive_asked() {
    let mut throttle = Throttle::new();
    assert_eq!(throttle.until(NOW), None);
    throttle.refused(secs(120), NOW);
    assert_eq!(throttle.until(NOW), Some(NOW + 120));
    assert!(throttle.holds(NOW + 119));
    assert_eq!((throttle.holds(NOW + 120), throttle.until(NOW + 120)), (false, None), "over, and no longer said");

    let mut throttle = Throttle::new();
    throttle.refused(secs(0), NOW);
    assert_eq!(throttle.until(NOW), Some(NOW + 1));
    throttle.refused(secs(100_000), NOW + 5);
    assert_eq!(throttle.until(NOW + 5), Some(NOW + 5 + 3600));
}

/// Of two waits OneDrive named, the later end stands, whichever answer came last.
#[test]
fn a_named_wait_is_never_cut_short_by_another_named_wait() {
    let mut throttle = Throttle::new();
    throttle.refused(secs(120), NOW);
    throttle.refused(secs(1), NOW + 1);
    assert_eq!(throttle.until(NOW + 1), Some(NOW + 120));
    throttle.refused(secs(300), NOW + 2);
    assert_eq!(throttle.until(NOW + 2), Some(NOW + 302));
}

/// With no time named the worker waits 10 s, doubling with each such throttle until a row
/// goes through; rows refused together are one throttle.
#[test]
fn a_wait_with_no_time_named_is_ten_seconds_doubling_once_a_throttle() {
    let mut throttle = Throttle::new();
    throttle.refused(None, NOW);
    throttle.refused(None, NOW);
    throttle.refused(None, NOW + 3);
    assert_eq!(throttle.until(NOW + 3), Some(NOW + 10), "the rows in flight together add nothing");
    throttle.refused(None, NOW + 10);
    assert_eq!(throttle.until(NOW + 10), Some(NOW + 30), "the next throttle waits 20 s");
    throttle.passed();
    throttle.refused(None, NOW + 30);
    assert_eq!(throttle.until(NOW + 30), Some(NOW + 40), "10 s again once a row went through");
}

/// A time OneDrive names takes the place of one the worker chose, even a shorter one; a
/// refusal with no time adds nothing to a named wait.
#[test]
fn a_named_wait_beats_a_chosen_one() {
    let mut throttle = Throttle::new();
    throttle.refused(None, NOW);
    throttle.refused(None, NOW + 10);
    assert_eq!(throttle.until(NOW + 10), Some(NOW + 30));
    throttle.refused(secs(5), NOW + 11);
    assert_eq!(throttle.until(NOW + 11), Some(NOW + 16), "OneDrive said when");
    throttle.refused(None, NOW + 12);
    assert_eq!(throttle.until(NOW + 12), Some(NOW + 16));
    throttle.refused(secs(2), NOW + 13);
    assert_eq!(throttle.until(NOW + 13), Some(NOW + 16), "a named wait is not cut short by a named one");
}

/// The gate's answer is said once for each reason; the folder that could not be opened
/// holds the worker until it is opened.
#[test]
fn each_trouble_is_set_and_cleared_by_its_own_cause() {
    let mut trouble = Trouble::default();
    assert!(!trouble.gate(None));
    assert!(trouble.gate(Some("read-only".into())));
    assert!(!trouble.gate(Some("read-only".into())), "said once");
    assert!(trouble.gate(Some("another drive".into())));
    assert!(!trouble.holds(), "the gate is asked again each time: it holds nothing by itself");
    trouble.folder(Some("Permission denied".into()));
    assert!(trouble.holds());
    assert!(!trouble.gate(None));
    assert_eq!((trouble.holds(), trouble.folder_closed().as_deref()), (true, Some("Permission denied")), "the gate opening does not open the folder");
    trouble.folder(None);
    assert_eq!((trouble.holds(), trouble.folder_closed()), (false, None));
    trouble.sign_out();
    assert!(trouble.holds(), "signed out for good");
}
