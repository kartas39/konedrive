//! Coalescing: many processes opening the same file wait on one hydration.
//!
//! This module also owns the suspended openers themselves. That is deliberate
//!: while the job table and the list of descriptors to answer
//! lived in two separate maps behind two separate locks, "claim the job" and
//! "register as a waiter" could not be made one atomic step, and a daemon
//! that answered quickly could run `finish` in the gap — dropping a job whose
//! only waiter had not been recorded yet, and leaving that opener suspended
//! for good. With the descriptors inside the job there is one lock, and
//! `enroll` is that single step.
//!
//! Every job also records **who** it belongs to: the uid *and* the particular
//! daemon connection that was asked to do the work. Nothing else in the
//! helper knew that before, and four separate holes came out of it — any
//! local user could finish another user's hydration by guessing a small
//! integer, and any local user could fail every hydration on the machine by
//! connecting and disconnecting. Ownership is checked in one place here
//! instead of being spot-fixed at each call site.
//!
//! And every job is either **sent** or **queued**. A connection
//! has a credit of [`MAX_OUTSTANDING_HYDRATIONS`] requests handed to its
//! daemon and not yet answered — the contract that keeps the daemon's reader
//! from ever stopping with an `Ack` stuck behind a request (see that
//! constant). A new hydration beyond the credit is enrolled exactly like any
//! other, its openers suspended the same way, but its request is not sent;
//! each `HydrateDone` that returns a credit sends the oldest queued one in its
//! place, in the same step. The 65th concurrent hydration used to be refused
//! `EAGAIN` instead, which is what a folder of 200 photos being thumbnailed
//! turned into 136 "Resource temporarily unavailable"s.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::os::fd::OwnedFd;

use konedrive_proto::MAX_OUTSTANDING_HYDRATIONS;

/// How many retired connection ids are remembered.
///
/// A connection can only be enrolled against after it died by a worker that
/// was already holding a `Daemon` clone when the cleanup ran — a window of
/// microseconds, and one that closes the moment that worker finishes. A
/// thousand connections of slack is therefore enormous, and the bound is
/// what stops a machine that reconnects daemons all day from growing this
/// set without end.
const RETIRED_REMEMBERED: usize = 1024;

/// Who a job belongs to: a uid, and the specific connection from that uid.
///
/// The connection matters as much as the uid. A daemon that reconnects is a
/// new connection with the same uid, and it has no idea what request ids the
/// previous one handed out; letting it answer them would be the same bug as
/// letting a stranger answer them, only harder to notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub conn: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Enrolled {
    /// This caller created the hydration and it has credit: its request must
    /// be sent now — [`Enrollment::dispatch`].
    New { req_id: u64 },
    /// This caller created the hydration beyond its connection's credit
    ///. The opener is enrolled and stays suspended; the request
    /// goes to the daemon when a credit returns, handed out by
    /// [`Jobs::finish`]. Nothing to send now.
    Queued { req_id: u64 },
    /// Someone already created it, sent or queued; this opener just waits.
    Existing { req_id: u64 },
    /// The connection this job would have belonged to has already gone away.
    /// The descriptor comes back in `Enrollment::evicted`; the caller must
    /// answer it.
    ConnectionGone,
}

/// A hydration whose request must go to its daemon now, because it was just
/// given one of its connection's credits.
///
/// Whoever receives one owns that credit and must either send the request or
/// hand the credit back with [`Jobs::finish`] — which also answers its openers
/// and passes the credit on to the next queued hydration. Dropping it keeps
/// the credit taken until the connection ends.
#[must_use]
#[derive(Debug)]
pub struct Dispatch {
    pub req_id: u64,
    /// A duplicate of one waiter's event fd, for the daemon to fill through.
    /// Made here, under the lock that keeps the event fd alive, so the number
    /// it duplicates cannot have been answered, closed and reused. An error
    /// (the helper is out of descriptors) leaves the request unsendable.
    pub fd: io::Result<OwnedFd>,
}

