//! The reader's timers: when the folder is walked again, and when the helper
//! is asked again for the directories it did not mark.
//!
//! - **A walk as soon as the queue is drained** ([`Timers::lost`]): an event
//!   was lost (an overflow), or the map is out of step with the disk.
//! - **A walk for a directory the map does not know** ([`Timers::walk_soon`]):
//!   at most once every [`UNKNOWN_WALK`], counted from when the last walk of
//!   the folder began, the bring-up's included; at once when the last one
//!   began longer ago than that.
//! - **A walk on the degraded beat** ([`Timers::degrade`]): every
//!   [`Timing::degraded_scan`] while part of the folder cannot be watched, so
//!   a directory made where no event is raised still gets its `MarkDir`.
//! - **The helper asked again** ([`Timers::uncovered`]):
//!   [`Timing::mark_retry`] after a `MarkDir` failed.
//!
//! Nothing here reads a clock: every call is given the time.

#[cfg(test)]
mod tests;

use std::time::{Duration, Instant};

use super::super::Timing;

/// The shortest time between two walks for a directory the map lost (an
/// event from a handle it does not know, a directory it could not list).
pub const UNKNOWN_WALK: Duration = Duration::from_secs(60);

/// What is due ([`Timers::due`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Due {
    /// The folder is walked again.
    pub walk: bool,
    /// The helper is asked again for what it did not mark.
    pub retry: bool,
}

pub(super) struct Timers {
    degraded_scan: Duration,
    mark_retry: Duration,
    /// Walk again as soon as the queue is drained.
    lost: bool,
    /// Walk again at this time (a directory the map lost).
    walk_at: Option<Instant>,
    last_walk: Option<Instant>,
    /// The next walk of a folder watched only in part.
    degraded_walk: Option<Instant>,
    /// When the helper is asked again.
    retry_at: Option<Instant>,
}

impl Timers {
    pub(super) fn new(timing: &Timing) -> Self {
        Self {
            degraded_scan: timing.degraded_scan,
            mark_retry: timing.mark_retry,
            lost: false,
            walk_at: None,
            last_walk: None,
            degraded_walk: None,
            retry_at: None,
        }
    }

    /// The map lost step with the disk: the folder is walked as soon as the
    /// queue is drained.
    pub(super) fn lost(&mut self) {
        self.lost = true;
    }

    /// A walk for a directory the map does not know, or could not look into:
    /// now if none ran within [`UNKNOWN_WALK`], else when that long has
    /// passed since the last one.
    pub(super) fn walk_soon(&mut self, now: Instant) {
        let earliest = self.last_walk.map_or(now, |at| at + UNKNOWN_WALK);
        self.walk_at.get_or_insert(earliest.max(now));
    }

    /// A walk of the whole folder begins at `now`: what asked for one "as
    /// soon as the queue is drained" has it. A walk asked for by time stays,
    /// since what it is for may be what this walk cannot look into either.
    pub(super) fn walking(&mut self, now: Instant) {
        self.lost = false;
        self.last_walk = Some(now);
    }

    /// Part of the folder cannot be watched: a walk is due
    /// [`Timing::degraded_scan`] from the first call after the last one.
    pub(super) fn degrade(&mut self, now: Instant) {
        self.degraded_walk.get_or_insert(now + self.degraded_scan);
    }

    /// `count` directories wait for the helper's mark: with any, the helper
    /// is asked again [`Timing::mark_retry`] after the first call; with none,
    /// nothing is asked.
    pub(super) fn uncovered(&mut self, now: Instant, count: usize) {
        if count == 0 {
            self.retry_at = None;
        } else {
            self.retry_at.get_or_insert(now + self.mark_retry);
        }
    }

    /// When the next timer runs out. A walk wanted at once is not a timer:
    /// see [`walk_wanted`](Self::walk_wanted).
    pub(super) fn next_wake(&self) -> Option<Instant> {
        [self.degraded_walk, self.walk_at, self.retry_at].into_iter().flatten().min()
    }

    /// A walk is wanted as soon as the queue is drained.
    pub(super) fn walk_wanted(&self) -> bool {
        self.lost
    }

    /// What is due at `now`. The timers that ran out are cleared; a walk
    /// wanted at once stays wanted until one begins ([`walking`](Self::walking)).
    pub(super) fn due(&mut self, now: Instant) -> Due {
        let ran_out = |timer: &mut Option<Instant>| {
            let due = timer.is_some_and(|at| at <= now);
            if due {
                *timer = None;
            }
            due
        };
        let degraded = ran_out(&mut self.degraded_walk);
        let unknown = ran_out(&mut self.walk_at);
        let retry = ran_out(&mut self.retry_at);
        Due { walk: self.lost || degraded || unknown, retry }
    }
}
