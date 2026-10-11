use std::time::Duration;

use super::*;
use crate::conditions::running::Hold;

const INTERVAL: Duration = Duration::from_secs(60);
const LIVE_INTERVAL: Duration = Duration::from_secs(300);
const RETRY: [Duration; 2] = [Duration::from_secs(5), Duration::from_secs(15)];

fn schedule() -> Schedule {
    Schedule { interval: INTERVAL, retry: RETRY.to_vec(), live_interval: LIVE_INTERVAL, live: None }
}

#[test]
fn after_a_cycle_that_ended_the_next_is_due_after_the_interval() {
    assert_eq!(decide_cycle_wait(&schedule(), LastCycle::Ended, NotificationSocket::Down), INTERVAL);
}

#[test]
fn while_the_notification_socket_is_up_the_next_cycle_is_due_after_the_live_interval() {
    assert_eq!(decide_cycle_wait(&schedule(), LastCycle::Ended, NotificationSocket::Up), LIVE_INTERVAL);
}

#[test]
fn after_a_failure_the_next_cycle_is_due_by_the_retry_schedule() {
    assert_eq!(decide_cycle_wait(&schedule(), LastCycle::Failed { failures_before: 0 }, NotificationSocket::Down), RETRY[0]);
    assert_eq!(decide_cycle_wait(&schedule(), LastCycle::Failed { failures_before: 1 }, NotificationSocket::Down), RETRY[1]);
}

#[test]
fn beyond_the_retry_schedule_a_failed_cycle_is_tried_again_after_the_interval() {
    assert_eq!(decide_cycle_wait(&schedule(), LastCycle::Failed { failures_before: 2 }, NotificationSocket::Down), INTERVAL);
    assert_eq!(decide_cycle_wait(&schedule(), LastCycle::Failed { failures_before: 9 }, NotificationSocket::Up), INTERVAL);
    let no_retries = Schedule { retry: Vec::new(), ..schedule() };
    assert_eq!(decide_cycle_wait(&no_retries, LastCycle::Failed { failures_before: 0 }, NotificationSocket::Down), INTERVAL);
}

#[test]
fn a_failed_cycle_is_tried_again_by_the_retry_schedule_whatever_the_socket() {
    assert_eq!(decide_cycle_wait(&schedule(), LastCycle::Failed { failures_before: 0 }, NotificationSocket::Up), RETRY[0]);
}

#[test]
fn a_timed_pause_is_waited_for_until_its_end_and_no_less_than_a_second() {
    assert_eq!(decide_pause_wait(Stop::Paused(1_500), 1_000), PauseWait::UntilClock(1_500));
    // Its time has passed, or is this very second: looked at again in a second.
    assert_eq!(decide_pause_wait(Stop::Paused(900), 1_000), PauseWait::UntilClock(1_001));
    assert_eq!(decide_pause_wait(Stop::Paused(1_000), 1_000), PauseWait::UntilClock(1_001));
}

#[test]
fn a_pause_with_no_end_and_a_hold_are_not_waited_for_by_the_clock() {
    assert_eq!(decide_pause_wait(Stop::Paused(0), 1_000), PauseWait::NotByClock);
    assert_eq!(decide_pause_wait(Stop::Held(Hold::Metered), 1_000), PauseWait::NotByClock);
}
