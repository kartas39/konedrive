//! How long the listing's poller waits: after a cycle, until the next one is due; and
//! while the account's work is stopped, until a timed pause ends. The two decisions of
//! the poller's task, as functions of what the schedule, the last cycle, the notification
//! socket and the account's clock said.
//!
//! **State owned:** none.
//!
//! **Tested without the rest**, a test per case:
// test: after_a_cycle_that_ended_the_next_is_due_after_the_interval
// test: while_the_notification_socket_is_up_the_next_cycle_is_due_after_the_live_interval
// test: after_a_failure_the_next_cycle_is_due_by_the_retry_schedule
// test: beyond_the_retry_schedule_a_failed_cycle_is_tried_again_after_the_interval
// test: a_failed_cycle_is_tried_again_by_the_retry_schedule_whatever_the_socket
// test: a_timed_pause_is_waited_for_until_its_end_and_no_less_than_a_second
// test: a_pause_with_no_end_and_a_hold_are_not_waited_for_by_the_clock

use std::time::Duration;

use crate::conditions::running::Stop;
use crate::remote::listing_poller::Schedule;

/// How the last cycle ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastCycle {
    /// Whole, or with a page of a listing the store gave up: no failure.
    Ended,
    /// It failed, after so many failures in a row before it.
    Failed { failures_before: usize },
}

/// Whether the notification socket is up: while it is, OneDrive says when something
/// changed, and the poll is the safety net alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationSocket {
    Up,
    Down,
}

/// How long after the end of the last cycle the next one is due. Pure. After a failure,
/// the wait the retry schedule has for it, and beyond the schedule's end the ordinary
/// interval, whatever the socket; after a cycle that ended, the interval, or the longer
/// one while the socket is up.
pub fn decide_cycle_wait(schedule: &Schedule, last_cycle: LastCycle, socket: NotificationSocket) -> Duration {
    match (last_cycle, socket) {
        (LastCycle::Failed { failures_before }, _) => schedule.retry.get(failures_before).copied().unwrap_or(schedule.interval),
        (LastCycle::Ended, NotificationSocket::Up) => schedule.live_interval,
        (LastCycle::Ended, NotificationSocket::Down) => schedule.interval,
    }
}

/// What a stopped poller waits for beside a refresh and the poll's interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseWait {
    /// A timed pause: until the account's clock reads this (unix seconds).
    UntilClock(i64),
    /// A pause until resumed, or the automatic hold: nothing the clock says ends it.
    NotByClock,
}

/// What ends the stop `stop` by the clock, which reads `now`. Pure. A timed pause ends at
/// its time, and is looked at again no sooner than in a second: a pause whose time has
/// passed by a clock that was set does not spin the task.
pub fn decide_pause_wait(stop: Stop, now: i64) -> PauseWait {
    match stop {
        Stop::Paused(until) if until > 0 => PauseWait::UntilClock(until.max(now.saturating_add(1))),
        Stop::Paused(_) | Stop::Held(_) => PauseWait::NotByClock,
    }
}

#[cfg(test)]
mod tests;