/// The result of enrolling one suspended open.
///
/// `evicted` carries the waiters of a job another uid's daemon was asked for
/// on the same inode — see `Jobs::enroll` — or, for
/// [`Enrolled::ConnectionGone`], the opener's own descriptor. The caller must
/// answer them; they are handed back rather than dropped because dropping an
/// event fd without writing a response leaves its opener blocked until the
/// helper exits.
#[must_use]
pub struct Enrollment {
    pub outcome: Enrolled,
    pub evicted: Vec<OwnedFd>,
    /// For [`Enrolled::New`] only: the request to send.
    pub dispatch: Option<Dispatch>,
}

/// A hydration its daemon has answered.
#[must_use]
pub struct Finished {
    /// Every opener waiting on it, to be answered.
    pub waiters: Vec<OwnedFd>,
    /// The helper's count of root unregistrations when the open that
    /// created this job was read (second guard): a job that
    /// began before an unregistration may be for a file whose tree that
    /// unregistration's walk has already passed, and gets no ignore mark.
    pub since: u64,
    /// The oldest hydration that was waiting for its connection's credit, now
    /// holding the one this returned. Its request must be sent.
    pub next: Option<Dispatch>,
}

struct Job {
    inode: (u64, u64),
    owner: Owner,
    /// The exact descriptors `read_events()` handed out, never duplicates of
    /// them — the kernel matches a permission response by fd *number*
    /// (`docs/kernel-behavior-7.2.md` §5.1).
    waiters: Vec<OwnedFd>,
    /// Whether its request has been handed out — whether it holds one of its
    /// connection's credits. A job that is not sent is in `queued`.
    sent: bool,
    /// See [`Finished::since`]. The creating open's, which is the earliest:
    /// an opener that joins later was read later.
    since: u64,
}

#[derive(Default)]
pub struct Jobs {
    next_id: u64,
    by_inode: HashMap<(u64, u64), u64>,
    jobs: HashMap<u64, Job>,
    /// Connections whose cleanup has already run, newest last.
    retired: HashSet<u64>,
    retired_order: VecDeque<u64>,
    /// How many of each connection's credits are taken: its sent jobs, the
    /// number [`MAX_OUTSTANDING_HYDRATIONS`] bounds. A connection with none
    /// holds no entry. Kept in step by [`insert_job`](Self::insert_job),
    /// [`promote`](Self::promote) and [`remove_job`](Self::remove_job), the
    /// only places a job is sent, comes or goes.
    outstanding: HashMap<u64, usize>,
    /// Each connection's jobs waiting for credit, oldest first.
    /// An id whose job has since gone — evicted by another uid's hydration of
    /// the same inode — is skipped when its turn comes. A connection with
    /// nothing queued holds no entry.
    ///
    /// Only ever non-empty for a connection whose credits are all taken:
    /// every credit that comes back is handed straight to the head of this
    /// queue in the same step, so a new hydration never finds a free credit
    /// with older ones still waiting, and arrival order is kept.
    queued: HashMap<u64, VecDeque<u64>>,
}

