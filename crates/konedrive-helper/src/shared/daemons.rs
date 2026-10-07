use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use konedrive_helper::outbox::Outbox;

use super::lock;
use super::slots::UidSlots;

/// How long an open waits for a daemon that is not connected yet.
pub(crate) const DAEMON_WAIT: Duration = Duration::from_secs(30);

/// How many workers may be parked waiting for **one uid's** daemon that has
/// not connected yet.
///
/// [`Daemons::wait_for`] is the one place a worker sleeps for a long time, so it
/// is the one place an unprivileged caller can aim at the pool: opening
/// somebody else's placeholders while their daemon is down used to park a
/// worker for the full `DAEMON_WAIT` each time, and 64 such opens stalled
/// every intercepted open on the machine. Opens beyond the cap are denied at
/// once rather than queueing behind it.
///
/// The counter is **keyed by uid**, and that is the whole point of it. As one
/// counter for the machine it was itself a denial of service: a local user
/// holding all eight slots by opening somebody else's placeholders made a
/// third user's perfectly legitimate early-boot open fail `EIO` immediately
/// instead of waiting the few seconds its own daemon needed to come up. Per
/// uid, the attacker can only spend the victim's own budget — and only for a
/// uid that has a registered root, which is what [`Daemons::wait_for`] checks
/// first.
///
/// Per-uid caps alone would let the pool's worst case grow to
/// `MAX_DAEMON_WAITERS` times the number of uids with a registered root
/// whose daemon is down, rather than a flat eight. That number is the
/// machine's real konedrive users, not anything a caller can inflate
/// (registering a root requires owning the directory) — but on a machine
/// with many such users it is no longer a fixed bound on the pool at all.
/// [`GLOBAL_MAX_DAEMON_WAITERS`], checked first by [`UidSlots::take`],
/// puts a flat ceiling back under that, so both properties hold at once:
/// one uid cannot starve another (this cap), and waiting still cannot
/// consume the pool (the global one).
pub(crate) const MAX_DAEMON_WAITERS: usize = 8;

/// The bound across **all** uids waiting at once, however it is spread
/// across them. Checked before the per-uid
/// cap in [`UidSlots::take`], so a caller cannot get around it by
/// spreading the same attack across several uids it happens to control,
/// and a machine with many legitimate uids whose daemons are briefly down
/// still cannot have more than this many workers parked waiting at once.
///
/// Provisional, like [`MAX_DAEMON_WAITERS`] and [`EVENT_WORKERS`]: chosen
/// to be obviously bounded relative to the 64-worker pool, not measured.
/// The VM suite's burst scenario is what should settle it.
pub(crate) const GLOBAL_MAX_DAEMON_WAITERS: usize = 32;

/// How many live connections one uid may hold.
///
/// Each costs the helper two threads and about three descriptors, and the
/// socket is 0666: without a bound, any local user could open connections
/// until the helper ran out of descriptors, and every intercepted open on
/// the machine was denied from then on. A daemon needs one, plus a moment's
/// overlap when it reconnects, and a same-uid transient (`konedrivectl`
/// talks to the daemon, not here) has no reason to hold many; beyond the
/// bound a connection is closed as soon as it is accepted, and only that
/// uid's are refused. Chosen, not measured.
pub(crate) const MAX_CONNECTIONS_PER_UID: usize = 16;

/// The places workers wait in for a daemon: [`MAX_DAEMON_WAITERS`] for each
/// uid, [`GLOBAL_MAX_DAEMON_WAITERS`] in all.
pub(crate) fn waiter_slots() -> Arc<UidSlots> {
    UidSlots::new(MAX_DAEMON_WAITERS, Some(GLOBAL_MAX_DAEMON_WAITERS))
}

/// The places of live connections: [`MAX_CONNECTIONS_PER_UID`] for each uid.
pub(crate) fn connection_slots() -> Arc<UidSlots> {
    UidSlots::new(MAX_CONNECTIONS_PER_UID, None)
}

/// One connected daemon.
#[derive(Clone)]
pub(crate) struct Daemon {
    /// Distinguishes this connection from any other, including a later one
    /// from the same uid. Everything this connection is allowed to touch is
    /// keyed on it.
    pub(crate) conn: u64,
    pub(crate) uid: u32,
    /// From `SO_PEERCRED`, so kernel-supplied and unforgeable.
    pub(crate) pid: i32,
    /// The send half: a bounded queue drained by this connection's own
    /// writer thread. Nothing that holds a worker thread ever
    /// blocks on the socket — queueing is `try_send`, and a request never
    /// finds its room taken while the connection lives, because that room is
    /// its credit.
    pub(crate) outbox: Arc<Outbox>,
}

