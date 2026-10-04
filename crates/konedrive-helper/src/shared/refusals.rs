use std::sync::Mutex;
use std::time::{Duration, Instant};

use konedrive_helper::jobs;

use super::daemons::{
    DAEMON_WAIT, GLOBAL_MAX_DAEMON_WAITERS, MAX_CONNECTIONS_PER_UID, MAX_DAEMON_WAITERS,
};
use super::registrations::ROOTS_FILE;
use super::{lock, EVENT_QUEUE_DEPTH, EVENT_WORKERS, LOG};

/// How long a repeating condition (descriptor exhaustion, a failing `accept`,
/// a refused open) may go unlogged. All of those can repeat thousands of
/// times a second, and a log line each is a flood that hides the one line
/// anybody needed.
const REPORT_EVERY: Duration = Duration::from_secs(5);

/// How often the refusals' pending counts are looked at, so
/// that the count for the last interval of a burst is written a moment after
/// the interval ends, not whenever — if ever — the next refusal happens.
pub(crate) const FLUSH_EVERY: Duration = Duration::from_secs(1);

/// What the helper says about an intercepted open the kernel could not hand
/// over (see [`Refusal::Unopenable`]).
pub(crate) const UNOPENABLE: &str = "intercepted opens the kernel could not hand over — most likely of a \
                          file some process holds a lease on — and denied EPERM itself";

/// What the helper says about an intercepted open whose descriptor the
/// kernel could not create (see [`Refusal::EventFdFailed`]). The VM suite
/// counts it (`tests/vm/scenarios/punch_rule.rs`, `EVENT_FD_FAILED`); keep the two in
/// step.
pub(crate) const EVENT_FD_FAILED: &str = "the kernel could not open the descriptor of an intercepted open \
                               and denied it EPERM itself";

/// What every throttled line says after the number of occurrences it stands
/// for. The VM suite reads the number off in front of it
/// (`tests/vm/scenarios/harness.rs`, `THROTTLE_MARK`); keep the two in step.
const THROTTLE_MARK: &str = " occurrence(s) since the last line like this";

/// Lets a condition that repeats without end be logged without flooding.
///
/// Descriptor exhaustion in the event loop and a failing `accept` retry on a
/// short backoff, so left alone they would write tens of lines a second for
/// as long as the condition lasts; a burst of opens refused for a genuinely
/// exhausted bound writes one line per open ([`Refusals`]). The first
/// occurrence is always reported immediately; after that at most one line per
/// interval, carrying the number of occurrences it stands for, so the journal
/// shows both that it started and that it is still going — and, through
/// [`flush`](Self::flush) and [`reset`](Self::reset), how many there were in
/// all: the occurrences after the last line are counted too, not dropped.
pub(crate) struct Throttle {
    every: Duration,
    next: Instant,
    since_last: u64,
}

impl Throttle {
    pub(crate) fn new() -> Self {
        Self::every(REPORT_EVERY)
    }

    fn every(every: Duration) -> Self {
        Self { every, next: Instant::now(), since_last: 0 }
    }

    /// How many occurrences this one stands for, or `None` to stay quiet.
    pub(crate) fn admit(&mut self) -> Option<u64> {
        self.since_last += 1;
        if Instant::now() < self.next {
            return None;
        }
        self.next = Instant::now() + self.every;
        Some(std::mem::take(&mut self.since_last))
    }

    /// The occurrences no line has counted yet, once the interval since the
    /// last line is over; `None` while it is not, or if there are none. For
    /// a condition that stopped, this is the only way its last count is ever
    /// written.
    fn flush(&mut self) -> Option<u64> {
        if self.since_last == 0 || Instant::now() < self.next {
            return None;
        }
        self.next = Instant::now() + self.every;
        Some(std::mem::take(&mut self.since_last))
    }

    /// Back to normal: the next occurrence is reported at once. Returns the
    /// occurrences no line had counted yet, for the caller to say so.
    pub(crate) fn reset(&mut self) -> u64 {
        let unreported = self.since_last;
        *self = Self::every(self.every);
        unreported
    }
}

/// The refusals of an intercepted open that a burst can produce by the
/// thousand. Each is a genuinely exhausted bound or a missing
/// daemon, and each was one `warn!` per open — 2400 to 2700 lines per burst
/// in the VM suite, burying the line that said why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The worker pool and its queue are full (`EAGAIN`).
    PoolFull,
    /// The file's owner has no registered root (`EIO`).
    NoRoot,
    /// Too many opens are already waiting for a daemon (`EIO`).
    TooManyWaiters,
    /// The owner's daemon did not connect in time (`EIO`).
    TimedOut,
    /// A hydration request could not be sent (`EIO` or `EAGAIN`).
    Undeliverable,
    /// A `HydrateDone` for nothing this connection was sent — which any
    /// local process can send as fast as it likes.
    StrayDone,
    /// A connection from a uid that already holds
    /// [`MAX_CONNECTIONS_PER_UID`], closed as soon as it was accepted.
    TooManyConnections,
    /// The group was readable and the first read found nothing: the kernel
    /// could not create the descriptor of the event at the head of the
    /// queue and answered it itself (`EPERM`) — a leased file, most likely
    /// — or its opener was killed before it was read.
    Unopenable,
    /// `read()` of the group failed with the errno of the kernel's own open
    /// of one event's descriptor, which it then denied `EPERM`: an open
    /// through a read-only mount (`EROFS`), of an executable
    /// that is running (`ETXTBSY`), and whatever else `dentry_open` can
    /// refuse `O_RDWR` for.
    EventFdFailed,
    /// An open for a uid that already has
    /// [`jobs::MAX_SUSPENDED_OPENS_PER_UID`] waiting for its daemon to
    /// answer (`EAGAIN`).
    TooManySuspended,
    /// A `RegisterRoot` or an `UnregisterRoot` that was refused — which any
    /// local process can send as fast as it likes.
    RootRefused,
    /// A `RegisterRoot` under an id another user holds. Counted apart from
    /// the other refusals, so that a flood of those does not reduce the one
    /// line that names somebody reaching for another user's registration
    /// to a count.
    RootIdTaken,
    /// A registration whose feature probe the helper's own sandbox stopped.
    /// Not a refusal — the registration goes on, on the filesystem type
    /// check — and under the shipped unit the ordinary case of a first
    /// registration; kept here for the throttle, since a peer can have the
    /// line as often as it registers.
    ProbeSkipped,
    /// `roots.json` could not be written. The helper's own trouble, but
    /// while it lasts every registration a peer sends repeats it.
    RootsNotSaved,
}

