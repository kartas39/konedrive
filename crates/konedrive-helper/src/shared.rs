use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use konedrive_helper::{jobs, marks, roots};

use konedrive_helper::outbox::Outbox;

/// Takes a shared lock, and keeps going when a previous holder panicked.
///
/// The helper now survives a panic in a worker: the worker is
/// caught, its opener is answered `EIO`, and the pool stays at strength. That
/// is only true if the *next* thread to want a lock that the panicking one
/// held can still have it. `Mutex::lock().unwrap()` would panic instead, and
/// a single poisoned `jobs` mutex would then take down every worker in turn,
/// turning one recoverable bug into "deny every open on the machine".
///
/// The data behind these locks tolerates it: each is a map the helper reads
/// and writes whole, with no multi-step invariant that a panic could leave
/// half-applied. The one thing that must never be lost — an event fd owing a
/// response — is owned by a `Job`, not by a lock.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) const ROOTS_FILE: &str = "/var/lib/konedrive/roots.json";
/// How long an open waits for a daemon that is not connected yet.
pub(crate) const DAEMON_WAIT: Duration = Duration::from_secs(30);

/// The bound on threads answering permission events, and on how many opens may
/// be waiting for one.
///
/// Both numbers are **provisional**: they were chosen to be obviously enough
/// for interactive use and obviously bounded, not measured. burst
/// scenario (several thousand concurrent opens) is what should settle them —
/// it measures thread count, memory and whether any opener is lost, which is
/// exactly the evidence these two constants need and which no unit test on the
/// host can produce.
pub(crate) const EVENT_WORKERS: usize = 64;
pub(crate) const EVENT_QUEUE_DEPTH: usize = 1024;

/// How many workers may be parked waiting for **one uid's** daemon that has
/// not connected yet.
///
/// `wait_for_daemon` is the one place a worker sleeps for a long time, so it
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
/// uid that has a registered root, which is what `wait_for_daemon` checks
/// first.
///
/// Per-uid caps alone would let the pool's worst case grow to
/// `MAX_DAEMON_WAITERS` times the number of uids with a registered root
/// whose daemon is down, rather than a flat eight. That number is the
/// machine's real konedrive users, not anything a caller can inflate
/// (registering a root requires owning the directory) — but on a machine
/// with many such users it is no longer a fixed bound on the pool at all.
/// [`GLOBAL_MAX_DAEMON_WAITERS`], checked first by [`WaiterSlot::take`],
/// puts a flat ceiling back under that, so both properties hold at once:
/// one uid cannot starve another (this cap), and waiting still cannot
/// consume the pool (the global one).
pub(crate) const MAX_DAEMON_WAITERS: usize = 8;

/// The bound across **all** uids waiting at once, however it is spread
/// across them (the follow-up to). Checked before the per-uid
/// cap in [`WaiterSlot::take`], so a caller cannot get around it by
/// spreading the same attack across several uids it happens to control,
/// and a machine with many legitimate uids whose daemons are briefly down
/// still cannot have more than this many workers parked waiting at once.
///
/// Provisional, like [`MAX_DAEMON_WAITERS`] and [`EVENT_WORKERS`]: chosen
/// to be obviously bounded relative to the 64-worker pool, not measured.
/// burst scenario is what should settle it.
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

/// How long to wait before reading the fanotify group again after the process
/// ran out of file descriptors. Long enough not to spin a core, short enough
/// that interception resumes the moment descriptors come back.
pub(crate) const EXHAUSTION_BACKOFF: Duration = Duration::from_millis(50);