/// Every live connection of every uid, oldest first.
///
/// One entry per uid used to be enough to say where a uid's hydrations go,
/// and it was not enough to say what happens when that entry goes away. A
/// process of the daemon's own uid that connected after it — newest wins, so
/// it took the entry over — and then disconnected removed the entry with it,
/// and the live daemon underneath was left unregistered with its socket still
/// open: it had no reason to reconnect, so every open of that user's
/// placeholders waited [`DAEMON_WAIT`] and was denied `EIO` until the daemon
/// restarted. No race was needed; connect, disconnect.
///
/// So every live connection is kept, in accept order (`connection::serve`
/// numbers them), and the **top** — the newest — is the one that matters: a
/// uid's hydrations go to it and only its pid is exempt. Any
/// connection leaving is removed wherever it sits, and whatever is newest
/// among the rest is the top again. That serves both real cases: a daemon that
/// restarts connects anew and takes over at once, and a transient connection
/// that comes and goes hands the uid straight back to the daemon underneath.
///
/// While a transient connection is on top its pid is the exempt one, but only
/// for files owned by its own uid — files it can read anyway — so being on top
/// gives it nothing it did not have.
#[derive(Default)]
struct Registry {
    /// Oldest first, so the top is the last. No uid holds an empty stack.
    by_uid: HashMap<u32, Vec<Daemon>>,
    /// The helper is stopping: no open waits for a daemon any more
    /// ([`Daemons::stop`]).
    stopping: bool,
}

impl Registry {
    /// The connection `uid`'s hydrations go to: its newest live one.
    fn top(&self, uid: u32) -> Option<&Daemon> {
        self.by_uid.get(&uid).and_then(|stack| stack.last())
    }

    /// Adds a connection in accept order, whatever order the connections'
    /// threads get here in. Returns whether it is now the top:
    /// an older connection that registers late goes underneath the newer one
    /// instead of taking over from it.
    fn register(&mut self, daemon: Daemon) -> bool {
        let stack = self.by_uid.entry(daemon.uid).or_default();
        let at = stack.partition_point(|live| live.conn < daemon.conn);
        stack.insert(at, daemon);
        at + 1 == stack.len()
    }

    /// Whether `pid` is the process behind `uid`'s top connection — the one
    /// pid exempts for that uid's files.
    fn is_top_pid(&self, uid: u32, pid: i32) -> bool {
        self.top(uid).is_some_and(|daemon| daemon.pid == pid)
    }

    /// Removes connection `conn` of `uid` wherever it sits in the stack. The
    /// newest connection left, if any, is the top from now on.
    fn deregister(&mut self, uid: u32, conn: u64) {
        let Some(stack) = self.by_uid.get_mut(&uid) else { return };
        stack.retain(|live| live.conn != conn);
        if stack.is_empty() {
            self.by_uid.remove(&uid);
        }
    }
}

/// Why there is no daemon to ask. Three different facts about the system,
/// which used to be reported as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoDaemon {
    /// This uid has registered no root, so no daemon of theirs could hydrate
    /// anything. Answered immediately; nothing was waited for.
    NoRoot,
    /// Either this uid's [`MAX_DAEMON_WAITERS`] slots, or the machine-wide
    /// [`GLOBAL_MAX_DAEMON_WAITERS`] backstop, are all taken. Answered
    /// immediately; nothing was waited for.
    TooManyWaiters,
    /// Waited the full [`DAEMON_WAIT`] and no daemon connected.
    TimedOut,
    /// The helper is stopping ([`Daemons::stop`]); the wait ended there.
    Stopping,
}

/// The connected daemons, and the opens waiting for one to connect: the
/// registry, what wakes a waiter when a daemon registers, and the bound on
/// how many may wait. One lock, never held while another of the helper's is
/// taken.
pub(crate) struct Daemons {
    /// Every live connection, by uid.
    live: Mutex<Registry>,
    /// Signalled whenever a daemon registers, so an intercepted open that
    /// arrives before the daemon does wakes the moment it connects instead of
    /// polling for it.
    arrived: Condvar,
    /// How many workers are currently parked in [`wait_for`](Self::wait_for),
    /// per uid.
    waiters: Arc<UidSlots>,
}