impl Jobs {
    /// Records one suspended open against the hydration of its inode,
    /// creating the job if this is the first opener. The descriptor is stored
    /// before this returns, so a `finish` that arrives immediately afterwards
    /// always sees it.
    ///
    /// A connection that has already been retired is refused. A
    /// worker that took its `Daemon` clone out of `wait_for_daemon` a moment
    /// before that connection's cleanup ran would otherwise create a job
    /// nobody is left to finish: usually the send that follows fails and
    /// drains it, but when the connection ended on a *deserialisation* error
    /// the peer socket is still open, the send succeeds, and — because §5.2
    /// deliberately puts no time limit on a hydration — the waiters stay
    /// suspended until the helper exits.
    ///
    /// A new job takes one of its connection's credits and comes back with
    /// the request to send ([`Enrolled::New`]), or, when they are all taken,
    /// is queued behind the others ([`Enrolled::Queued`]).
    /// Nobody is refused for want of credit; joining a job that already
    /// exists, sent or queued, asks the daemon for nothing more.
    ///
    /// `since` is the helper's count of root unregistrations when this open
    /// was read (see [`Finished::since`]); it is recorded only if the open
    /// creates the job.
    pub fn enroll(&mut self, inode: (u64, u64), owner: Owner, fd: OwnedFd, since: u64) -> Enrollment {
        let mut evicted = Vec::new();
        if self.retired.contains(&owner.conn) {
            return Enrollment {
                outcome: Enrolled::ConnectionGone,
                evicted: vec![fd],
                dispatch: None,
            };
        }
        if let Some(&req_id) = self.by_inode.get(&inode) {
            // Joined whichever of the uid's connections has it in hand, not
            // only the one this open was routed to. A uid can
            // have several live connections, and hydrations go to the newest;
            // an older one that already holds this inode's request is still
            // going to answer it — with `HydrateDone`, or, if it is on its way
            // out, through its disconnect guard, which takes every waiter on
            // its jobs. Either way this opener is answered, and nobody else
            // is asked for a file already being filled.
            let same_uid = self.jobs.get(&req_id).is_some_and(|job| job.owner.uid == owner.uid);
            if same_uid {
                if let Some(job) = self.jobs.get_mut(&req_id) {
                    job.waiters.push(fd);
                    return Enrollment {
                        outcome: Enrolled::Existing { req_id },
                        evicted,
                        dispatch: None,
                    };
                }
            }
            // Another uid's hydration of this inode: the file changed owner
            // while it was being filled, and the daemon in hand was asked on
            // behalf of somebody who no longer owns it. Its waiters are
            // handed back for denial rather than left to that daemon, and
            // this open starts a job of its own with its owner's daemon.
            evicted = self.evict(req_id);
        }
        self.next_id += 1;
        let req_id = self.next_id;
        if self.outstanding_for(owner.conn) < MAX_OUTSTANDING_HYDRATIONS {
            let dispatch = Dispatch { req_id, fd: fd.try_clone() };
            self.insert_job(req_id, Job { inode, owner, waiters: vec![fd], sent: true, since });
            return Enrollment {
                outcome: Enrolled::New { req_id },
                evicted,
                dispatch: Some(dispatch),
            };
        }
        self.insert_job(req_id, Job { inode, owner, waiters: vec![fd], sent: false, since });
        Enrollment { outcome: Enrolled::Queued { req_id }, evicted, dispatch: None }
    }

    /// How many of `conn`'s credits are taken: requests handed to its daemon
    /// and not yet answered.
    pub fn outstanding_for(&self, conn: u64) -> usize {
        self.outstanding.get(&conn).copied().unwrap_or(0)
    }

    /// How many of `conn`'s hydrations are waiting for credit.
    pub fn queued_for(&self, conn: u64) -> usize {
        self.jobs.values().filter(|job| job.owner.conn == conn && !job.sent).count()
    }

    fn insert_job(&mut self, req_id: u64, job: Job) {
        if job.sent {
            *self.outstanding.entry(job.owner.conn).or_insert(0) += 1;
        } else {
            self.queued.entry(job.owner.conn).or_default().push_back(req_id);
        }
        self.by_inode.insert(job.inode, req_id);
        self.jobs.insert(req_id, job);
    }

    /// Removes a job and everything that indexes it, returning its credit if
    /// it held one. The `by_inode` entry goes only if it still points at this
    /// job; a queued job's place in `queued` is skipped when its turn comes.
    fn remove_job(&mut self, req_id: u64) -> Option<Job> {
        let job = self.jobs.remove(&req_id)?;
        if self.by_inode.get(&job.inode) == Some(&req_id) {
            self.by_inode.remove(&job.inode);
        }
        if job.sent {
            if let Some(count) = self.outstanding.get_mut(&job.owner.conn) {
                *count -= 1;
                if *count == 0 {
                    self.outstanding.remove(&job.owner.conn);
                }
            }
        }
        Some(job)
    }

    /// Takes the waiters off another uid's hydration of the same inode.
    ///
    /// A queued job goes entirely: nobody was asked for it. A sent one stays
    /// behind with no waiters and no inode — it still holds its credit,
    /// because its daemon still holds its request and will answer it; handing
    /// the credit back now would let one more request into a daemon whose
    /// queue has room for exactly the credit, which is the circular wait
    /// `MAX_OUTSTANDING_HYDRATIONS` exists to prevent. Its `HydrateDone`, or
    /// its connection ending, returns it.
    fn evict(&mut self, req_id: u64) -> Vec<OwnedFd> {
        let sent = self.jobs.get(&req_id).is_some_and(|job| job.sent);
        if !sent {
            return self.remove_job(req_id).map(|job| job.waiters).unwrap_or_default();
        }
        let Some(job) = self.jobs.get_mut(&req_id) else { return Vec::new() };
        let inode = job.inode;
        let waiters = std::mem::take(&mut job.waiters);
        if self.by_inode.get(&inode) == Some(&req_id) {
            self.by_inode.remove(&inode);
        }
        waiters
    }

