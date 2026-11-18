use std::sync::Mutex;
use std::time::Duration;

use super::*;

fn at(start: Instant, seconds: u64) -> Instant {
    start + Duration::from_secs(seconds)
}

#[test]
fn an_answer_of_any_status_makes_microsoft_reachable() {
    let start = Instant::now();
    let unreachable = Reach { is_unreachable: true, answered_at: None };
    assert_eq!(decide_reach(unreachable, RequestEnd::Answered, at(start, 5)), Reach { is_unreachable: false, answered_at: Some(at(start, 5)) });
    let reachable = Reach { is_unreachable: false, answered_at: Some(start) };
    assert_eq!(decide_reach(reachable, RequestEnd::Answered, at(start, 5)), Reach { is_unreachable: false, answered_at: Some(at(start, 5)) }, "the last answer's time is kept");
}

#[test]
fn a_request_with_no_answer_makes_microsoft_unreachable() {
    let start = Instant::now();
    assert_eq!(decide_reach(Reach::default(), RequestEnd::Unanswered { began: start }, at(start, 1)), Reach { is_unreachable: true, answered_at: None }, "no answer ever came");
    let answered = Reach { is_unreachable: false, answered_at: Some(start) };
    assert_eq!(decide_reach(answered, RequestEnd::Unanswered { began: at(start, 2) }, at(start, 3)), Reach { is_unreachable: true, answered_at: Some(start) }, "it began after the last answer");
}

#[test]
fn a_request_that_began_before_the_last_answer_says_nothing() {
    let start = Instant::now();
    // Sent while the network was down, given up after another request was answered.
    let answered = Reach { is_unreachable: false, answered_at: Some(at(start, 10)) };
    assert_eq!(decide_reach(answered, RequestEnd::Unanswered { began: start }, at(start, 15)), answered);
}

#[test]
fn the_observer_is_told_at_once_and_at_every_change_alone() {
    let reachability = Reachability::new();
    let told = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&told);
    reachability.set_observer(Arc::new(move |is_unreachable| record.lock().unwrap().push(is_unreachable)));
    let start = Instant::now();
    reachability.note_at(RequestEnd::Unanswered { began: start }, at(start, 1));
    reachability.note_at(RequestEnd::Unanswered { began: at(start, 1) }, at(start, 2));
    assert!(reachability.is_unreachable());
    reachability.note_at(RequestEnd::Answered, at(start, 3));
    reachability.note_at(RequestEnd::Answered, at(start, 4));
    assert!(!reachability.is_unreachable());
    assert_eq!(*told.lock().unwrap(), [false, true, false], "what holds at once, then each change once");
}
