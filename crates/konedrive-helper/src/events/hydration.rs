//! The "ask the daemon" path: an open joins a hydration, the request goes
//! to the owner's daemon, and the daemon's answer is passed on to every
//! opener that waited for it.

use std::os::fd::AsFd;

use konedrive_fs::placeholder::{read_state, State};
use konedrive_helper::errno::Errno;
use konedrive_helper::jobs::{self, Enrolled, Owner, MAX_SUSPENDED_OPENS_PER_UID};
use konedrive_helper::outbox::{Outbox, Outgoing};
use konedrive_helper::pending::PendingOpen;
use konedrive_proto::ToDaemon;

use super::decision::{file_of, mark_while_hydrated};
use super::LOG;
use crate::shared::{
    NoDaemon, Refusal, Shared, DAEMON_WAIT, GLOBAL_MAX_DAEMON_WAITERS, MAX_DAEMON_WAITERS,
};

/// The "ask the daemon" path: coalesce by inode, register the opener, then
/// send. Registering before sending is the whole point — a daemon that
/// answers immediately would otherwise find an empty job, finish it, and
/// leave this opener suspended with nothing left to answer it.
pub(super) fn hydrate(shared: &Shared, open: PendingOpen, owner_uid: u32, dev: u64, ino: u64, since: u64) {
    let has_root = || shared.roots.has_root_for(owner_uid);
    let daemon = match shared.daemons.wait_for(owner_uid, has_root) {
        Ok(daemon) => daemon,
        Err(why) => {
            // One message per refusal. All three used to print
            // "no daemon for uid X after 30s", which was caught claiming a
            // thirty-second wait for an open that was answered in 185 µs — a
            // log line that sends whoever reads it looking for a daemon that
            // was never going to be asked for.
            //
            // Throttled: each of the three can come thousands
            // at a time, and the first line of an interval keeps the uid.
            match why {
                NoDaemon::NoRoot => shared.refusals.report(Refusal::NoRoot, || {
                    format!(
                        "uid {owner_uid} has no registered root, so no daemon of theirs could \
                         hydrate this file; denying EIO without waiting"
                    )
                }),
                NoDaemon::TooManyWaiters => shared.refusals.report(Refusal::TooManyWaiters, || {
                    format!(
                        "uid {owner_uid}'s own {MAX_DAEMON_WAITERS}-waiter budget, or the \
                         machine-wide {GLOBAL_MAX_DAEMON_WAITERS}-waiter backstop, is already \
                         full; denying this open EIO at once rather than queueing behind them"
                    )
                }),
                NoDaemon::TimedOut => shared.refusals.report(Refusal::TimedOut, || {
                    format!(
                        "uid {owner_uid}'s daemon did not connect within {DAEMON_WAIT:?}; denying \
                         EIO"
                    )
                }),
            }
            open.deny(Errno::EIO);
            return;
        }
    };
    let owner = Owner { uid: daemon.uid, conn: daemon.conn };

    // The event fd itself goes into the job, before anything is sent; the
    // daemon is sent a duplicate, made under the jobs lock (see
    // `jobs::Dispatch`). A `SCM_RIGHTS` copy of either is the same open file
    // description.
    let enrollment = shared.jobs.enroll((dev, ino), owner, open, since);
    for stranded in enrollment.evicted {
        let errno = match enrollment.outcome {
            Enrolled::ConnectionGone => {
                // This connection's cleanup already ran, so nothing
                // would ever answer a job created on it.
                tracing::warn!(
                    target: LOG,
                    "the daemon connection went away while this open was being handled"
                );
                Errno::EIO
            }
            Enrolled::TooMany => {
                // Throttled: a daemon that answers nothing turns every open
                // of its user's placeholders into one of these.
                shared.refusals.report(Refusal::TooManySuspended, || {
                    format!(
                        "uid {} already has {MAX_SUSPENDED_OPENS_PER_UID} opens waiting for its \
                         daemon to answer; denying this one EAGAIN",
                        owner.uid
                    )
                });
                Errno::EAGAIN
            }
            _ => {
                tracing::warn!(
                    target: LOG,
                    "a hydration of this file was in hand for another uid, which no longer owns \
                     it; denying its openers EIO"
                );
                Errno::EIO
            }
        };
        stranded.deny(errno);
    }
    // `New` comes with its request to send. `Queued` has none yet: the
    // opener is enrolled and stays suspended until a returning credit sends
    // it. `Existing` asked for nothing, and `ConnectionGone` and `TooMany`
    // were answered above.
    dispatch(shared, &daemon.outbox, owner, enrollment.dispatch);
}

