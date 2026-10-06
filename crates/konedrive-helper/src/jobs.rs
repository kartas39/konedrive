//! Coalescing: many processes opening the same file wait on one hydration.
//!
//! This module also owns the suspended openers themselves. That is
//! deliberate: while the job table and the list of descriptors to answer
//! lived in two separate maps behind two separate locks, "claim the job" and
//! "register as a waiter" could not be made one atomic step, and a daemon
//! that answered quickly could run `finish` in the gap — dropping a job whose
//! only waiter had not been recorded yet, and leaving that opener suspended
//! for good. With the descriptors inside the job there is one lock, and
//! `enroll` is that single step.
//!
//! What a job keeps for each opener is the type `W`: in the helper a
//! [`PendingOpen`](crate::pending::PendingOpen), which answers its open when
//! it is consumed, or with `EIO` if it is ever dropped. The table itself only
//! duplicates a waiter's descriptor for the daemon, so all it asks of `W` is
//! `AsFd`.
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
use std::os::fd::{AsFd, OwnedFd};

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

/// How many suspended opens may wait on one uid's daemons at once, over all
/// of that uid's connections and jobs.
///
/// Each suspended open is an event fd the helper keeps until the daemon
/// answers, and a daemon is free never to answer: without a bound, one uid
/// whose program takes the requests and says nothing holds descriptors up to
/// the helper's `LimitNOFILE` (65 536 in the unit), and from then on every
/// other user's intercepted open is denied and no daemon can connect. Past
/// the bound an open is refused `EAGAIN` ([`Enrolled::TooMany`]); only that
/// uid's are.
///
/// An open is charged to the uid whose daemon is asked — the file's owner —
/// not to whoever opens it: the event carries no uid for the opener, and the
/// descriptors are held for as long as that daemon stays silent, whoever
/// opened.
///
/// Chosen, not measured: well above the 3 000 concurrent opens the VM suite's
/// burst holds, and an eighth of the unit's descriptors.
pub const MAX_SUSPENDED_OPENS_PER_UID: usize = 8192;

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
    /// This caller created the hydration beyond its connection's credit.
    /// The opener is enrolled and stays suspended; the request
    /// goes to the daemon when a credit returns, handed out by
    /// [`Jobs::finish`]. Nothing to send now.
    Queued { req_id: u64 },
    /// Someone already created it, sent or queued; this opener just waits.
    Existing { req_id: u64 },
    /// The connection this job would have belonged to has already gone away.
    /// The descriptor comes back in `Enrollment::evicted`; the caller must
    /// answer it.
    ConnectionGone,
    /// The uid whose daemon would be asked already has
    /// [`MAX_SUSPENDED_OPENS_PER_UID`] opens suspended. Nothing was enrolled;
    /// the descriptor comes back in `Enrollment::evicted`, and the caller
    /// must answer it (`EAGAIN`).
    TooMany,
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
/// [`Enrolled::ConnectionGone`] and [`Enrolled::TooMany`], the opener's own
/// descriptor and nothing else. The caller must
/// answer them: they are handed back because only the caller knows which
/// errno each is owed.
#[must_use]
pub struct Enrollment<W> {
    pub outcome: Enrolled,
    pub evicted: Vec<W>,
    /// For [`Enrolled::New`] only: the request to send.
    pub dispatch: Option<Dispatch>,
}

/// A hydration its daemon has answered.
#[must_use]
pub struct Finished<W> {
    /// Every opener waiting on it, to be answered.
    pub waiters: Vec<W>,
    /// The helper's count of root unregistrations when the open that
    /// created this job was read (second guard): a job that
    /// began before an unregistration may be for a file whose tree that
    /// unregistration's walk has already passed, and gets no ignore mark.
    pub since: u64,
    /// The oldest hydration that was waiting for its connection's credit, now
    /// holding the one this returned. Its request must be sent.
    pub next: Option<Dispatch>,
}

struct Job<W> {
    inode: (u64, u64),
    owner: Owner,
    /// The exact descriptors `read_events()` handed out, never duplicates of
    /// them — the kernel matches a permission response by fd *number*
    /// (`docs/kernel-behavior-7.2/interception.md` §5.1).
    waiters: Vec<W>,
    /// Whether its request has been handed out — whether it holds one of its
    /// connection's credits. A job that is not sent is in `queued`.
    sent: bool,
    /// See [`Finished::since`]. The creating open's, which is the earliest:
    /// an opener that joins later was read later.
    since: u64,
}

pub struct Jobs<W> {
    next_id: u64,
    by_inode: HashMap<(u64, u64), u64>,
    jobs: HashMap<u64, Job<W>>,
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
    /// How many opens are suspended for each uid: the waiters of all its
    /// jobs, on whichever of its connections, the number
    /// [`MAX_SUSPENDED_OPENS_PER_UID`] bounds. A uid with none holds no
    /// entry. Kept in step by [`hold`](Self::hold) and
    /// [`release`](Self::release), wherever a waiter enters or leaves a job.
    suspended: HashMap<u32, usize>,
}