    /// Gives one of `conn`'s free credits to its oldest queued hydration, if
    /// it has a free credit and a queued hydration.
    fn promote(&mut self, conn: u64) -> Option<Dispatch> {
        if self.outstanding_for(conn) >= MAX_OUTSTANDING_HYDRATIONS {
            return None;
        }
        let queue = self.queued.get_mut(&conn)?;
        let mut next = None;
        while let Some(req_id) = queue.pop_front() {
            if let Some(job) = self.jobs.get_mut(&req_id).filter(|job| !job.sent) {
                job.sent = true;
                let fd = match job.waiters.first() {
                    Some(fd) => fd.try_clone(),
                    None => Err(io::Error::other("a queued hydration with no waiter")),
                };
                next = Some(Dispatch { req_id, fd });
                break;
            }
        }
        if queue.is_empty() {
            self.queued.remove(&conn);
        }
        if next.is_some() {
            *self.outstanding.entry(conn).or_insert(0) += 1;
        }
        next
    }

    /// Takes the openers waiting on a hydration its daemon has answered, but
    /// only for the connection it was sent to, and only once it was sent: a
    /// queued hydration's request id has not been handed to anyone, so an
    /// answer to it is a guess. `None` means the request id is unknown, not
    /// sent, or somebody else's, and the caller must not act on it.
    ///
    /// The credit it held goes, in the same step, to the connection's oldest
    /// queued hydration, which comes back as [`Finished::next`].
    pub fn finish(&mut self, req_id: u64, owner: Owner) -> Option<Finished> {
        let job = self.jobs.get(&req_id)?;
        if job.owner != owner || !job.sent {
            return None;
        }
        let job = self.remove_job(req_id)?;
        let next = self.promote(owner.conn);
        Some(Finished { waiters: job.waiters, since: job.since, next })
    }

    /// Retires a connection and takes everything it was going to hydrate —
    /// sent and queued alike.
    ///
    /// The retirement happens **before** the drain and under the same lock
    ///, so there is no instant at which a worker can add a job
    /// to a connection whose jobs have already been collected. Only that
    /// connection's jobs are taken: another user's hydrations are none of its
    /// business, which is what stopped any local user failing every hydration
    /// on the machine with a connect-and-close loop.
    pub fn retire(&mut self, conn: u64) -> Vec<Vec<OwnedFd>> {
        if self.retired.insert(conn) {
            self.retired_order.push_back(conn);
            while self.retired_order.len() > RETIRED_REMEMBERED {
                if let Some(old) = self.retired_order.pop_front() {
                    self.retired.remove(&old);
                }
            }
        }
        self.queued.remove(&conn);
        self.take_all_of(conn)
    }

    /// Everything one connection was going to hydrate. Callers outside this
    /// module want [`retire`](Self::retire), which also closes the window.
    /// A job whose waiters were evicted has nobody left to answer and is not
    /// counted.
    fn take_all_of(&mut self, conn: u64) -> Vec<Vec<OwnedFd>> {
        let doomed: Vec<u64> = self
            .jobs
            .iter()
            .filter(|(_, job)| job.owner.conn == conn)
            .map(|(&req_id, _)| req_id)
            .collect();
        doomed
            .into_iter()
            .filter_map(|req_id| Some(self.remove_job(req_id)?.waiters))
            .filter(|waiters| !waiters.is_empty())
            .collect()
    }

