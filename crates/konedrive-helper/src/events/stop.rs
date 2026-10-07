//! The ordinary stop: on `SIGTERM` or `SIGINT`, every open the helper holds
//! is answered with an error before the process ends.
//!
//! A process that ends closes its fanotify group, and the kernel then lets
//! every open the group still held through, onto a placeholder nobody
//! filled (`docs/design/hydration.md` §13). A stop that was asked for need
//! not do that: the event loop, which is told of the signal, hands nothing
//! more to a worker and answers what is held `EIO` instead. A crash or a
//! `SIGKILL` runs none of this (limitations log Z1).

use std::os::fd::AsFd;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::ExitCode;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use konedrive_helper::errno::Errno;
use konedrive_helper::marks::Marks;
use konedrive_helper::pending::PendingOpen;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

use super::{classify_read_failure, owed, ReadFailure};
use crate::shared::{Shared, EXHAUSTION_BACKOFF};

/// How long the stop may take. It waits for nothing outside the helper, so
/// this is far more than it needs and far less than systemd allows a stop
/// (90 s by default) before it kills the process.
const STOP_BOUND: Duration = Duration::from_secs(5);

/// How long the stop sleeps between two looks at the opens other threads
/// still hold.
const STOP_PAUSE: Duration = Duration::from_millis(2);

/// One read of the kernel's queue, for the stop.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Queue<T> {
    /// What the read handed over. It may be nothing while the queue holds
    /// more: read on.
    Read(Vec<T>),
    /// Nothing is queued.
    Empty,
    /// The group cannot be read, and will not be.
    Unreadable(nix::errno::Errno),
}

/// What the stop knows of the kernel's queue when it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueueEnd {
    /// Its last read found the queue empty.
    Empty,
    /// No read found it empty within the bound.
    NeverEmptied,
    /// A read failed for good.
    Unreadable(nix::errno::Errno),
}

/// What a stop did.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Stopped {
    /// How many opens got their answer during the stop.
    pub(super) answered: usize,
    /// How many opens read from the group were still unanswered at the end.
    pub(super) left: usize,
    /// What became of the kernel's queue.
    pub(super) queue: QueueEnd,
    /// The stop panicked and was run again; `answered` is of the second run.
    pub(super) panicked: bool,
}

impl Stopped {
    /// Whether the helper may say it stopped with nothing left: every open
    /// it read has its answer, a read found the kernel's queue empty, and
    /// nothing panicked on the way. Anything else is exit status 1.
    pub(super) fn clean(&self) -> bool {
        self.left == 0 && self.queue == QueueEnd::Empty && !self.panicked
    }