/// Sends a hydration request that has just been given a credit — and, if it
/// cannot be sent, answers its openers and passes the credit on, for as long
/// as the next one cannot be sent either.
///
/// Queued, never written here: a worker thread must not be able
/// to block on a socket the peer controls. The request's room in the outbox
/// is its credit, so a refusal is not a slow daemon: the
/// connection is over — `EIO`, as its disconnect guard answers everything
/// else it had — or a peer that answered a request before it was sent
/// returned a credit early, and its own openers get `EAGAIN`. A descriptor
/// that cannot be duplicated (the helper is out of them) is `EIO`, as it
/// always was.
///
/// A loop, not recursion: when every send fails, as it does once the
/// connection is over, the whole queue drains through here one hydration at
/// a time.
pub(crate) fn dispatch(shared: &Shared, outbox: &Outbox, owner: Owner, mut next: Option<jobs::Dispatch>) {
    while let Some(jobs::Dispatch { req_id, fd }) = next.take() {
        let (errno, why) = match fd {
            Ok(fd) => {
                let request = Outgoing { message: ToDaemon::HydrateRequest { req_id }, fd: Some(fd) };
                match outbox.try_send(request) {
                    Ok(()) => return,
                    Err(_) if outbox.is_closed() => (Errno::EIO, "the connection is over".to_owned()),
                    Err(_) => (Errno::EAGAIN, "its request capacity is taken".to_owned()),
                }
            }
            Err(e) => (Errno::EIO, format!("cannot duplicate an event fd for the daemon: {e}")),
        };
        shared.refusals.report(Refusal::Undeliverable, || {
            format!(
                "a hydration request could not be queued for uid {} connection {} ({why}); \
                 denying its openers errno {errno}",
                owner.uid, owner.conn
            )
        });
        // Every opener that has joined this job is answered, not just the
        // first: they are all waiting on a request that was never delivered.
        next = settle(shared, req_id, owner, Err(errno), Finish::Undeliverable);
    }
}

/// What brought us into [`settle`]. It changes nothing about what the function
/// does and everything about what "there is no such job" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Finish {
    /// The daemon sent `HydrateDone`.
    Reported,
    /// The helper is draining a request it could not deliver to the daemon in
    /// the first place. No `HydrateDone` was ever involved, and saying one was
    /// sends the reader looking for a message that does not exist.
    Undeliverable,
}

/// Answers every opener waiting on a finished hydration, and returns the
/// hydration its credit now goes to, whose request the caller
/// must send — see [`dispatch`].
pub(crate) fn settle(
    shared: &Shared,
    req_id: u64,
    owner: Owner,
    outcome: Result<(), Errno>,
    why: Finish,
) -> Option<jobs::Dispatch> {
    let Some(jobs::Finished { waiters, since, next }) = shared.jobs.finish(req_id, owner)
    else {
        // Unknown, already finished, or another connection's. A request id is
        // a small sequential integer, so "another connection's" is the case
        // that matters: without this check any local user could connect to the
        // 0666 socket and force-allow every suspended open in the system by
        // guessing numbers from 1 upwards.
        match why {
            Finish::Reported => shared.refusals.report(Refusal::StrayDone, || {
                format!(
                    "ignoring HydrateDone for request {req_id} from uid {} connection {}: \
                     unknown, already finished, never sent, or not this connection's",
                    owner.uid, owner.conn
                )
            }),
            Finish::Undeliverable => tracing::warn!(
                target: LOG,
                "request {req_id} for uid {} connection {} could not be sent to the daemon, and \
                 by the time it was drained it was already gone; its openers were answered \
                 elsewhere",
                owner.uid,
                owner.conn
            ),
        }
        return None;
    };
    answer(shared, req_id, waiters, outcome, since);
    next
}

/// Answers the openers of one hydration with its outcome. `since` is the
/// count of root unregistrations when the open that started it was read.
fn answer(
    shared: &Shared,
    req_id: u64,
    waiters: Vec<PendingOpen>,
    outcome: Result<(), Errno>,
    since: u64,
) {
    if let Err(errno) = outcome {
        let delivered = errno.deliverable();
        if delivered != errno {
            tracing::warn!(
                target: LOG,
                "the daemon reported errno {errno} for request {req_id}, which the kernel will \
                 not deliver; denying with {delivered} instead"
            );
        }
        for open in waiters {
            open.deny(delivered);
        }
        return;
    }

    // The ignore mark goes on only after the file's state has
    // been read again, from the event fd itself — the exact inode the opener
    // is about to get, with no path in between and so nothing to race. A
    // hydration that reports success but does not leave the file `hydrated`
    // has not put the content there as far as we can tell, and `docs/design/hydration.md` §5.1 is
    // unconditional about what happens then.
    //
    // The mark then goes through `mark_while_hydrated`, like every other
    // mark: a dehydration that began after the read above
    // takes it off again here, and a hydration that began before a root was
    // unregistered leaves no mark behind it.
    let Some(first) = waiters.first() else { return };
    let verdict = read_state(first);
    let verdict = match verdict {
        Ok(Some(State::Hydrated)) => {
            let file = file_of(first.as_fd());
            mark_while_hydrated(shared, first.as_fd(), file, since).map_err(|now| {
                tracing::error!(
                    target: LOG,
                    "request {req_id}: the file read hydrated, and then {now:?} once it was \
                     marked — a dehydration began in between"
                );
                now
            })
        }
        other => Err(other),
    };
    match verdict {
        Ok(()) => {
            for open in waiters {
                open.allow();
            }
        }
        Err(other) => {
            tracing::error!(
                target: LOG,
                "request {req_id} was reported successful but the file does not read as \
                 hydrated ({other:?}); denying EIO rather than risk serving zeros"
            );
            for open in waiters {
                open.deny(Errno::EIO);
            }
        }
    }
}