    /// How many hydrations are in hand, sent or queued. Used only for
    /// logging.
    pub fn in_flight(&self) -> usize {
        self.jobs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fd() -> OwnedFd {
        tempfile::tempfile().unwrap().into()
    }

    fn owner(uid: u32, conn: u64) -> Owner {
        Owner { uid, conn }
    }

    #[test]
    fn the_first_opener_claims_the_job_and_the_rest_wait() {
        let mut jobs = Jobs::default();
        let a = owner(1000, 1);
        assert_eq!(jobs.enroll((42, 7), a, fd(), 0).outcome, Enrolled::New { req_id: 1 });
        assert_eq!(jobs.enroll((42, 7), a, fd(), 0).outcome, Enrolled::Existing { req_id: 1 });
        assert_eq!(jobs.enroll((42, 8), a, fd(), 0).outcome, Enrolled::New { req_id: 2 });
    }

    /// The race this ordering exists to close: the descriptor is in the job
    /// the instant the request id exists, so there is no window in which a
    /// `finish` can drain a job whose waiter has not been recorded.
    #[test]
    fn enrolling_registers_the_waiter_in_the_same_step_as_the_claim() {
        let mut jobs = Jobs::default();
        let a = owner(1000, 1);
        let Enrolled::New { req_id } = jobs.enroll((42, 7), a, fd(), 0).outcome else {
            panic!("expected a new job")
        };
        let _ = jobs.enroll((42, 7), a, fd(), 0);
        let _ = jobs.enroll((42, 7), a, fd(), 0);
        assert_eq!(jobs.finish(req_id, a).unwrap().waiters.len(), 3, "three openers were waiting");
        assert_eq!(
            jobs.enroll((42, 7), a, fd(), 0).outcome,
            Enrolled::New { req_id: 2 },
            "the inode is free again, and only claims consume a request id"
        );
    }

    /// second guard needs to know when a job began: the count
    /// of unregistrations when the open that *created* it was read. An opener
    /// that joins later brings a later count, which must not replace it.
    #[test]
    fn a_finished_job_carries_the_count_of_the_open_that_created_it() {
        let mut jobs = Jobs::default();
        let a = owner(1000, 1);
        let Enrolled::New { req_id } = jobs.enroll((42, 7), a, fd(), 3).outcome else {
            panic!("expected a new job")
        };
        let _ = jobs.enroll((42, 7), a, fd(), 5);
        assert_eq!(jobs.finish(req_id, a).unwrap().since, 3);
    }

    #[test]
    fn another_user_cannot_finish_someone_elses_hydration() {
        let mut jobs = Jobs::default();
        let mine = owner(1000, 1);
        let Enrolled::New { req_id } = jobs.enroll((42, 7), mine, fd(), 0).outcome else {
            panic!("expected a new job")
        };
        assert!(jobs.finish(req_id, owner(1001, 2)).is_none(), "wrong uid");
        assert!(jobs.finish(req_id, owner(1000, 2)).is_none(), "right uid, wrong connection");
        assert!(jobs.finish(999, mine).is_none(), "unknown request id");
        assert!(jobs.finish(req_id, mine).is_some(), "the owner can still finish it");
    }

    #[test]
    fn a_disconnect_only_touches_that_connections_jobs() {
        let mut jobs = Jobs::default();
        let mine = owner(1000, 1);
        let theirs = owner(1001, 2);
        let Enrolled::New { req_id } = jobs.enroll((42, 7), mine, fd(), 0).outcome else {
            panic!("expected a new job")
        };
        let _ = jobs.enroll((42, 8), theirs, fd(), 0);

        let drained = jobs.retire(2);
        assert_eq!(drained.len(), 1, "only the disconnecting connection's job");
        assert_eq!(jobs.in_flight(), 1, "the other user's hydration survives");
        assert!(jobs.finish(req_id, mine).is_some(), "and can still be finished normally");
    }

    /// The window this closes: a worker holding a `Daemon` clone
    /// from just before the cleanup ran must not be able to create a job on
    /// a connection whose jobs have already been drained — nothing would
    /// ever answer it, and there is no per-job timeout to rescue it.
    #[test]
    fn a_retired_connection_cannot_be_enrolled_against() {
        let mut jobs = Jobs::default();
        let gone = owner(1000, 1);
        let _ = jobs.enroll((42, 7), gone, fd(), 0);
        assert_eq!(jobs.retire(1).len(), 1);

        let enrollment = jobs.enroll((42, 9), gone, fd(), 0);
        assert_eq!(enrollment.outcome, Enrolled::ConnectionGone);
        assert_eq!(enrollment.evicted.len(), 1, "the descriptor comes back to be answered");
        assert_eq!(jobs.in_flight(), 0, "and no job is left behind for nobody to finish");

        // A fresh connection from the same user is unaffected.
        let fresh = owner(1000, 2);
        assert_eq!(jobs.enroll((42, 9), fresh, fd(), 0).outcome, Enrolled::New { req_id: 2 });
    }

    /// The set of retired connections is bounded, so a machine that
    /// reconnects daemons all day does not grow it without end.
    #[test]
    fn the_retired_set_is_bounded() {
        let mut jobs = Jobs::default();
        for conn in 0..(RETIRED_REMEMBERED as u64 * 2) {
            let _ = jobs.retire(conn);
        }
        assert_eq!(jobs.retired.len(), RETIRED_REMEMBERED);
        assert_eq!(jobs.retired_order.len(), RETIRED_REMEMBERED);
        assert!(jobs.retired.contains(&(RETIRED_REMEMBERED as u64 * 2 - 1)), "the newest is kept");
        assert!(!jobs.retired.contains(&0), "the oldest is forgotten");
    }

    const MAX: u64 = MAX_OUTSTANDING_HYDRATIONS as u64;

    /// Fills `conn`'s credit with hydrations of inodes `0..MAX`, then enrolls
    /// `extra` more, and returns the extra ones' request ids.
    fn beyond_the_credit(jobs: &mut Jobs, who: Owner, extra: u64) -> Vec<u64> {
        for ino in 0..MAX {
            let enrollment = jobs.enroll((42, ino), who, fd(), 0);
            assert!(matches!(enrollment.outcome, Enrolled::New { .. }));
            assert!(enrollment.dispatch.is_some_and(|d| d.fd.is_ok()), "and is sent at once");
        }
        (MAX..MAX + extra)
            .map(|ino| match jobs.enroll((42, ino), who, fd(), 0) {
                Enrollment { outcome: Enrolled::Queued { req_id }, evicted, dispatch } => {
                    assert!(evicted.is_empty() && dispatch.is_none());
                    req_id
                }
                other => panic!("beyond the credit a new hydration must queue: {:?}", other.outcome),
            })
            .collect()
    }

    /// Beyond its connection's credit a new hydration is
    /// enrolled and held back — its opener suspended like any other — not
    /// refused, and nothing more is handed to the daemon than its request
    /// queue has room for (the circular wait `MAX_OUTSTANDING_HYDRATIONS`
    /// exists to prevent).
    #[test]
    fn beyond_the_credit_a_new_hydration_waits_instead_of_being_refused() {
        let mut jobs = Jobs::default();
        let a = owner(1000, 1);
        let queued = beyond_the_credit(&mut jobs, a, 100);
        assert_eq!(queued.len(), 100);
        assert_eq!(jobs.outstanding_for(1), MAX_OUTSTANDING_HYDRATIONS, "no more than the credit");
        assert_eq!(jobs.queued_for(1), 100, "and every one beyond it waiting");
        assert_eq!(jobs.in_flight(), MAX_OUTSTANDING_HYDRATIONS + 100, "nobody refused");

        assert_eq!(
            jobs.enroll((42, MAX + 3), a, fd(), 0).outcome,
            Enrolled::Existing { req_id: queued[3] },
            "joining a queued hydration is joining it"
        );
        assert!(
            matches!(jobs.enroll((42, 9999), owner(1001, 2), fd(), 0).outcome, Enrolled::New { .. }),
            "another connection has a credit of its own"
        );
    }

    /// Every credit that comes back goes to the oldest waiting
    /// hydration, in the same step, until none is left waiting — so every
    /// opener beyond the credit is eventually answered, in arrival order.
    #[test]
    fn each_returned_credit_sends_the_oldest_waiting_hydration() {
        let mut jobs = Jobs::default();
        let a = owner(1000, 1);
        let queued = beyond_the_credit(&mut jobs, a, 100);

        let mut answered = 0;
        let mut sent_later = Vec::new();
        let mut in_hand: VecDeque<u64> = (1..=MAX).collect();
        while let Some(req_id) = in_hand.pop_front() {
            let finished = jobs.finish(req_id, a).expect("a sent hydration can be finished");
            answered += finished.waiters.len();
            if let Some(next) = finished.next {
                assert!(next.fd.is_ok(), "the promoted request carries a descriptor to fill");
                sent_later.push(next.req_id);
                in_hand.push_back(next.req_id);
            }
            assert!(jobs.outstanding_for(1) <= MAX_OUTSTANDING_HYDRATIONS);
        }
        assert_eq!(sent_later, queued, "every waiting hydration is sent, oldest first");
        assert_eq!(answered, MAX_OUTSTANDING_HYDRATIONS + 100, "and every opener answered");
        assert_eq!(jobs.in_flight(), 0);
        assert!(jobs.queued.is_empty() && jobs.outstanding.is_empty(), "nothing is left behind");
    }

    /// A queued hydration's request id has not been handed to anybody, so an
    /// answer to it is a guess, and a guess must not finish it — nor take a
    /// credit it does not hold.
    #[test]
    fn a_hydration_never_sent_cannot_be_finished() {
        let mut jobs = Jobs::default();
        let a = owner(1000, 1);
        let queued = beyond_the_credit(&mut jobs, a, 1);
        assert!(jobs.finish(queued[0], a).is_none(), "its daemon was never asked for it");
        assert_eq!(jobs.queued_for(1), 1, "and it still waits for its turn");
        assert_eq!(jobs.outstanding_for(1), MAX_OUTSTANDING_HYDRATIONS);
    }

    /// The disconnect half: a hydration waiting for credit is
    /// its connection's as much as a sent one, and goes with it — its
    /// openers come back to be denied, and no credit is left to send it with.
    #[test]
    fn queued_hydrations_are_taken_with_their_connection() {
        let mut jobs = Jobs::default();
        let a = owner(1000, 1);
        let queued = beyond_the_credit(&mut jobs, a, 10);
        let _ = jobs.enroll((42, MAX + 1), a, fd(), 0);

        let drained = jobs.retire(1);
        assert_eq!(drained.len(), MAX_OUTSTANDING_HYDRATIONS + 10, "sent and queued alike");
        assert_eq!(
            drained.iter().map(Vec::len).sum::<usize>(),
            MAX_OUTSTANDING_HYDRATIONS + 11,
            "with every opener joined to them"
        );
        assert_eq!(jobs.in_flight(), 0);
        assert!(jobs.queued.is_empty() && jobs.outstanding.is_empty());
        assert!(jobs.finish(queued[0], a).is_none());
    }

    /// The credit must follow a hydration through every way it can end, or
    /// a connection would drift towards "no credit" for good and every open
    /// for its user would wait forever. Finished and retired return it. An
    /// eviction — another uid's hydration of the same inode — does **not**:
    /// the daemon still holds that request and will answer it, and handing
    /// its credit on early would let one more request in than the daemon's
    /// queue has room for. Its answer returns it.
    #[test]
    fn the_credit_follows_every_way_a_hydration_ends() {
        let mut jobs = Jobs::default();
        let first = owner(1000, 1);
        let other_uid = owner(1001, 2);
        let queued = beyond_the_credit(&mut jobs, first, 1);

        let finished = jobs.finish(1, first).unwrap();
        assert_eq!(
            finished.next.map(|next| next.req_id),
            Some(queued[0]),
            "a finished hydration hands its credit to the next"
        );
        assert_eq!(jobs.outstanding_for(1), MAX_OUTSTANDING_HYDRATIONS);

        // Request 2 is inode (42, 1), sent. The file changes owner.
        let rechowned = jobs.enroll((42, 1), other_uid, fd(), 0);
        assert_eq!(rechowned.evicted.len(), 1, "its opener comes back to be answered");
        assert_eq!(
            jobs.outstanding_for(1),
            MAX_OUTSTANDING_HYDRATIONS,
            "but the daemon still holds that request, so the credit stays taken"
        );
        let answered = jobs.finish(2, first).expect("the daemon's answer is still accepted");
        assert!(answered.waiters.is_empty(), "with nobody left to answer");
        assert_eq!(jobs.outstanding_for(1), MAX_OUTSTANDING_HYDRATIONS - 1, "and it returns it");

        assert_eq!(jobs.retire(1).len(), MAX_OUTSTANDING_HYDRATIONS - 1);
        assert_eq!(jobs.outstanding_for(1), 0, "a retired connection's hydrations no longer count");
        assert!(!jobs.outstanding.contains_key(&1), "and a connection with none holds no entry");
        assert_eq!(jobs.outstanding_for(2), 1, "nor does retiring one touch another");
    }

    /// A hydration evicted while it was still waiting for credit is gone for
    /// good: nobody was asked for it, and its turn is skipped rather than
    /// sending a request for a job with no openers.
    #[test]
    fn an_evicted_hydration_that_never_went_out_loses_its_turn() {
        let mut jobs = Jobs::default();
        let first = owner(1000, 1);
        let queued = beyond_the_credit(&mut jobs, first, 2);

        let rechowned = jobs.enroll((42, MAX), owner(1001, 2), fd(), 0);
        assert_eq!(rechowned.evicted.len(), 1);
        assert_eq!(jobs.queued_for(1), 1);

        let next = jobs.finish(1, first).unwrap().next.map(|next| next.req_id);
        assert_eq!(next, Some(queued[1]), "the evicted one's turn is skipped");
        assert_eq!(jobs.outstanding_for(1), MAX_OUTSTANDING_HYDRATIONS);
    }

    /// A uid's hydrations go to its newest connection, and an
    /// older one stays live underneath it. An opener routed to the newer
    /// connection, for a file the older one is already hydrating, joins that
    /// hydration: the older connection will answer it with `HydrateDone`, or,
    /// if it is in fact on its way out, its disconnect guard denies every
    /// waiter on it. Starting over instead — as this did while one connection
    /// per uid meant "the other one is dead" — denied the live daemon's
    /// waiters `EIO` for nothing but a second connection appearing, and asked
    /// the newcomer for a file already being filled.
    #[test]
    fn an_opener_joins_a_hydration_an_older_live_connection_has_in_hand() {
        let mut jobs = Jobs::default();
        let live = owner(1000, 1);
        let newer = owner(1000, 2);
        let _ = jobs.enroll((42, 7), live, fd(), 0);
        let _ = jobs.enroll((42, 7), live, fd(), 0);

        let joined = jobs.enroll((42, 7), newer, fd(), 0);
        assert!(joined.evicted.is_empty(), "the live daemon's waiters must not be denied");
        assert_eq!(joined.outcome, Enrolled::Existing { req_id: 1 }, "it joins the job in hand");
        assert_eq!(jobs.in_flight(), 1, "and asks nobody for the file again");
        assert!(jobs.finish(1, newer).is_none(), "only the connection that was asked answers it");
        assert_eq!(jobs.finish(1, live).unwrap().waiters.len(), 3, "and it answers all three");
    }

    /// The other half: if the older connection was on its way out after all,
    /// its disconnect guard takes the joined opener along with its own, so
    /// nobody is stranded.
    #[test]
    fn a_joined_opener_is_answered_when_the_older_connection_goes() {
        let mut jobs = Jobs::default();
        let old = owner(1000, 1);
        let newer = owner(1000, 2);
        let _ = jobs.enroll((42, 7), old, fd(), 0);
        let _ = jobs.enroll((42, 7), newer, fd(), 0);
        let drained = jobs.retire(1);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].len(), 2, "both openers come back to be denied");
        assert_eq!(jobs.in_flight(), 0);
        assert!(
            matches!(jobs.enroll((42, 7), newer, fd(), 0).outcome, Enrolled::New { .. }),
            "and the next opener starts afresh on the connection that is left"
        );
    }

    /// Another uid's hydration of the same inode — the file changed owner
    /// while it was being filled — is not joined: that daemon was asked on
    /// behalf of somebody else. Its waiters are handed back to be answered,
    /// rather than left to a daemon that is no longer the file's owner's,
    /// and this open starts a hydration of its own.
    #[test]
    fn another_uids_hydration_of_the_same_inode_is_not_joined() {
        let mut jobs = Jobs::default();
        let before = owner(1000, 1);
        let after = owner(1001, 2);
        let _ = jobs.enroll((42, 7), before, fd(), 0);
        let _ = jobs.enroll((42, 7), before, fd(), 0);

        let enrollment = jobs.enroll((42, 7), after, fd(), 0);
        assert_eq!(enrollment.outcome, Enrolled::New { req_id: 2 });
        assert_eq!(enrollment.evicted.len(), 2, "both stranded openers come back to be answered");
        assert_eq!(jobs.finish(2, after).unwrap().waiters.len(), 1);
    }
}