/// The same, for a failing `accept`. `EMFILE` there used to be a 100% CPU
/// spin, because the loop logged and retried immediately.
pub(crate) const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// Deliberate panics, for the two unwind paths nothing input-reachable can
/// exercise any more, and one deliberate stall, for a race window too narrow
/// to hit by chance — compiled in **only** with the `fault-injection` cargo
/// feature.
///
/// is about what happens *after* a panic: a worker that panicked
/// answers its opener `EIO` and the pool stays at strength, and a connection
/// whose request loop panicked still runs its `Disconnect` guard, so its
/// suspended openers are denied instead of being left in the kernel forever.
/// Both are only true if something unwinds, and — by design — nothing in the
/// helper panics on any input any more. The VM scenario suite
/// (`tests/vm/scenarios/faults.rs`) therefore restarts the helper with one of these
/// armed and then asserts on what the *opener* got, which is the only
/// evidence either ruling can have.
///
/// Armed from the environment, which no peer can set, and read at the point
/// of use. They used to be in every build on that argument alone; they are
/// not any more, because a panic trigger in the release binary of a root
/// process is test code in the one place test code should never be, however
/// unreachable. `tests/vm/run.sh` builds the helper with the feature, into
/// the VM suite's own target directory, so the binary it produces is never
/// the one `target/release` holds. Without the feature every function here is
/// empty and the variable names do not appear in the binary at all.
#[cfg(feature = "fault-injection")]
pub(crate) mod fault {
    /// Panic while deciding an intercepted open of a file of exactly this
    /// size. A size rather than a name because `handle_open` never sees a
    /// name — it is handed a descriptor for an inode.
    pub fn panic_on_size(size: u64) {
        let Some(armed) = std::env::var_os("KONEDRIVE_FAULT_PANIC_ON_SIZE") else { return };
        if armed.to_str().and_then(|s| s.parse::<u64>().ok()) == Some(size) {
            panic!("KONEDRIVE_FAULT_PANIC_ON_SIZE: deliberate panic while deciding an open");
        }
    }

    /// Panic while applying a `MarkFile` request, i.e. on the connection's
    /// own thread, inside the region the `Disconnect` guard covers.
    pub fn panic_on_mark_file() {
        if std::env::var_os("KONEDRIVE_FAULT_PANIC_ON_MARKFILE").is_some() {
            panic!("KONEDRIVE_FAULT_PANIC_ON_MARKFILE: deliberate panic on a connection thread");
        }
    }

