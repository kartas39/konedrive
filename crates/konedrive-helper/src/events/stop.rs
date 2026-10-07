//! The ordinary stop: on `SIGTERM` or `SIGINT`, every open the helper holds
//! is answered with an error before the process ends.
//!
//! A process that ends closes its fanotify group, and the kernel then lets
//! every open the group still held through, onto a placeholder nobody
//! filled (`docs/design/hydration.md` §13). A stop that was asked for need
//! not do that: the event loop, which is told of the signal, hands nothing
//! more to a worker and answers what is held `EIO` instead. A crash or a
//! `SIGKILL` runs none of this (limitations log Z1).

use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use konedrive_helper::errno::Errno;
use konedrive_helper::pending::PendingOpen;

use super::{classify_read_failure, owed, ReadFailure};
use crate::shared::{Shared, EXHAUSTION_BACKOFF};

/// How long the stop may take. It waits for nothing outside the helper, so
/// this is far more than it needs and far less than systemd allows a stop
/// (90 s by default) before it kills the process.
const STOP_BOUND: Duration = Duration::from_secs(5);

/// How long the stop sleeps between two looks at the opens other threads
/// still hold.
const STOP_PAUSE: Duration = Duration::from_millis(2);

/// What a stop did.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Stopped {
    /// How many opens got their answer during the stop.
    pub(super) answered: usize,
    /// `None` when every open was answered and a read found the kernel's
    /// queue empty; otherwise how many were still unanswered when the bound
    /// ran out.
    pub(super) left: Option<usize>,
}

/// Stops the helper: answers every open it holds, and returns the status
/// the process exits with.
///
/// Runs on the event loop's thread, in place of the loop, so no open is
/// handed to a worker from here on. Where an open can be held, and what
/// answers it:
///
/// - **queued for a worker:** the workers see `stopping` and deny what they
///   take off the queue;
/// - **in a worker's hands:** the decision is short, and where it would
///   wait — for a daemon that is not connected (`Daemons::wait_for`), or by
///   joining a hydration (`Jobs::enroll`) — it is refused, and the worker
///   denies the open;
/// - **waiting for a daemon's answer:** taken out of the table here
///   (`Jobs::stop`) and denied;
/// - **in a connection's hands**, between the table and the answer its
///   daemon just sent: answered by that thread, as always;
/// - **still in the kernel's queue:** read here and denied.
///
/// None of these is taken on trust: every open read from the group is
/// counted until its answer is written (`Marks::unanswered`), and the stop
/// ends when that count is zero. What the stop answers itself it denies
/// `EIO`; the number its line gives is of every open that got its answer
/// meanwhile, whichever thread wrote it.
pub(super) fn stop(shared: &Shared, signal: u32) -> ExitCode {
    let started = Instant::now();
    shared.stopping.store(true, Ordering::SeqCst);
    shared.daemons.stop();
    let stopped = deny_held(
        STOP_BOUND,
        || shared.jobs.stop(),
        || read_queue(shared),
        |open: PendingOpen, errno| open.deny(errno),
        || shared.marks.unanswered(),
    );
    let answered = stopped.answered;
    match stopped.left {
        None => {
            tracing::info!(
                "stopping on signal {signal}: {answered} open(s) the helper held were answered \
                 in {:?}, and none is left",
                started.elapsed()
            );
            ExitCode::SUCCESS
        }
        Some(left) => {
            tracing::error!(
                "stopping on signal {signal}: {answered} open(s) the helper held were answered, \
                 and {left} more could not be within {STOP_BOUND:?}; the helper exits anyway, \
                 and the kernel lets those through"
            );
            ExitCode::FAILURE
        }
    }
}

/// One read of the kernel's queue for the stop: the opens it handed over,
/// or `None` when it is empty.
fn read_queue(shared: &Shared) -> Option<Vec<PendingOpen>> {
    loop {
        match shared.marks.group().read_events() {
            Ok(events) => {
                return Some(
                    events
                        .into_iter()
                        .filter_map(|event| owed(event, &shared.marks))
                        .map(|(open, _)| open)
                        .collect(),
                )
            }
            Err(e) => match classify_read_failure(e) {
                // A group that cannot be read has nothing more to give.
                ReadFailure::Drained | ReadFailure::Fatal => return None,
                ReadFailure::Interrupted => continue,
                // The kernel answered what it could not hand over itself;
                // the queue may hold more.
                ReadFailure::EventRefused => return Some(Vec::new()),
                ReadFailure::Exhausted => {
                    std::thread::sleep(EXHAUSTION_BACKOFF);
                    return Some(Vec::new());
                }
            },
        }
    }
}

/// Denies every held open `EIO`, for at most `bound`.
///
/// `waiting` takes the opens that wait for a daemon's answer, `queued` is
/// one read of the kernel's queue (`None`: it is empty), `deny` answers one
/// open, and `unanswered` is how many opens read from the kernel have no
/// answer yet, in whoever's hands. It goes round — opens can still be on
/// their way from a worker to their answer — until a read has found the
/// queue empty and nothing is unanswered, or `bound` has passed.
pub(super) fn deny_held<W>(
    bound: Duration,
    mut waiting: impl FnMut() -> Vec<W>,
    mut queued: impl FnMut() -> Option<Vec<W>>,
    mut deny: impl FnMut(W, Errno),
    unanswered: impl Fn() -> usize,
) -> Stopped {
    let deadline = Instant::now() + bound;
    let held = unanswered();
    let mut arrived = 0;
    loop {
        for open in waiting() {
            deny(open, Errno::EIO);
        }
        let mut drained = false;
        while !drained && Instant::now() < deadline {
            match queued() {
                Some(batch) => {
                    arrived += batch.len();
                    for open in batch {
                        deny(open, Errno::EIO);
                    }
                }
                None => drained = true,
            }
        }
        let left = unanswered();
        let answered = (held + arrived).saturating_sub(left);
        if drained && left == 0 {
            return Stopped { answered, left: None };
        }
        if Instant::now() >= deadline {
            return Stopped { answered, left: Some(left) };
        }
        std::thread::sleep(STOP_PAUSE);
    }
}

#[cfg(test)]
mod tests;