impl Daemons {
    pub(crate) fn new() -> Self {
        Self {
            live: Mutex::new(Registry::default()),
            arrived: Condvar::new(),
            waiters: waiter_slots(),
        }
    }

    /// The connection `uid`'s hydrations go to: its newest live one.
    pub(crate) fn top(&self, uid: u32) -> Option<Daemon> {
        lock(&self.live).top(uid).cloned()
    }

    /// Whether `pid` is the process behind `uid`'s top connection — the one
    /// pid exempts for that uid's files.
    pub(crate) fn is_top_pid(&self, uid: u32, pid: i32) -> bool {
        lock(&self.live).is_top_pid(uid, pid)
    }

    /// Adds a connection (see [`Registry::register`]) and wakes every open
    /// waiting for a daemon. Returns whether the connection is now its uid's
    /// top.
    pub(crate) fn register(&self, daemon: Daemon) -> bool {
        let mut live = lock(&self.live);
        let on_top = live.register(daemon);
        self.arrived.notify_all();
        on_top
    }

    /// Removes connection `conn` of `uid`; the newest one left, if any, is
    /// the top from now on.
    pub(crate) fn deregister(&self, uid: u32, conn: u64) {
        lock(&self.live).deregister(uid, conn);
    }

    /// Ends every wait for a daemon, now and from here on
    /// ([`NoDaemon::Stopping`]): the helper is stopping, and a worker parked
    /// in [`wait_for`](Self::wait_for) holds an open that must be answered
    /// before the process ends.
    pub(crate) fn stop(&self) {
        lock(&self.live).stopping = true;
        self.arrived.notify_all();
    }

    /// Waits for `uid`'s daemon to connect, up to [`DAEMON_WAIT`]. Woken by
    /// [`register`](Self::register) the instant one registers, rather than
    /// polling. `has_root` says whether the uid has a registered root; it is
    /// asked only when no daemon is there, and with no lock held.
    ///
    /// Two limits are put on the waiting, because this is the only place
    /// a worker sleeps for tens of seconds and therefore the only lever an
    /// unprivileged caller has on the pool:
    ///
    /// - **a user with no registered root is never waited for.** A daemon that
    ///   has never registered anything cannot hydrate anything either, so the
    ///   wait could only ever end in the same `EIO` thirty seconds later. This is
    ///   what stops someone parking workers by opening files belonging to a uid
    ///   that does not run konedrive at all.
    /// - **at most [`MAX_DAEMON_WAITERS`] workers wait for any one uid at once,**
    ///   and **at most [`GLOBAL_MAX_DAEMON_WAITERS`] wait for any combination of
    ///   uids at once.** Beyond either cap the open is denied immediately rather
    ///   than queueing behind the others, so waiting can never consume the pool
    ///   and stall interception for everybody else on the machine — not for one
    ///   uid pinned against its own cap, and not for the machine as a whole no
    ///   matter how many uids are waiting at once.
    ///
    /// The refusal says which of the three happened, or that the helper is
    /// stopping. The caller logs it; the reason never changes the answer,
    /// which is always `EIO`.
    pub(crate) fn wait_for(
        &self,
        uid: u32,
        has_root: impl FnOnce() -> bool,
    ) -> Result<Daemon, NoDaemon> {
        // The overwhelmingly common case: the daemon is already there, and
        // nothing below applies.
        if let Some(daemon) = self.top(uid) {
            return Ok(daemon);
        }
        if !has_root() {
            return Err(NoDaemon::NoRoot);
        }
        let Some(_slot) = self.waiters.take(uid) else {
            return Err(NoDaemon::TooManyWaiters);
        };

        let deadline = Instant::now() + DAEMON_WAIT;
        let mut live = lock(&self.live);
        loop {
            if let Some(daemon) = live.top(uid) {
                return Ok(daemon.clone());
            }
            if live.stopping {
                return Err(NoDaemon::Stopping);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(NoDaemon::TimedOut);
            }
            let (guard, _) = self
                .arrived
                .wait_timeout(live, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            live = guard;
        }
    }
}

#[cfg(test)]
mod tests;