    /// The stop's line in the log, after "stopping on signal N: ". When the
    /// stop is not clean it says each thing that was wrong, apart: opens
    /// left unanswered, a queue that never emptied, a queue that could not
    /// be read, a panic.
    pub(super) fn line(&self, took: Duration) -> String {
        let answered = self.answered;
        if self.clean() {
            return format!(
                "{answered} open(s) the helper held were answered in {took:?}, and none is left"
            );
        }
        let mut line = format!("{answered} open(s) the helper held were answered");
        if self.left > 0 {
            line += &format!(", and {} more could not be within {STOP_BOUND:?}", self.left);
        } else {
            line += ", and none that it had read is left unanswered";
        }
        match self.queue {
            QueueEnd::Empty => {}
            QueueEnd::NeverEmptied => {
                line += &format!(
                    "; the kernel's queue never emptied within {STOP_BOUND:?}: opens kept coming"
                );
            }
            QueueEnd::Unreadable(e) => {
                line += &format!("; the kernel's queue could not be read ({e})");
            }
        }
        if self.panicked {
            line += "; the stop panicked and was run again, and the count is of its second run";
        }
        line + "; the helper exits anyway, and the kernel lets through what has no answer"
    }
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
/// - **still in the kernel's queue:** read here and denied, but for the
///   helper's own opens, which are allowed as the loop allows them
///   (`hand_over`): a connection thread waits in `open_by_handle_at` for
///   each, and an error there is what its daemon would be told.
///
/// None of these is taken on trust: every open read from the group is
/// counted until its answer is written (`Marks::unanswered`), and the stop
/// ends when that count is zero. What the stop answers itself it denies
/// `EIO`; the number its line gives is of every open that got its answer
/// meanwhile, whichever thread wrote it.
///
/// A panic in here is contained like any other on this thread ([`contain`]):
/// let out, it would end the process with opens unanswered, and the kernel
/// allows those.
pub(super) fn stop(shared: &Shared, signal: u32) -> ExitCode {
    let started = Instant::now();
    shared.stopping.store(true, Ordering::SeqCst);
    let own_pid = std::process::id() as i32;
    let group = shared.marks.group();
    let stopped = contain(
        || {
            shared.daemons.stop();
            deny_held(
                STOP_BOUND.saturating_sub(started.elapsed()),
                own_pid,
                || shared.jobs.stop(),
                || match read_queue(|| group.read_events(), || readable(&shared.marks)) {
                    Queue::Read(events) => Queue::Read(
                        events.into_iter().filter_map(|event| owed(event, &shared.marks)).collect(),
                    ),
                    Queue::Empty => Queue::Empty,
                    Queue::Unreadable(e) => Queue::Unreadable(e),
                },
                |open: PendingOpen, errno| open.deny(errno),
                |open: PendingOpen| open.allow(),
                || shared.marks.unanswered(),
            )
        },
        || shared.marks.unanswered(),
    );
    let line = stopped.line(started.elapsed());
    if stopped.clean() {
        tracing::info!("stopping on signal {signal}: {line}");
        ExitCode::SUCCESS
    } else {
        tracing::error!("stopping on signal {signal}: {line}");
        ExitCode::FAILURE
    }
}

/// Runs the stop, and contains a panic in it: the stop is then run once
/// more, so that what the helper holds is still denied and the queue still
/// read out, and its outcome says it panicked. The open in hand when the
/// panic came is denied by its drop (`PendingOpen`). If the second run
/// panics too, `unanswered` says how many opens are left.
pub(super) fn contain(
    mut attempt: impl FnMut() -> Stopped,
    unanswered: impl Fn() -> usize,
) -> Stopped {
    if let Ok(stopped) = catch_unwind(AssertUnwindSafe(&mut attempt)) {
        return stopped;
    }
    match catch_unwind(AssertUnwindSafe(&mut attempt)) {
        Ok(stopped) => Stopped { panicked: true, ..stopped },
        Err(_) => Stopped {
            answered: 0,
            left: catch_unwind(AssertUnwindSafe(unanswered)).unwrap_or(0),
            queue: QueueEnd::NeverEmptied,
            panicked: true,
        },
    }
}

/// Whether the group has something queued, asked without waiting.
///
/// A failure to ask counts as "it has": the next read says what is there,
/// and the stop's bound ends it if nothing does.
fn readable(marks: &Marks) -> bool {
    let mut fds = [PollFd::new(marks.group().as_fd(), PollFlags::POLLIN)];
    poll(&mut fds, PollTimeout::ZERO).map_or(true, |ready| ready > 0)
}

/// One read of the kernel's queue for the stop.
///
/// `EAGAIN` from `read` does not by itself say the queue is empty: it is
/// also what the read returns when the open at the head of the queue is of
/// a leased file, which the kernel then denies itself, leaving the opens
/// behind it queued (`docs/kernel-behavior-7.2/leases.md`; the event loop
/// goes back to `poll` for the same reason). So the group is asked
/// (`readable`), and the queue is empty only when it has nothing to read.
/// Each such `EAGAIN` stands for one event taken off the queue, so reading
/// on cannot spin.
pub(super) fn read_queue<E>(
    mut read: impl FnMut() -> nix::Result<Vec<E>>,
    readable: impl Fn() -> bool,
) -> Queue<E> {
    loop {
        match read() {
            Ok(events) => return Queue::Read(events),
            Err(e) => match classify_read_failure(e) {
                ReadFailure::Drained if readable() => return Queue::Read(Vec::new()),
                ReadFailure::Drained => return Queue::Empty,
                // Nothing says the queue is empty, and nothing more can be
                // read from it.
                ReadFailure::Fatal => return Queue::Unreadable(e),
                ReadFailure::Interrupted => continue,
                // The kernel answered what it could not hand over itself;
                // the queue may hold more.
                ReadFailure::EventRefused => return Queue::Read(Vec::new()),
                ReadFailure::Exhausted => {
                    std::thread::sleep(EXHAUSTION_BACKOFF);
                    return Queue::Read(Vec::new());
                }
            },
        }
    }
}

/// Answers every held open, for at most `bound`.
///
/// `waiting` takes the opens that wait for a daemon's answer, `queued` is
/// one read of the kernel's queue, each open with its opener's pid, `deny`
/// and `allow` answer one open, and `unanswered` is how many opens read
/// from the kernel have no answer yet, in whoever's hands. Every open is
/// denied `EIO` but the helper's own (`own_pid`), which are allowed. It goes
/// round — opens can still be on their way from a worker to their answer —
/// until a read has found the queue empty, or has failed for good, and
/// nothing is unanswered, or `bound` has passed.
pub(super) fn deny_held<W>(
    bound: Duration,
    own_pid: i32,
    mut waiting: impl FnMut() -> Vec<W>,
    mut queued: impl FnMut() -> Queue<(W, i32)>,
    mut deny: impl FnMut(W, Errno),
    mut allow: impl FnMut(W),
    unanswered: impl Fn() -> usize,
) -> Stopped {
    let deadline = Instant::now() + bound;
    let held = unanswered();
    let mut arrived = 0;
    let mut queue = QueueEnd::NeverEmptied;
    loop {
        for open in waiting() {
            deny(open, Errno::EIO);
        }
        // A queue that could not be read is not read again.
        if !matches!(queue, QueueEnd::Unreadable(_)) && Instant::now() < deadline {
            queue = QueueEnd::NeverEmptied;
            loop {
                match queued() {
                    Queue::Read(batch) => {
                        arrived += batch.len();
                        for (open, pid) in batch {
                            if pid == own_pid {
                                allow(open);
                            } else {
                                deny(open, Errno::EIO);
                            }
                        }
                    }
                    Queue::Empty => queue = QueueEnd::Empty,
                    Queue::Unreadable(e) => queue = QueueEnd::Unreadable(e),
                }
                if queue != QueueEnd::NeverEmptied || Instant::now() >= deadline {
                    break;
                }
            }
        }
        let left = unanswered();
        let answered = (held + arrived).saturating_sub(left);
        if (queue != QueueEnd::NeverEmptied && left == 0) || Instant::now() >= deadline {
            return Stopped { answered, left, queue, panicked: false };
        }
        std::thread::sleep(STOP_PAUSE);
    }
}

#[cfg(test)]
mod tests;
