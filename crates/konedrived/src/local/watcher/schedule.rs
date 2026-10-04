//! The examiner's schedule: which batch is examined next, and when.
//!
//! What is handed over waits here as one batch. After an examination the
//! schedule keeps what is to be looked at again:
//!
//! - what the sink asked to see again comes back after [`Timing::recheck`];
//! - the entries passed over have **one** recheck pending, however many runs
//!   passed them over. Its wait starts at [`Timing::retry`] and doubles while
//!   a recheck passes any over again, up to [`Timing::degraded_scan`]; a run
//!   beside a pending recheck joins it and leaves its time alone;
//! - a batch the sink could not take yet is offered again after
//!   [`Timing::retry`], with whatever came meanwhile; one it failed on, after
//!   that long doubled at each failure in a row, up to
//!   [`Timing::degraded_scan`]. A flush does not wait for either;
//! - while part of the folder cannot be watched, a Full local scan is due
//!   every [`Timing::degraded_scan`].
//!
//! Nothing here reads a clock or waits: every call is given the time, and
//! [`Schedule::wake_at`] says when the next thing is due. The examiner thread
//! (`examiner`) does the waiting and calls the sink.

#[cfg(test)]
mod tests;

use std::time::{Duration, Instant};

use super::{Handled, Timing, FAILING_AFTER};
use crate::local::{Batch, ScanReason};

/// Whether a batch waits for its retry time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Take {
    /// A batch that could not be examined waits until it is offered again.
    WhenDue,
    /// A flush: what is pending is examined now.
    Now,
}

/// What became of the batch taken last, for the status and for a flush.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Examined,
    /// Not examined, and not a failure: there is nothing to compare with yet.
    NotYet,
    /// Not examined. `said`: it failed [`FAILING_AFTER`] times in a row by
    /// now, which the status says.
    Failed { why: String, wait: Duration, said: bool },
    RootGone,
}

pub(super) struct Schedule {
    timing: Timing,
    /// What is examined next.
    pending: Batch,
    /// Taken and not yet answered for.
    examining: Batch,
    /// `pending` may not be examined before this time (a retry).
    retry_at: Option<Instant>,
    /// Failures in a row.
    failures: u32,
    /// What the sink asked to see again, and when.
    rechecks: Vec<(Instant, Batch)>,
    /// The entries passed over, and when their one recheck is due.
    passed: Batch,
    passed_at: Option<Instant>,
    /// Rechecks in a row that passed something over again.
    passes: u32,
    /// `pending` or `examining` holds the recheck of the passed-over entries.
    rechecking: bool,
    /// The next Full local scan of a folder watched only in part.
    next_scan: Option<Instant>,
}

impl Schedule {
    pub(super) fn new(timing: Timing) -> Self {
        Self {
            timing,
            pending: Batch::new(),
            examining: Batch::new(),
            retry_at: None,
            failures: 0,
            rechecks: Vec::new(),
            passed: Batch::new(),
            passed_at: None,
            passes: 0,
            rechecking: false,
            next_scan: None,
        }
    }

    /// `batch` was handed over: it joins what is examined next.
    pub(super) fn add(&mut self, batch: Batch) {
        self.pending.merge(batch);
    }

    /// Part of the folder cannot be watched: from the first call on, a Full
    /// local scan is due every [`Timing::degraded_scan`].
    pub(super) fn degrade(&mut self, now: Instant) {
        self.next_scan.get_or_insert(now + self.timing.degraded_scan);
    }

    /// The batch to examine at `now`, if any: everything pending and
    /// everything whose time has come, as one. [`done`](Self::done) must be
    /// told what became of it before the next call.
    pub(super) fn take(&mut self, now: Instant, take: Take) -> Option<&Batch> {
        if self.next_scan.is_some_and(|at| at <= now) {
            self.pending.merge(Batch::scan(ScanReason::Periodic));
            self.next_scan = Some(now + self.timing.degraded_scan);
        }
        let (due, waiting) = std::mem::take(&mut self.rechecks).into_iter().partition(|(at, _)| *at <= now);
        self.rechecks = waiting;
        for (_, batch) in due {
            self.pending.merge(batch);
        }
        if self.passed_at.is_some_and(|at| at <= now) {
            self.pending.merge(std::mem::take(&mut self.passed));
            self.passed_at = None;
            self.rechecking = true;
        }
        let held = take == Take::WhenDue && self.retry_at.is_some_and(|at| at > now);
        if self.pending.is_empty() || held {
            return None;
        }
        self.examining = std::mem::take(&mut self.pending);
        Some(&self.examining)
    }

    /// What the sink made of the batch taken last, at `now` (the end of the
    /// examination: every wait counts from there).
    pub(super) fn done(&mut self, handled: Handled, now: Instant) -> Outcome {
        let batch = std::mem::take(&mut self.examining);
        match handled {
            Handled::Done { recheck, passed } => {
                self.retry_at = None;
                self.failures = 0;
                if !recheck.is_empty() {
                    self.rechecks.push((now + self.timing.recheck, recheck));
                }
                if passed.is_empty() {
                    if self.rechecking {
                        self.passes = 0;
                    }
                } else {
                    // A run beside a pending recheck joins it, and leaves its time alone.
                    if self.passed_at.is_none() {
                        self.passes = self.passes.saturating_add(1);
                        self.passed_at = Some(now + self.backoff(self.passes));
                    }
                    self.passed.merge(*passed);
                }
                self.rechecking = false;
                Outcome::Examined
            }
            Handled::NotYet => {
                // Not a failure: what failed before is no longer what holds the batch.
                self.failures = 0;
                self.offer_again(batch, now + self.timing.retry);
                Outcome::NotYet
            }
            Handled::Failed(why) => {
                self.failures = self.failures.saturating_add(1);
                let wait = self.backoff(self.failures);
                self.offer_again(batch, now + wait);
                Outcome::Failed { why, wait, said: self.failures >= FAILING_AFTER }
            }
            Handled::RootGone => Outcome::RootGone,
        }
    }

    /// When [`take`](Self::take) next has something to give, unless more is
    /// handed over first. `None`: nothing waits for a time.
    pub(super) fn wake_at(&self) -> Option<Instant> {
        let retry = self.retry_at.filter(|_| !self.pending.is_empty());
        let recheck = self.rechecks.iter().map(|(at, _)| *at).min();
        [retry, recheck, self.passed_at, self.next_scan].into_iter().flatten().min()
    }

    /// `batch` was not examined: it is pending again, before whatever came
    /// meanwhile, and not offered before `at` unless a flush asks.
    fn offer_again(&mut self, batch: Batch, at: Instant) {
        let came = std::mem::replace(&mut self.pending, batch);
        self.pending.merge(came);
        self.retry_at = Some(at);
    }

    /// The wait after the `nth` time in a row: [`Timing::retry`], doubled
    /// each time, up to [`Timing::degraded_scan`].
    fn backoff(&self, nth: u32) -> Duration {
        self.timing.retry.saturating_mul(1 << nth.min(16).saturating_sub(1)).min(self.timing.degraded_scan)
    }
}