    /// Stall a worker between reading `hydrated` and placing the ignore
    /// mark, as a preempted thread would. The natural window is well under
    /// 100 µs; this makes it as wide as the VM suite needs to put a
    /// dehydration's `dehydrating` and `ClearIgnore` inside it.
    pub fn delay_before_ignore_mark() {
        if let Some(ms) = std::env::var_os("KONEDRIVE_FAULT_DELAY_IGNORE_MS")
            .and_then(|v| v.to_str().and_then(|s| s.parse::<u64>().ok()))
        {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
}

/// The shipped build: nothing to arm, nothing compiled in.
#[cfg(not(feature = "fault-injection"))]
pub(crate) mod fault {
    #[inline(always)]
    pub fn panic_on_size(_size: u64) {}

    #[inline(always)]
    pub fn panic_on_mark_file() {}

    #[inline(always)]
    pub fn delay_before_ignore_mark() {}
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
/// So every live connection is kept, in accept order ([`serve`] numbers them,
///), and the **top** — the newest — is the one that matters: a
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
pub(crate) struct Registry {
    /// Oldest first, so the top is the last. No uid holds an empty stack.
    by_uid: HashMap<u32, Vec<Daemon>>,
}

impl Registry {
    /// The connection `uid`'s hydrations go to: its newest live one.
    pub(crate) fn top(&self, uid: u32) -> Option<&Daemon> {
        self.by_uid.get(&uid).and_then(|stack| stack.last())
    }

    /// Adds a connection in accept order, whatever order the connections'
    /// threads get here in. Returns whether it is now the top:
    /// an older connection that registers late goes underneath the newer one
    /// instead of taking over from it.
    pub(crate) fn register(&mut self, daemon: Daemon) -> bool {
        let stack = self.by_uid.entry(daemon.uid).or_default();
        let at = stack.partition_point(|live| live.conn < daemon.conn);
        stack.insert(at, daemon);
        at + 1 == stack.len()
    }

    /// Whether `pid` is the process behind `uid`'s top connection — the one
    /// pid exempts for that uid's files.
    pub(crate) fn is_top_pid(&self, uid: u32, pid: i32) -> bool {
        self.top(uid).is_some_and(|daemon| daemon.pid == pid)
    }

    /// Removes connection `conn` of `uid` wherever it sits in the stack. The
    /// newest connection left, if any, is the top from now on.
    pub(crate) fn deregister(&mut self, uid: u32, conn: u64) {
        let Some(stack) = self.by_uid.get_mut(&uid) else { return };
        stack.retain(|live| live.conn != conn);
        if stack.is_empty() {
            self.by_uid.remove(&uid);
        }
    }
}

pub(crate) struct Shared {
    pub(crate) marks: marks::Marks,
    pub(crate) roots: Mutex<roots::Roots>,
    pub(crate) jobs: Mutex<jobs::Jobs>,
    /// Every live connection, by uid.
    pub(crate) daemons: Mutex<Registry>,
    /// Signalled whenever a daemon registers, so an intercepted open that
    /// arrives before the daemon does wakes the moment it connects instead of
    /// polling for it.
    pub(crate) daemon_arrived: Condvar,
    /// Roots whose startup walk could not cover everything. Kept
    /// so the condition is visible rather than only logged; nothing consumes
    /// it yet.
    pub(crate) degraded_roots: Mutex<HashSet<String>>,
    /// The throttled log of refused opens.
    pub(crate) refusals: Refusals,
    /// How many workers are currently parked in `wait_for_daemon`, per uid.
    /// A uid with nobody waiting holds no entry, so
    /// the map is the size of the set of uids currently waiting and no
    /// larger.
    pub(crate) daemon_waiters: Mutex<HashMap<u32, usize>>,
    /// Live connections per uid; see
    /// [`MAX_CONNECTIONS_PER_UID`].
    pub(crate) connections: Arc<Mutex<HashMap<u32, usize>>>,
    /// Every root unregistration's walk, as it begins and as it ends: what
    /// `mark_while_hydrated` looks at to leave no ignore mark behind a walk
    /// that has already passed (second guard).
    pub(crate) unregistrations: Unregistrations,
}

/// How many walk boundaries [`Unregistrations`] remembers, two per
/// unregistration. Anything older than that is assumed to concern everyone.
const UNREGISTRATIONS_REMEMBERED: usize = 4096;

/// A sequence number bumped as each root's unregistration walk begins and as
/// it ends, and which uid's root each bump was for.
///
/// Per uid, and not one count for the machine, because the guard withholds an
/// ignore mark, and a withheld mark costs a permission event on every later
/// open of that file until one is placed. With one count, any local user
/// could register and unregister a root of their own in a loop — with a tree
/// as large as they like, which is as long as each walk takes — and keep
/// every other user's hydrated files from ever being marked. A file is
/// matched to an unregistration by its owner's uid, which is the uid of the
/// root it is in whenever the daemon can act on it at all (§6.2); a file in
/// someone else's root that this misses is still cleared by the registration
/// walk before that tree is intercepted again.
pub(crate) struct Unregistrations {
    seq: AtomicU64,
    /// `(the sequence number the bump produced, the root's uid)`, oldest
    /// first. Written and read under the lock; `seq` is only ever advanced
    /// under it too, so an entry is there by the time its number is seen.
    recent: Mutex<std::collections::VecDeque<(u64, u32)>>,
}

impl Unregistrations {
    pub(crate) fn new() -> Self {
        Self { seq: AtomicU64::new(0), recent: Mutex::new(std::collections::VecDeque::new()) }
    }

    /// The sequence number now: what an event read now is stamped with.
    pub(crate) fn now(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    /// One boundary of `uid`'s unregistration walk.
    pub(crate) fn bump(&self, uid: u32) {
        let mut recent = lock(&self.recent);
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        recent.push_back((seq, uid));
        while recent.len() > UNREGISTRATIONS_REMEMBERED {
            recent.pop_front();
        }
    }

    /// Whether one of `uid`'s roots has been unregistered — a walk begun or
    /// ended — since `since`; with no uid, whether anybody's has. `true`
    /// also when bumps after `since` have been forgotten, since then nobody
    /// can say whose they were.
    pub(crate) fn since(&self, since: u64, uid: Option<u32>) -> bool {
        let recent = lock(&self.recent);
        if self.seq.load(Ordering::SeqCst) == since {
            return false;
        }
        match recent.front() {
            Some(&(oldest, _)) if oldest > since + 1 => true,
            None => true,
            Some(_) => recent.iter().any(|&(seq, who)| seq > since && uid.is_none_or(|u| u == who)),
        }
    }
}

/// Holds one of `uid`'s [`MAX_CONNECTIONS_PER_UID`] places for as long as it
/// lives — the life of the connection's thread.
pub(crate) struct ConnectionSlot {
    counters: Arc<Mutex<HashMap<u32, usize>>>,
    uid: u32,
}

impl ConnectionSlot {
    pub(crate) fn take(counters: &Arc<Mutex<HashMap<u32, usize>>>, uid: u32) -> Option<ConnectionSlot> {
        let mut held = lock(counters);
        let live = held.entry(uid).or_insert(0);
        if *live >= MAX_CONNECTIONS_PER_UID {
            return None;
        }
        *live += 1;
        Some(ConnectionSlot { counters: Arc::clone(counters), uid })
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        let mut held = lock(&self.counters);
        if let Some(live) = held.get_mut(&self.uid) {
            *live -= 1;
            if *live == 0 {
                held.remove(&self.uid);
            }
        }
    }
}

/// Holds one of `uid`'s [`MAX_DAEMON_WAITERS`] slots for as long as it lives.
pub(crate) struct WaiterSlot<'a> {
    counters: &'a Mutex<HashMap<u32, usize>>,
    uid: u32,
}

impl<'a> WaiterSlot<'a> {
    pub(crate) fn take(counters: &'a Mutex<HashMap<u32, usize>>, uid: u32) -> Option<WaiterSlot<'a>> {
        let mut held = lock(counters);
        // The global backstop is checked first, and before any per-uid entry
        // is touched: the map is the size of the set of uids currently
        // waiting (see `Shared::daemon_waiters`), so this sum is bounded by
        // GLOBAL_MAX_DAEMON_WAITERS itself and never a hot loop over
        // unrelated state.
        let total: usize = held.values().sum();
        if total >= GLOBAL_MAX_DAEMON_WAITERS {
            return None;
        }
        let waiting = held.entry(uid).or_insert(0);
        if *waiting >= MAX_DAEMON_WAITERS {
            // No entry is created by this arm: `or_insert(0)` only inserted a
            // zero if there was nothing there, and a zero cannot reach the cap.
            return None;
        }
        *waiting += 1;
        drop(held);
        Some(WaiterSlot { counters, uid })
    }
}

impl Drop for WaiterSlot<'_> {
    fn drop(&mut self) {
        let mut held = lock(self.counters);
        if let Some(waiting) = held.get_mut(&self.uid) {
            *waiting -= 1;
            if *waiting == 0 {
                held.remove(&self.uid);
            }
        }
    }
}

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
}

impl Refusal {
    const ALL: [Refusal; 9] = [
        Refusal::PoolFull,
        Refusal::NoRoot,
        Refusal::TooManyWaiters,
        Refusal::TimedOut,
        Refusal::Undeliverable,
        Refusal::StrayDone,
        Refusal::TooManyConnections,
        Refusal::Unopenable,
        Refusal::EventFdFailed,
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
            tracing::warn!("{} ({occurrences}{THROTTLE_MARK})", describe());
        }
    }

    /// Writes the count of every kind whose interval is over with
    /// occurrences not yet written.
    pub(crate) fn flush(&self) {
        for kind in Refusal::ALL {
            let pending = lock(&self.throttles[kind as usize]).flush();
            if let Some(occurrences) = pending {
                tracing::warn!("{} ({occurrences}{THROTTLE_MARK})", kind.summary());
            }
        }
    }
}

#[cfg(test)]
mod tests;