impl<W> Default for Jobs<W> {
    fn default() -> Self {
        Self {
            next_id: 0,
            by_inode: HashMap::new(),
            jobs: HashMap::new(),
            retired: HashSet::new(),
            retired_order: VecDeque::new(),
            outstanding: HashMap::new(),
            queued: HashMap::new(),
            suspended: HashMap::new(),
        }
    }
}

impl<W: AsFd> Jobs<W> {
    /// Records one suspended open against the hydration of its inode,
    /// creating the job if this is the first opener. The descriptor is stored
    /// before this returns, so a `finish` that arrives immediately afterwards
    /// always sees it.
    ///
    /// A connection that has already been retired is refused. A
    /// worker that took its `Daemon` clone out of `Daemons::wait_for` a moment
    /// before that connection's cleanup ran would otherwise create a job
    /// nobody is left to finish: usually the send that follows fails and
    /// drains it, but when the connection ended on a *deserialisation* error
    /// the peer socket is still open, the send succeeds, and — because hydration.md §5.1
    /// deliberately puts no time limit on a hydration — the waiters stay
    /// suspended until the helper exits.
    ///
    /// A new job takes one of its connection's credits and comes back with
    /// the request to send ([`Enrolled::New`]), or, when they are all taken,
    /// is queued behind the others ([`Enrolled::Queued`]).
    /// Nobody is refused for want of credit; joining a job that already
    /// exists, sent or queued, asks the daemon for nothing more.
    ///
    /// A uid that already has [`MAX_SUSPENDED_OPENS_PER_UID`] opens
    /// suspended is refused ([`Enrolled::TooMany`]), whether the open would
    /// start a job or join one: a joined waiter is a descriptor like any
    /// other. Nothing else changes then — no job is created and nobody is
    /// evicted.
    ///
    /// `since` is the helper's count of root unregistrations when this open
    /// was read (see [`Finished::since`]); it is recorded only if the open
    /// creates the job.
    pub fn enroll(&mut self, inode: (u64, u64), owner: Owner, fd: W, since: u64) -> Enrollment<W> {
        let mut evicted = Vec::new();
        if self.retired.contains(&owner.conn) {
            return Enrollment {
                outcome: Enrolled::ConnectionGone,
                evicted: vec![fd],
                dispatch: None,
            };
        }
        if self.suspended_for(owner.uid) >= MAX_SUSPENDED_OPENS_PER_UID {
            return Enrollment { outcome: Enrolled::TooMany, evicted: vec![fd], dispatch: None };
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
                    let uid = job.owner.uid;
                    self.hold(uid, 1);
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
            let dispatch = Dispatch { req_id, fd: fd.as_fd().try_clone_to_owned() };
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
    #[cfg(test)]
    pub fn queued_for(&self, conn: u64) -> usize {
        self.jobs.values().filter(|job| job.owner.conn == conn && !job.sent).count()
    }

    /// How many opens are suspended for `uid`, over all its connections.
    pub fn suspended_for(&self, uid: u32) -> usize {
        self.suspended.get(&uid).copied().unwrap_or(0)
    }

    /// Counts `waiters` more suspended opens against `uid`.
    fn hold(&mut self, uid: u32, waiters: usize) {
        if waiters > 0 {
            *self.suspended.entry(uid).or_insert(0) += waiters;
        }
    }

    /// Takes `waiters` suspended opens off `uid`'s count: they left their
    /// job, to be answered.
    fn release(&mut self, uid: u32, waiters: usize) {
        if let Some(count) = self.suspended.get_mut(&uid) {
            *count = count.saturating_sub(waiters);
            if *count == 0 {
                self.suspended.remove(&uid);
            }
        }
    }

    fn insert_job(&mut self, req_id: u64, job: Job<W>) {
        self.hold(job.owner.uid, job.waiters.len());
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
    fn remove_job(&mut self, req_id: u64) -> Option<Job<W>> {
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
        self.release(job.owner.uid, job.waiters.len());
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
    fn evict(&mut self, req_id: u64) -> Vec<W> {
        let sent = self.jobs.get(&req_id).is_some_and(|job| job.sent);
        if !sent {
            return self.remove_job(req_id).map(|job| job.waiters).unwrap_or_default();
        }
        let Some(job) = self.jobs.get_mut(&req_id) else { return Vec::new() };
        let inode = job.inode;
        let uid = job.owner.uid;
        let waiters = std::mem::take(&mut job.waiters);
        if self.by_inode.get(&inode) == Some(&req_id) {
            self.by_inode.remove(&inode);
        }
        self.release(uid, waiters.len());
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
                    Some(fd) => fd.as_fd().try_clone_to_owned(),
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
    pub fn finish(&mut self, req_id: u64, owner: Owner) -> Option<Finished<W>> {
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
    /// The retirement happens **before** the drain and under the same
    /// lock, so there is no instant at which a worker can add a job
    /// to a connection whose jobs have already been collected. Only that
    /// connection's jobs are taken: another user's hydrations are none of its
    /// business, which is what stopped any local user failing every hydration
    /// on the machine with a connect-and-close loop.
    pub fn retire(&mut self, conn: u64) -> Vec<Vec<W>> {
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
    fn take_all_of(&mut self, conn: u64) -> Vec<Vec<W>> {
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
mod tests;