impl Refusal {
    const ALL: [Refusal; 14] = [
        Refusal::PoolFull,
        Refusal::NoRoot,
        Refusal::TooManyWaiters,
        Refusal::TimedOut,
        Refusal::Undeliverable,
        Refusal::StrayDone,
        Refusal::TooManyConnections,
        Refusal::Unopenable,
        Refusal::EventFdFailed,
        Refusal::TooManySuspended,
        Refusal::RootRefused,
        Refusal::RootIdTaken,
        Refusal::ProbeSkipped,
        Refusal::RootsNotSaved,
    ];

    /// What a line says when the occurrences it counts are not in front of
    /// it — the count written when an interval ends with no new occurrence.
    /// Each keeps the words its per-occurrence line has, so a search for one
    /// finds both.
    fn summary(self) -> String {
        match self {
            Refusal::PoolFull => format!(
                "all {EVENT_WORKERS} workers busy and {EVENT_QUEUE_DEPTH} opens already queued; \
                 opens denied EAGAIN"
            ),
            Refusal::NoRoot => {
                "opens of files whose owner has no registered root, so no daemon of theirs could \
                 hydrate them; denied EIO without waiting"
                    .into()
            }
            Refusal::TooManyWaiters => format!(
                "opens refused a place in a {MAX_DAEMON_WAITERS}-waiter budget or the \
                 machine-wide {GLOBAL_MAX_DAEMON_WAITERS}-waiter backstop; denied EIO at once"
            ),
            Refusal::TimedOut => {
                format!("opens whose owner's daemon did not connect within {DAEMON_WAIT:?}; denied EIO")
            }
            Refusal::Undeliverable => {
                "hydration requests that could not be queued for their daemon; their openers were \
                 denied"
                    .into()
            }
            Refusal::StrayDone => {
                "ignoring HydrateDone for requests that were unknown, already finished, never \
                 sent, or not the sending connection's"
                    .into()
            }
            Refusal::TooManyConnections => format!(
                "connections closed as soon as they were accepted, their uid already holding \
                 {MAX_CONNECTIONS_PER_UID}"
            ),
            Refusal::Unopenable => UNOPENABLE.into(),
            Refusal::EventFdFailed => format!(
                "{EVENT_FD_FAILED} — an open through a read-only mount, or of an executable that \
                 is running, most likely"
            ),
            Refusal::TooManySuspended => format!(
                "opens for a uid that already has {} opens waiting for its daemon to answer; \
                 denied EAGAIN",
                jobs::MAX_SUSPENDED_OPENS_PER_UID
            ),
            Refusal::RootRefused => {
                "requests to register or unregister a root that were refused".into()
            }
            Refusal::RootIdTaken => {
                "requests to register a root id which belongs to another user; refused".into()
            }
            Refusal::ProbeSkipped => {
                "registrations that went on although the helper's own sandbox stopped the \
                 feature probe, relying on the filesystem type check and the daemon's own probe"
                    .into()
            }
            Refusal::RootsNotSaved => format!(
                "registrations and unregistrations refused because the helper cannot save \
                 {ROOTS_FILE}"
            ),
        }
    }
}

/// One [`Throttle`] per kind of [`Refusal`], shared by every thread that
/// refuses anything, and flushed once a [`FLUSH_EVERY`] by a thread of its
/// own so that the count after the last line is always written.
pub(crate) struct Refusals {
    throttles: [Mutex<Throttle>; Refusal::ALL.len()],
}

impl Refusals {
    pub(crate) fn new() -> Self {
        Self { throttles: std::array::from_fn(|_| Mutex::new(Throttle::new())) }
    }

    /// Logs one refusal — the line `describe` builds, with the count it
    /// stands for — or only counts it, if its kind was logged less than an
    /// interval ago. `describe` runs only when a line is written.
    pub(crate) fn report(&self, kind: Refusal, describe: impl FnOnce() -> String) {
        let admitted = lock(&self.throttles[kind as usize]).admit();
        if let Some(occurrences) = admitted {
            tracing::warn!(target: LOG, "{} ({occurrences}{THROTTLE_MARK})", describe());
        }
    }

    /// Writes the count of every kind whose interval is over with
    /// occurrences not yet written.
    pub(crate) fn flush(&self) {
        for kind in Refusal::ALL {
            let pending = lock(&self.throttles[kind as usize]).flush();
            if let Some(occurrences) = pending {
                tracing::warn!(target: LOG, "{} ({occurrences}{THROTTLE_MARK})", kind.summary());
            }
        }
    }
}

#[cfg(test)]
mod tests;
