//! konedrive-helper: the only privileged part. It knows nothing about OneDrive.

mod pool;

use konedrive_helper::{by_handle, jobs, marks, outbox, roots};

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::{read_item_id, read_state, State, StateError};
use konedrive_fs::probe::{probe_dir, ProbeError};
use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION, SOCKET_PATH};
use nix::errno::Errno;
use nix::fcntl::{openat2, OFlag, OpenHow, ResolveFlag};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::fanotify::MaskFlags;
use nix::sys::socket::{
    accept, bind, connect, getsockopt, listen as sock_listen, socket, sockopt::PeerCredentials,
    AddressFamily, Backlog, SockFlag, SockType, UnixAddr,
};

use jobs::{Enrolled, Owner};
use outbox::{Outbox, Outgoing};

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
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

const ROOTS_FILE: &str = "/var/lib/konedrive/roots.json";
/// How long an open waits for a daemon that is not connected yet.
const DAEMON_WAIT: Duration = Duration::from_secs(30);

/// The bound on threads answering permission events, and on how many opens may
/// be waiting for one.
///
/// Both numbers are **provisional**: they were chosen to be obviously enough
/// for interactive use and obviously bounded, not measured. burst
/// scenario (several thousand concurrent opens) is what should settle them —
/// it measures thread count, memory and whether any opener is lost, which is
/// exactly the evidence these two constants need and which no unit test on the
/// host can produce.
const EVENT_WORKERS: usize = 64;
const EVENT_QUEUE_DEPTH: usize = 1024;

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
const MAX_DAEMON_WAITERS: usize = 8;

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
const GLOBAL_MAX_DAEMON_WAITERS: usize = 32;

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
const MAX_CONNECTIONS_PER_UID: usize = 16;

/// How long a repeating condition (descriptor exhaustion, a failing `accept`,
/// a refused open) may go unlogged. All of those can repeat thousands of
/// times a second, and a log line each is a flood that hides the one line
/// anybody needed.
const REPORT_EVERY: Duration = Duration::from_secs(5);

/// How often the refusals' pending counts are looked at, so
/// that the count for the last interval of a burst is written a moment after
/// the interval ends, not whenever — if ever — the next refusal happens.
const FLUSH_EVERY: Duration = Duration::from_secs(1);

/// What the helper says about an intercepted open the kernel could not hand
/// over (see [`Refusal::Unopenable`]).
const UNOPENABLE: &str = "intercepted opens the kernel could not hand over — most likely of a \
                          file some process holds a lease on — and denied EPERM itself";

/// What the helper says about an intercepted open whose descriptor the
/// kernel could not create (see [`Refusal::EventFdFailed`]). The VM suite
/// counts it (`tests/vm/scenarios.rs`, `EVENT_FD_FAILED`); keep the two in
/// step.
const EVENT_FD_FAILED: &str = "the kernel could not open the descriptor of an intercepted open \
                               and denied it EPERM itself";

/// What every throttled line says after the number of occurrences it stands
/// for. The VM suite reads the number off in front of it
/// (`tests/vm/scenarios.rs`, `THROTTLE_MARK`); keep the two in step.
const THROTTLE_MARK: &str = " occurrence(s) since the last line like this";

/// How long to wait before reading the fanotify group again after the process
/// ran out of file descriptors. Long enough not to spin a core, short enough
/// that interception resumes the moment descriptors come back.
const EXHAUSTION_BACKOFF: Duration = Duration::from_millis(50);

/// The same, for a failing `accept`. `EMFILE` there used to be a 100% CPU
/// spin, because the loop logged and retried immediately.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

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
/// (`tests/vm/scenarios.rs`) therefore restarts the helper with one of these
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
mod fault {
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
mod fault {
    #[inline(always)]
    pub fn panic_on_size(_size: u64) {}

    #[inline(always)]
    pub fn panic_on_mark_file() {}

    #[inline(always)]
    pub fn delay_before_ignore_mark() {}
}

/// One connected daemon.
#[derive(Clone)]
struct Daemon {
    /// Distinguishes this connection from any other, including a later one
    /// from the same uid. Everything this connection is allowed to touch is
    /// keyed on it.
    conn: u64,
    uid: u32,
    /// From `SO_PEERCRED`, so kernel-supplied and unforgeable.
    pid: i32,
    /// The send half: a bounded queue drained by this connection's own
    /// writer thread. Nothing that holds a worker thread ever
    /// blocks on the socket — queueing is `try_send`, and a request never
    /// finds its room taken while the connection lives, because that room is
    /// its credit.
    outbox: Arc<Outbox>,
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
struct Registry {
    /// Oldest first, so the top is the last. No uid holds an empty stack.
    by_uid: HashMap<u32, Vec<Daemon>>,
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

struct Shared {
    marks: marks::Marks,
    roots: Mutex<roots::Roots>,
    jobs: Mutex<jobs::Jobs>,
    /// Every live connection, by uid.
    daemons: Mutex<Registry>,
    /// Signalled whenever a daemon registers, so an intercepted open that
    /// arrives before the daemon does wakes the moment it connects instead of
    /// polling for it.
    daemon_arrived: Condvar,
    /// Roots whose startup walk could not cover everything. Kept
    /// so the condition is visible rather than only logged; nothing consumes
    /// it yet.
    degraded_roots: Mutex<HashSet<String>>,
    /// The throttled log of refused opens.
    refusals: Refusals,
    /// How many workers are currently parked in `wait_for_daemon`, per uid.
    /// A uid with nobody waiting holds no entry, so
    /// the map is the size of the set of uids currently waiting and no
    /// larger.
    daemon_waiters: Mutex<HashMap<u32, usize>>,
    /// Live connections per uid; see
    /// [`MAX_CONNECTIONS_PER_UID`].
    connections: Arc<Mutex<HashMap<u32, usize>>>,
    /// Every root unregistration's walk, as it begins and as it ends: what
    /// `mark_while_hydrated` looks at to leave no ignore mark behind a walk
    /// that has already passed (second guard).
    unregistrations: Unregistrations,
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
struct Unregistrations {
    seq: AtomicU64,
    /// `(the sequence number the bump produced, the root's uid)`, oldest
    /// first. Written and read under the lock; `seq` is only ever advanced
    /// under it too, so an entry is there by the time its number is seen.
    recent: Mutex<std::collections::VecDeque<(u64, u32)>>,
}

impl Unregistrations {
    fn new() -> Self {
        Self { seq: AtomicU64::new(0), recent: Mutex::new(std::collections::VecDeque::new()) }
    }

    /// The sequence number now: what an event read now is stamped with.
    fn now(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    /// One boundary of `uid`'s unregistration walk.
    fn bump(&self, uid: u32) {
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
    fn since(&self, since: u64, uid: Option<u32>) -> bool {
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
struct ConnectionSlot {
    counters: Arc<Mutex<HashMap<u32, usize>>>,
    uid: u32,
}

impl ConnectionSlot {
    fn take(counters: &Arc<Mutex<HashMap<u32, usize>>>, uid: u32) -> Option<ConnectionSlot> {
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
struct WaiterSlot<'a> {
    counters: &'a Mutex<HashMap<u32, usize>>,
    uid: u32,
}

impl<'a> WaiterSlot<'a> {
    fn take(counters: &'a Mutex<HashMap<u32, usize>>, uid: u32) -> Option<WaiterSlot<'a>> {
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
struct Throttle {
    every: Duration,
    next: Instant,
    since_last: u64,
}

impl Throttle {
    fn new() -> Self {
        Self::every(REPORT_EVERY)
    }

    fn every(every: Duration) -> Self {
        Self { every, next: Instant::now(), since_last: 0 }
    }

    /// How many occurrences this one stands for, or `None` to stay quiet.
    fn admit(&mut self) -> Option<u64> {
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
    fn reset(&mut self) -> u64 {
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
enum Refusal {
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
struct Refusals {
    throttles: [Mutex<Throttle>; Refusal::ALL.len()],
}

impl Refusals {
    fn new() -> Self {
        Self { throttles: std::array::from_fn(|_| Mutex::new(Throttle::new())) }
    }

    /// Logs one refusal — the line `describe` builds, with the count it
    /// stands for — or only counts it, if its kind was logged less than an
    /// interval ago. `describe` runs only when a line is written.
    fn report(&self, kind: Refusal, describe: impl FnOnce() -> String) {
        let admitted = lock(&self.throttles[kind as usize]).admit();
        if let Some(occurrences) = admitted {
            tracing::warn!("{} ({occurrences}{THROTTLE_MARK})", describe());
        }
    }

    /// Writes the count of every kind whose interval is over with
    /// occurrences not yet written.
    fn flush(&self) {
        for kind in Refusal::ALL {
            let pending = lock(&self.throttles[kind as usize]).flush();
            if let Some(occurrences) = pending {
                tracing::warn!("{} ({occurrences}{THROTTLE_MARK})", kind.summary());
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();
    let shared = Arc::new(Shared {
        marks: marks::Marks::new()?,
        roots: Mutex::new(load_roots()),
        jobs: Mutex::new(jobs::Jobs::default()),
        daemons: Mutex::new(Registry::default()),
        daemon_arrived: Condvar::new(),
        degraded_roots: Mutex::new(HashSet::new()),
        daemon_waiters: Mutex::new(HashMap::new()),
        refusals: Refusals::new(),
        unregistrations: Unregistrations::new(),
        connections: Arc::new(Mutex::new(HashMap::new())),
    });
    // The last count of a burst of refusals is written by this thread, a
    // moment after its interval ends, since no further refusal may come to
    // write it.
    let flushing = Arc::clone(&shared);
    std::thread::Builder::new().name("konedrive-log".into()).spawn(move || loop {
        std::thread::sleep(FLUSH_EVERY);
        flushing.refusals.flush();
    })?;

    // Everything that can fail and end the process comes before the first
    // mark. Marking first and then failing to build
    // the pool or bind the socket exited with the group open over marked
    // trees, and every open suspended in the meantime was released by the
    // kernel as allowed — onto placeholders nobody had filled. The workers
    // exist before anything can connect, so that the
    // first daemon to arrive never finds the event loop with nowhere to hand
    // work; nothing is accepted before the walk is done.
    let pool = pool::Pool::new(Arc::clone(&shared), EVENT_WORKERS, EVENT_QUEUE_DEPTH)?;
    let socket = listen()?;
    let (walked, walk_done) = std::sync::mpsc::channel::<()>();
    let accepting = Arc::clone(&shared);
    std::thread::Builder::new().name("konedrive-accept".into()).spawn(move || {
        if walk_done.recv().is_ok() {
            serve(accepting, socket);
        }
    })?;

    // Cover every registered root before anyone can open anything in it.
    // Sorted so that which of two overlapping roots wins is the same on every
    // boot rather than whatever order the map iterated in.
    let mut registered: Vec<roots::Root> = lock(&shared.roots).iter().cloned().collect();
    registered.sort_by(|a, b| a.root_id.cmp(&b.root_id));
    let mut covered: Vec<roots::Root> = Vec::new();
    for root in &registered {
        if let Some(conflict) = overlap_with(&covered, root) {
            // The checks that ran at registration are re-run
            // here, because the stored path is only a hint and what it leads
            // to can have changed since. Two roots that now overlap cannot
            // both be marked — an event in the shared part would belong to
            // neither daemon in particular — so the later one is left alone
            // and said so, loudly.
            tracing::error!(
                "root {} ({}) now overlaps root {conflict} and will NOT be covered; opens inside \
                 it are not intercepted until it is re-registered",
                root.root_id,
                root.path
            );
            lock(&shared.degraded_roots).insert(root.root_id.clone());
            continue;
        }
        if cover_root(&shared, root) {
            covered.push(root.clone());
        }
    }

    // Nothing that can fail stands between the walk and the event loop but
    // the loop itself (§6.5).
    let _ = walked.send(());
    event_loop(&shared, &pool)
}

/// A helper that cannot read its registrations must still come up: with no
/// interception every placeholder in every sync folder reads as zeros, so
/// starting with nothing registered is strictly better than not starting.
/// `Roots::load` already moves a corrupt file aside; this covers the rest.
fn load_roots() -> roots::Roots {
    match roots::Roots::load(Path::new(ROOTS_FILE)) {
        Ok(roots) => roots,
        Err(e) => {
            tracing::error!(
                "cannot read {ROOTS_FILE}: {e}; starting with no registered roots — each daemon \
                 will re-register on its next connection"
            );
            roots::Roots::default()
        }
    }
}

/// Opens an absolute path one component at a time, from a held `/`
/// descriptor, refusing to resolve through anything that is not a real
/// directory entry.
///
/// `RESOLVE_BENEATH` cannot be handed a multi-component absolute path in one
/// call, so the descent is explicit: each `openat2` resolves exactly one name
/// against the descriptor of the directory above it, and each one carries
/// `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`. That
/// removes the whole class the previous code left open — it resolved the
/// whole path in one call with only `RESOLVE_NO_MAGICLINKS`, so resolution
/// *did* follow symlinks and only the `(st_dev, st_ino)` comparison
/// afterwards stopped the attack.
///
/// # Why `RESOLVE_NO_XDEV` is not here, and where it is instead
///
/// All four flags belong together, and `marks::walk_and_mark` uses all four —
/// correctly, because below the root a mount point is a boundary the walk
/// must not cross. Getting *to* the root is the opposite case: a sync root
/// normally lives on a filesystem of its own. Measured on this host,
/// descending to `/home/<user>` from `/` with `RESOLVE_NO_XDEV` fails at the
/// first component with **`EXDEV`**, because `/home` is a separate mount —
/// so applying it here would make the helper unable to re-open the ordinary
/// sync root at every startup, which means no interception, which means
/// every placeholder reads as zeros. The cross-device question that actually
/// matters is answered instead by the `(st_dev, st_ino)` check below: the
/// directory reached must be the exact one that was registered, mount points
/// on the way in or not.
fn open_beneath(path: &str) -> io::Result<File> {
    let mut dir = File::open("/")?;
    let how = OpenHow::new()
        .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
        .resolve(
            ResolveFlag::RESOLVE_BENEATH
                | ResolveFlag::RESOLVE_NO_SYMLINKS
                | ResolveFlag::RESOLVE_NO_MAGICLINKS,
        );
    for component in path.split('/').filter(|c| !c.is_empty()) {
        if component == "." || component == ".." {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{path} is not a resolved absolute path"),
            ));
        }
        dir = File::from(openat2(dir.as_fd(), component, how)?);
    }
    Ok(dir)
}

/// Re-opens a registered root and proves it is still the same directory.
///
/// This is the check that stops a user having the helper mark directories it
/// was never given. The helper walks as root and reloads roots from a path
/// string, so between registration and the next startup that path can have
/// become something else entirely. Two things answer that: [`open_beneath`]
/// refuses to resolve through a symlink or a magic link at all, and the
/// `(st_dev, st_ino)` comparison then requires that what was reached is the
/// very directory that was registered.
///
/// The second check is deliberately not load-bearing on its own. Measured on
/// ext4, deleting a directory and creating another in the same parent reused
/// the same inode number on the first attempt (Btrfs and XFS did not), so
/// `(dev, ino)` equality is not proof of identity on every filesystem — which
/// is exactly why resolution is no longer allowed to wander.
fn reopen_and_verify(path: &str, dev: u64, ino: u64) -> io::Result<File> {
    let dir = open_beneath(path)?;
    let meta = dir.metadata()?;
    if meta.dev() != dev || meta.ino() != ino {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{path} is now dev={} ino={}, not the dev={dev} ino={ino} it was registered as",
                meta.dev(),
                meta.ino()
            ),
        ));
    }
    Ok(dir)
}

fn open_root(root: &roots::Root) -> io::Result<File> {
    let dir = reopen_and_verify(&root.path, root.dev, root.ino)?;
    if dir.metadata()?.uid() != root.uid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is no longer owned by uid {}", root.path, root.uid),
        ));
    }
    Ok(dir)
}

/// Whether `root` overlaps anything already covered this startup: the
/// nesting rule (`docs/design/hydration.md` §11) applies at every boot, not
/// only at registration, because what a stored path leads to can change in
/// between.
fn overlap_with(covered: &[roots::Root], root: &roots::Root) -> Option<String> {
    let mut seen = roots::Roots::default();
    for other in covered {
        seen.insert(other.clone());
    }
    seen.nesting_conflict(&root.path, root.dev, root.ino).map(|conflict| match conflict {
        roots::Nesting::Inside(id) | roots::Nesting::Contains(id) | roots::Nesting::SameDirectory(id) => id,
    })
}

/// Re-opens, re-checks and walks one registered root. Returns whether it is
/// now covered, so the caller knows whether to compare later roots against it.
fn cover_root(shared: &Shared, root: &roots::Root) -> bool {
    let dir = match open_root(root) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::error!("root {} is not covered: {e}", root.root_id);
            lock(&shared.degraded_roots).insert(root.root_id.clone());
            return false;
        }
    };
    // The filesystem check is re-run too — a root can have been
    // moved onto a filesystem that cannot host placeholders since it was
    // registered. Only the `fstatfs` half: the feature probe writes a file,
    // and writing into every user's sync folder on every boot is both
    // unnecessary (it was probed at registration) and, once this root is
    // marked, exactly the self-interception hazard is about.
    if let Err(errno) = check_filesystem_type(&dir, &root.path) {
        tracing::error!(
            "root {} ({}) is on a filesystem konedrive cannot use (errno {errno}); not covering it",
            root.root_id,
            root.path
        );
        lock(&shared.degraded_roots).insert(root.root_id.clone());
        return false;
    }
    record_walk(shared, root, marks::walk_and_mark(&shared.marks, dir.as_fd(), &root.path));
    true
}

/// One unreadable subdirectory must never abort a root's walk, and
/// must never pass in silence either. Everything reachable is marked, every
/// failure is named, and the root is flagged degraded.
fn record_walk(shared: &Shared, root: &roots::Root, report: marks::WalkReport) {
    if !report.degraded() {
        tracing::info!("marked {} directories under {}", report.marked, root.path);
        lock(&shared.degraded_roots).remove(&root.root_id);
        return;
    }
    tracing::error!(
        "root {} ({}) is DEGRADED: {} directories marked, {} could not be covered; opens of \
         files in them will not be intercepted",
        root.root_id,
        root.path,
        report.marked,
        report.failures.len()
    );
    for failure in &report.failures {
        tracing::error!("  {failure}");
    }
    lock(&shared.degraded_roots).insert(root.root_id.clone());
}

/// Binds the control socket. This must be a genuine `SOCK_SEQPACKET` socket,
/// not the `SOCK_STREAM` that `std::os::unix::net::UnixListener` produces:
/// `konedrive_proto::Channel` frames exactly one message per datagram and
/// relies on that framing coming from the socket type itself. Two messages
/// sent back to back over a stream socket can coalesce into a single
/// `read()` — the first `recv` then fails to parse ("trailing characters")
/// and the second call blocks forever, since nothing else is ever going to
/// arrive. A `SOCK_SEQPACKET` socket keeps each `send()` as its own `recv()`,
/// so this cannot happen. (`Channel::new` now rejects the wrong socket type
/// outright; std has no `SOCK_SEQPACKET` listener, so the socket is built
/// directly with nix.)
fn listen() -> anyhow::Result<OwnedFd> {
    let path = Path::new(SOCKET_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let addr = UnixAddr::new(path)?;

    // A leftover socket file is not proof that nothing is listening on it, and
    // unlinking it unconditionally is how a second helper silently takes the
    // socket away from a running first one: the first keeps its bound
    // descriptor and its marks, and goes on owning every suspended open, while
    // every daemon now talks to the second, which has no idea those events
    // exist. Connecting is the only way to tell the two apart.
    if path.exists() {
        let probe = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)?;
        match connect(probe.as_raw_fd(), &addr) {
            Ok(()) => anyhow::bail!(
                "another konedrive-helper is already listening on {SOCKET_PATH}; refusing to \
                 take the socket away from it"
            ),
            Err(e) => tracing::info!("{SOCKET_PATH} is stale ({e}); replacing it"),
        }
        std::fs::remove_file(path)?;
    }

    let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)?;
    bind(fd.as_raw_fd(), &addr)?;
    sock_listen(&fd, Backlog::new(16)?)?;
    // Anyone may connect; every request is authorised by SO_PEERCRED plus the
    // ownership rules in roots.rs.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o666))?;
    Ok(fd)
}

/// Takes ownership of a permission event's fd, preserving its exact number.
///
/// `fanotify_write()` matches a permission response against the fd number
/// `read_events()` handed out for that event (`fanotify(7)`: "fd — This is
/// the file descriptor from the structure fanotify_event_metadata"). A
/// duplicate has a different number, so anything we may answer only after
/// this event's iteration of the read loop ends — the "ask the daemon and
/// wait" path in `handle_open` — must keep using this exact descriptor, not
/// a dup of it, and must not let it close before the response is written: a
/// permission event that is read but never answered leaves its opener
/// blocked until the whole fanotify group fd is closed (`fanotify(7)`),
/// which in practice means until the helper exits.
///
/// `FanotifyEvent::drop` would close this fd when the event goes out of
/// scope at the end of the read loop's iteration; `mem::forget` disarms that
/// so the `OwnedFd` we build from the same raw number is the sole owner.
fn take_fd(event: nix::sys::fanotify::FanotifyEvent) -> Option<OwnedFd> {
    let raw = event.fd()?.as_raw_fd();
    std::mem::forget(event);
    // SAFETY: `event.fd()` returned a valid, open descriptor owned by
    // `event`; forgetting `event` just above means nothing else will close
    // it, so this `OwnedFd` becomes its sole owner.
    Some(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// What the event loop does about an errno from `read_events`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadFailure {
    /// Everything queued has been read — or the event at the head of the
    /// queue could not be handed over and the kernel answered it itself
    /// — go back to `poll()`, which reports whatever is still
    /// queued at once.
    Drained,
    /// A signal interrupted the read; read again at once.
    Interrupted,
    /// The process, or the machine, is out of file descriptors.
    Exhausted,
    /// The kernel could not open one event's descriptor, answered that
    /// event `FAN_DENY` itself, and handed its errno back instead of it
    ///. Read on at once: the event is gone from the queue.
    EventRefused,
    /// The group's own descriptor, or the buffer it is read into, is broken.
    Fatal,
}

/// **The helper exiting is worse than the helper denying.**
///
/// `fanotify(7)` is explicit that closing the group's descriptor sets every
/// outstanding permission event to *allowed*, so a helper that dies hands
/// every suspended open straight through and each of those applications reads
/// a placeholder full of zeros — silently, with nothing to notice it. A denial
/// is the opposite: visible, answerable, and something the application can
/// retry. So the bar for ending the process is very high, and running out of
/// descriptors does not come near it.
///
/// `EMFILE` and `ENFILE` are the errnos that failure arrives as, and they are
/// self-correcting: the descriptors the helper is short of are the event fds
/// of opens that are still in flight, and every one of them is released as its
/// hydration finishes. The kernel has already denied the events it could not
/// copy out, so the opens caught in the window are answered rather than left
/// hanging; the loop's job is simply to still be there afterwards.
///
/// # Every other errno is one event's
///
/// The kernel creates each permission event's descriptor inside our `read()`
/// — `dentry_open()` with the group's `O_RDWR`, against the **opener's**
/// mount — and when that open fails, `fanotify_read()` answers the event
/// `FAN_DENY` itself, stops, and returns the events before it or, if there
/// were none, that open's errno. The errno describes one event the kernel has
/// already dealt with, not the group. Measured in the VM suite, on Btrfs,
/// ext4 and XFS: `EROFS` for an open through a read-only mount — a read-only
/// bind, `ProtectHome=read-only`, a Flatpak app with `home:ro` — and
/// `ETXTBSY` for a second open of an executable that is running. Both used
/// to fall into `Fatal`: the helper exited, the kernel allowed every open
/// suspended at that moment, and a reader waiting for a hydration got 65 536
/// zero bytes. Any local user could do it to everybody, and `Restart=always`
/// made it a crash loop. The opener of the refused event got `EPERM` in 8 ms
/// either way; the helper now simply reads on.
///
/// It cannot make the loop spin: `fanotify_read()` takes each event off the
/// queue before it tries to open its descriptor, so every such errno stands
/// for one event consumed — read on, and the queue drains to `EAGAIN` as it
/// always does.
///
/// Only what the group's own descriptor can report is fatal: `EBADF`,
/// `EINVAL` (a buffer too small for one event, or a broken group) and
/// `EFAULT` mean the thing this whole process exists to read is broken, and
/// staying alive around it buys nothing, since a group that cannot be read
/// cannot be answered either.
fn classify_read_failure(e: Errno) -> ReadFailure {
    match e {
        Errno::EAGAIN => ReadFailure::Drained,
        Errno::EINTR => ReadFailure::Interrupted,
        Errno::EMFILE | Errno::ENFILE => ReadFailure::Exhausted,
        Errno::EBADF | Errno::EINVAL | Errno::EFAULT => ReadFailure::Fatal,
        _ => ReadFailure::EventRefused,
    }
}

/// Answers permission events. Every event is only inspected here — mask and
/// pid are plain values, and the fd is handed off immediately — because
/// kernel fact 2 means any open our own code causes (a `stat`, an xattr
/// read, anything) inside a marked directory raises another event aimed at
/// us. `handle_open` runs its own file access and, on the "ask the daemon"
/// path, its own bounded wait on a worker thread, never on this one: that
/// wait can take up to `DAEMON_WAIT`, and blocking here would stall every
/// other pending open in the system for that long.
///
/// Nothing here waits on anything but the group: the event
/// descriptors the kernel creates inside `read_events()` are `O_NONBLOCK`, so
/// a file somebody holds a lease on cannot stop the loop — see `Marks::new`.
///
/// `read_events()` is nonblocking (kernel fact 3: the group is created with
/// `FAN_NONBLOCK`, or it would never return `EAGAIN` at all), specifically so
/// this loop can `poll()` the group fd instead of calling a blocking
/// `read()` directly: `poll()` blocks with no CPU cost until there is
/// something to read, then the inner loop drains everything currently
/// queued before polling again. A `read_events()`-then-`continue`-on-EAGAIN
/// loop with no wait in between would be a busy spin pinning a CPU core for
/// as long as the helper runs, which is not acceptable for a permanent
/// system service.
fn event_loop(shared: &Arc<Shared>, pool: &pool::Pool) -> anyhow::Result<()> {
    let mut exhaustion = Throttle::new();
    let own_pid = std::process::id() as i32;
    loop {
        let mut fds = [PollFd::new(shared.marks.group().as_fd(), PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::NONE) {
            Ok(_) => {}
            Err(Errno::EINTR) => continue,
            Err(e) => return Err(e.into()),
        }
        let mut first_read = true;
        loop {
            // Before the read, so that an event is never given a count later
            // than one it could have been queued under (see
            // `mark_while_hydrated`).
            let since = shared.unregistrations.now();
            let events = match shared.marks.group().read_events() {
                Ok(events) => {
                    let unreported = exhaustion.reset();
                    if unreported > 0 {
                        tracing::error!(
                            "descriptors are available again; {unreported} more read(s) of the \
                             fanotify group had failed since the last report"
                        );
                    }
                    first_read = false;
                    events
                }
                Err(e) => match classify_read_failure(e) {
                    // The event descriptors are `O_NONBLOCK`, so
                    // an event whose file is leased cannot be handed over:
                    // the kernel answers it `FAN_DENY` itself and this read
                    // reports `EAGAIN`, as it does for an empty queue. The
                    // opener has its answer (`EPERM`) and the next `poll`
                    // sees whatever is still queued; only the journal can
                    // say it happened, so the first read after `poll`
                    // finding nothing is counted. A lower bound: an event
                    // that fails behind others in the same read is answered
                    // the same way, and the read returns the ones before it.
                    ReadFailure::Drained => {
                        if first_read {
                            shared.refusals.report(Refusal::Unopenable, || UNOPENABLE.to_owned());
                        }
                        break;
                    }
                    ReadFailure::Interrupted => continue,
                    ReadFailure::Exhausted => {
                        // Its own line, below; not also an unopenable one.
                        first_read = false;
                        if let Some(occurrences) = exhaustion.admit() {
                            tracing::error!(
                                "out of file descriptors reading the fanotify group ({e}); \
                                 {occurrences} read(s) failed, the kernel denies the events it \
                                 cannot hand over, and this loop keeps the group open and retries \
                                 every {EXHAUSTION_BACKOFF:?}. Raise LimitNOFILE in the unit if \
                                 this persists"
                            );
                        }
                        std::thread::sleep(EXHAUSTION_BACKOFF);
                        continue;
                    }
                    // The kernel has denied that one event
                    // (`EPERM` at its opener) and taken it off the queue;
                    // whatever is behind it is read next. Its own line, not
                    // also an unopenable one.
                    ReadFailure::EventRefused => {
                        first_read = false;
                        shared.refusals.report(Refusal::EventFdFailed, || {
                            format!(
                                "{EVENT_FD_FAILED} ({e}) — an open through a read-only mount \
                                 (EROFS) or of an executable that is running (ETXTBSY), most \
                                 likely"
                            )
                        });
                        continue;
                    }
                    ReadFailure::Fatal => return Err(e.into()),
                },
            };
            for event in events {
                let mask = event.mask();
                if mask.contains(MaskFlags::FAN_Q_OVERFLOW) {
                    tracing::warn!("queue overflow: some opens were not seen");
                    continue;
                }
                if !mask.contains(MaskFlags::FAN_OPEN_PERM) {
                    // We only ever mark FAN_OPEN_PERM, so this should not
                    // happen. The event simply drops: there is no permission
                    // decision pending on an event of a kind we never asked for.
                    continue;
                }
                let pid = event.pid();
                let Some(fd) = take_fd(event) else {
                    tracing::warn!("a permission event arrived with no descriptor");
                    continue;
                };
                // The helper's own opens: an `OpenByHandle` object in a
                // marked directory, or with a mark of its own, raises
                // an event aimed at this very group, while the connection
                // thread that opened it waits in `open_by_handle_at` and
                // reads nothing more from its daemon — so a hydration asked
                // of that daemon could never be reported back. Allowed here,
                // on this thread, before the pool: no worker, no daemon, and
                // not behind a full queue. The event's pid is the process's,
                // whichever thread opened (no FAN_REPORT_TID; pinned by
                // marks.rs's INIT_FLAGS and its test). The only files
                // the helper opens are those objects, handed straight to
                // their owner's daemon, and the feature probe's nameless
                // file at registration (`docs/design/writes.md` §8.2; SECURITY.md); measured in
                // docs/kernel-behavior-7.2.md §15.
                if pid == own_pid {
                    respond_allow(shared, fd);
                    continue;
                }
                if let Err(rejected) = pool.submit(pool::OpenEvent { fd, pid, since }) {
                    // Saturation, not failure: EAGAIN tells the application to
                    // try the open again, which is true and is an answer. The
                    // alternative — spawning without bound — ends with the
                    // process dying and the kernel allowing every suspended
                    // open in the system.
                    shared.refusals.report(Refusal::PoolFull, || {
                        format!(
                            "all {EVENT_WORKERS} workers busy and {EVENT_QUEUE_DEPTH} opens \
                             already queued; denying an open with EAGAIN"
                        )
                    });
                    respond_deny(shared, rejected.fd, libc::EAGAIN);
                }
            }
        }
    }
}

/// Takes the event fd out of the slot the worker holds it in.
///
/// The slot exists for. The worker keeps ownership of the
/// descriptor *outside* the `catch_unwind` boundary and lends this function a
/// `&mut Option<OwnedFd>`, so a panic anywhere below does not drop the fd
/// while unwinding — the worker still has it and can deny `EIO` with the
/// original descriptor. That matters because a response is matched by fd
/// *number* (`docs/kernel-behavior-7.2.md` §5.1): once the number is closed
/// it can be recycled, and answering a recycled number would answer somebody
/// else's event. Taking it here, at the exact moment it is consumed, is also
/// what makes "every path answers exactly once" checkable by reading.
fn claim(slot: &mut Option<OwnedFd>) -> OwnedFd {
    slot.take().expect("an intercepted open is answered exactly once")
}

/// Decides one intercepted open and always answers it — allow, deny, or a
/// move into a hydration job that guarantees a later answer from `finish` —
/// before returning. No path may leave `slot` full without one of those
/// three; doing so would leave the opener blocked forever (see `take_fd`).
///
/// `since` is the count of root unregistrations when the event was read (see
/// [`mark_while_hydrated`]).
///
/// # No duplicate outlives the answer
///
/// The file is inspected through duplicates of the event fd — kernel fact 1
/// rules out opening it ourselves, but not `dup()`ing one we did not open —
/// and each is closed as soon as it has been read, never held across an
/// answer. A duplicate shares the event's `O_RDWR` open file, so while one
/// is open the file counts as open for writing: the daemon's registration
/// probe, which creates a file in an already-marked root and at once takes a
/// write lease on it, found the lease refused when this function still held
/// one after it had allowed the probe's open.
fn handle_open(shared: &Shared, slot: &mut Option<OwnedFd>, opener_pid: i32, since: u64) {
    let meta = match metadata_of(slot.as_ref().expect("the event fd is still here").as_fd()) {
        Ok(meta) => meta,
        Err(e) => {
            tracing::error!("cannot stat an intercepted open: {e}");
            respond_deny(shared, claim(slot), libc::EIO);
            return;
        }
    };
    if !meta.is_file() {
        respond_allow(shared, claim(slot));
        return;
    }
    let owner = meta.uid();
    let dev = meta.dev();
    let ino = meta.ino();
    // Compiled in only with `fault-injection`, armed only by the VM suite
    //; an empty function otherwise. Placed after the
    // descriptor has been taken out of `slot`'s reach and before any
    // decision, so the unwind it causes is exactly the one
    // describes: the worker still owns the event fd and can answer `EIO`.
    fault::panic_on_size(meta.len());

    // The owning daemon's own opens bypass everything
    // else: it must be able to re-open files it left `hydrating` or
    // `dehydrating` during startup recovery, and treating that open like any
    // other would mean asking the very daemon that is blocked on it to
    // hydrate the file — a deadlock.
    //
    // The exemption is deliberately narrow. It is not "this pid connected to
    // our socket": anyone can do that, and an unscoped version of this check
    // let any local process be exempted from interception of any file. It is
    // "this pid holds a connection that owns a registered root, and the file
    // being opened belongs to that same user". A process with no root gets
    // nothing, and no daemon is ever exempted from another user's files.
    //
    // Dehydration (`konedrived/src/sync/root.rs::dehydrate`) used
    // to depend on this, and deliberately no longer does: it
    // opened the file again, by path, after clearing the ignore mark, and
    // that open was let through only because it hit this exemption first.
    // It now does the whole sequence on the one descriptor it opened before
    // the first check, so it raises no open of its own at all. Nothing else
    // should acquire such a dependency: what the exemption covers is startup
    // recovery, not a way to open a file the state check would have handled.
    if daemon_is_exempt(shared, opener_pid, owner) {
        respond_allow(shared, claim(slot));
        return;
    }

    let mut state = state_of(slot.as_ref().expect("the event fd is still here").as_fd());
    // At most two turns: the second only when the file stopped reading
    // `hydrated` between the first read and the mark, and then it is not
    // `hydrated` any more.
    loop {
        match state {
            // The ignore mark is added only to a file whose state
            // is `hydrated` — its content is actually present. A file with no
            // konedrive xattrs at all is not managed by us and is let through,
            // but it must NOT get an ignore mark, because a placeholder under
            // construction looks exactly like that. `create_placeholder`
            // (konedrive_fs::placeholder) opens a nameless O_TMPFILE in the
            // directory, writes the size, item id, state and mtime through
            // that descriptor, and only then links it in by name — so no name
            // ever shows a half-built file. But the O_TMPFILE open is itself
            // an open in a marked directory and raises a FAN_OPEN_PERM
            // (kernel fact 7, docs/kernel-behavior-7.2.md §7) before a single
            // xattr exists; that is the event that arrives here with none.
            // Answered with an ignore mark, the mark — which survives
            // modification — would still be on the inode when the finished
            // `online-only` placeholder is linked in, and every open of it
            // would be let through to zeros. The owning daemon's own builds
            // are normally allowed by the exemption above and never get this
            // far; this rule covers a build by anyone it does not.
            //
            // And the state is read once more *after* the mark is placed
            //: see `mark_while_hydrated`.
            Ok(Some(State::Hydrated)) => {
                // `fault-injection` builds only: the VM suite's I1 scenario.
                fault::delay_before_ignore_mark();
                let fd = slot.as_ref().expect("the event fd is still here").as_fd();
                match mark_while_hydrated(shared, fd, FileId { owner: Some(owner), dev, ino }, since) {
                    Ok(()) => respond_allow(shared, claim(slot)),
                    Err(now) => {
                        tracing::info!(
                            "dev={dev} ino={ino} stopped reading hydrated while its open was \
                             being decided (it now reads {now:?}); deciding again"
                        );
                        state = now;
                        continue;
                    }
                }
            }
            Ok(None) => {
                // A file with no state attribute is not ours — unless it also
                // carries an item id, in which case it is one of ours with its
                // state missing, and we have no idea whether its body is there.
                // §5.2's last rule applies: never allow zeros.
                match item_id_of(slot.as_ref().expect("the event fd is still here").as_fd()) {
                    Ok(None) => respond_allow(shared, claim(slot)),
                    Ok(Some(item)) => {
                        tracing::error!(
                            "dev={dev} ino={ino} carries item id {item} but no state attribute; \
                             denying rather than risk serving an unfilled placeholder"
                        );
                        respond_deny(shared, claim(slot), libc::EIO);
                    }
                    Err(e) => {
                        tracing::error!("cannot read the item id on dev={dev} ino={ino}: {e}");
                        respond_deny(shared, claim(slot), libc::EIO);
                    }
                }
            }
            Ok(Some(_)) => hydrate(shared, slot, owner, dev, ino, since),
            Err(StateError::Corrupt(value)) => {
                tracing::error!(
                    "dev={dev} ino={ino} has an unrecognised state {value:?}; denying rather than \
                     risk serving an unfilled placeholder"
                );
                respond_deny(shared, claim(slot), libc::EIO);
            }
            Err(StateError::Io(e)) => {
                tracing::error!("unreadable xattrs on dev={dev} ino={ino}: {e}");
                respond_deny(shared, claim(slot), libc::EIO);
            }
        }
        return;
    }
}

/// Places the ignore mark on a file just read `hydrated`, and keeps it only
/// if the file still reads `hydrated` **after** the mark is in place, and no
/// root has been unregistered since its open was read. `Err` carries what
/// the file reads now, when that is not `hydrated`; the mark is off again by
/// then. Every place the helper marks a file goes through here.
///
/// # Why after
///
/// "Read `hydrated`, then mark" is two steps, and a dehydration can fall
/// between them: it makes `dehydrating` durable and then has the helper
/// `ClearIgnore` (step 2). A mark placed after that `ClearIgnore`,
/// on the strength of a read made before the `dehydrating`, was outlived by
/// the punch: measured with a 1.5 s stall injected between the two steps,
/// the next reader got 65 536 zero bytes after no fetch, on Btrfs, ext4 and
/// XFS. Reading again after the mark closes it from both sides. If the
/// `dehydrating` came first, the second read sees it, and the mark comes off
/// here. If the mark came first, it was there for the `ClearIgnore` to
/// remove. So a mark is left only on a file that read `hydrated` at a moment
/// the mark was already in place.
///
/// # Why "no unregistration since" (second guard)
///
/// A root's unregistration walk takes the ignore mark off every file it
/// passes, and a hydration still in flight then — or an open read off the
/// queue before its directory was unmarked — used to mark its file after
/// the walk had gone by. None of that can empty a marked file any more: the
/// daemon's local rule has every punch clear the mark first, or not punch
///. This guard, like the registration walk's clearing
/// (`marks::walk_and_mark`), is defence in depth: a mark it withholds is one
/// nothing has to clear later. `unregistrations` is bumped
/// before an unregistration's walk begins and again after it ends. Whatever
/// was read before either bump for the file owner's uid is not marked: the file is still let
/// through, since its content is there, but its next open is simply decided
/// again. The helper cannot tell from a descriptor which root a file is in,
/// so the count is kept per uid and matched by the file's owner (see
/// [`Unregistrations`]): an unregistration costs that user's files being
/// decided at that moment one extra event each, later, and nobody else's.
///
/// What this guard cannot see is an open that the kernel queued before its
/// directory was unmarked and the event loop read only after the walk had
/// ended; the mark that leaves is harmless on a file with content, and the
/// daemon clears it before it ever empties the file.
fn mark_while_hydrated(
    shared: &Shared,
    fd: BorrowedFd<'_>,
    file: FileId,
    since: u64,
) -> Result<(), Result<Option<State>, StateError>> {
    let FileId { owner, dev, ino } = file;
    place_ignore_mark(shared, fd, dev, ino);
    let now = state_of(fd);
    let hydrated = matches!(now, Ok(Some(State::Hydrated)));
    let unregistered = shared.unregistrations.since(since, owner);
    if hydrated && !unregistered {
        return Ok(());
    }
    if let Err(e) = shared.marks.clear_ignore(fd) {
        // Practically unreachable (a removal allocates nothing, and the
        // descriptor is the one just marked through), and not left
        // unguarded if it happens: a file that is not `hydrated` is not let
        // through here, and a dehydration of it cannot take its lease while
        // this open's descriptor is held; a stale mark on a `hydrated` file
        // is harmless while the file holds its content, and the daemon clears
        // it before it empties the file.
        tracing::error!(
            "cannot take the ignore mark off dev={dev} ino={ino} again ({e}); it reads {now:?}"
        );
    }
    if hydrated {
        tracing::info!(
            "dev={dev} ino={ino} was let through without an ignore mark: a root was unregistered \
             while its open was being decided"
        );
        return Ok(());
    }
    Err(now)
}

/// The "ask the daemon" path: coalesce by inode, register the opener, then
/// send. Registering before sending is the whole point — a daemon that
/// answers immediately would otherwise find an empty job, finish it, and
/// leave this opener suspended with nothing left to answer it.
fn hydrate(
    shared: &Shared,
    slot: &mut Option<OwnedFd>,
    owner_uid: u32,
    dev: u64,
    ino: u64,
    since: u64,
) {
    // The descriptor stays in `slot` across the wait — the step here that
    // takes locks and sleeps, and could therefore panic on somebody else's
    // bug — so guarantee still holds over it.
    let daemon = match wait_for_daemon(shared, owner_uid) {
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
            respond_deny(shared, claim(slot), libc::EIO);
            return;
        }
    };
    let owner = Owner { uid: daemon.uid, conn: daemon.conn };

    // The event fd itself goes into the job, before anything is sent; the
    // daemon is sent a duplicate, made under the jobs lock (see
    // `jobs::Dispatch`). A `SCM_RIGHTS` copy of either is the same open file
    // description.
    let enrollment = lock(&shared.jobs).enroll((dev, ino), owner, claim(slot), since);
    let gone = matches!(enrollment.outcome, Enrolled::ConnectionGone);
    for stranded in enrollment.evicted {
        if gone {
            // This connection's cleanup already ran, so nothing
            // would ever answer a job created on it.
            tracing::warn!("the daemon connection went away while this open was being handled");
        } else {
            tracing::warn!(
                "a hydration of this file was in hand for another uid, which no longer owns it; \
                 denying its openers EIO"
            );
        }
        respond_deny(shared, stranded, libc::EIO);
    }
    // `New` comes with its request to send. `Queued` has none yet: the
    // opener is enrolled and stays suspended until a returning credit sends
    // it. `Existing` asked for nothing, and `ConnectionGone`
    // was answered above.
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
fn dispatch(shared: &Shared, outbox: &Outbox, owner: Owner, mut next: Option<jobs::Dispatch>) {
    while let Some(jobs::Dispatch { req_id, fd }) = next.take() {
        let (errno, why) = match fd {
            Ok(fd) => {
                let request = Outgoing { message: ToDaemon::HydrateRequest { req_id }, fd: Some(fd) };
                match outbox.try_send(request) {
                    Ok(()) => return,
                    Err(_) if outbox.is_closed() => (libc::EIO, "the connection is over".to_owned()),
                    Err(_) => (libc::EAGAIN, "its request capacity is taken".to_owned()),
                }
            }
            Err(e) => (libc::EIO, format!("cannot duplicate an event fd for the daemon: {e}")),
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
        next = settle(shared, req_id, owner, errno, Finish::Undeliverable);
    }
}

/// narrow exemption. `SO_PEERCRED` supplies the pid, so the
/// caller cannot claim to be a daemon it is not; owning a registered root is
/// what separates the user's daemon from any process that merely connected.
///
/// Only the file owner's **top** connection is exempt: the one
/// its hydrations go to, which is the daemon in every case but a transient
/// same-uid connection sitting on top of it — and that one is exempt only for
/// files of its own uid, which it can read anyway.
fn daemon_is_exempt(shared: &Shared, opener_pid: i32, file_owner: u32) -> bool {
    let on_top = lock(&shared.daemons).is_top_pid(file_owner, opener_pid);
    on_top && lock(&shared.roots).has_root_for(file_owner)
}

fn place_ignore_mark(shared: &Shared, fd: BorrowedFd<'_>, dev: u64, ino: u64) {
    // Not verified by reading `/proc/self/fdinfo/<group>`: that is O(marks)
    // per hydration, and the helper holds one mark per directory in every
    // sync tree on the machine. VM suite asserts on fdinfo instead,
    // where the cost does not matter and the assertion is worth making — the
    // syscall's return value is known to lie about this (M3).
    if let Err(e) = shared.marks.ignore_file(fd) {
        tracing::error!(
            "cannot place the ignore mark on dev={dev} ino={ino}: {e}; every open of this file \
             will keep raising a permission event"
        );
    }
}

fn respond_allow(shared: &Shared, fd: OwnedFd) {
    if let Err(e) = shared.marks.allow(fd.as_fd()) {
        tracing::error!("cannot allow an intercepted open: {e}");
    }
}

fn respond_deny(shared: &Shared, fd: OwnedFd, errno: i32) {
    if let Err(e) = shared.marks.deny(fd.as_fd(), errno) {
        tracing::error!("cannot deny an intercepted open: {e}");
    }
}

/// Why there is no daemon to ask. Three different facts about the system,
/// which used to be reported as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoDaemon {
    /// This uid has registered no root, so no daemon of theirs could hydrate
    /// anything. Answered immediately; nothing was waited for.
    NoRoot,
    /// Either this uid's [`MAX_DAEMON_WAITERS`] slots, or the machine-wide
    /// [`GLOBAL_MAX_DAEMON_WAITERS`] backstop, are all taken. Answered
    /// immediately; nothing was waited for.
    TooManyWaiters,
    /// Waited the full [`DAEMON_WAIT`] and no daemon connected.
    TimedOut,
}

/// Waits for the owning user's daemon to connect, up to `DAEMON_WAIT`.
/// Woken by `daemon_arrived` the instant one registers, rather than polling.
///
/// puts two limits on the waiting, because this is the only place
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
/// The refusal says which of the three happened. `hydrate` logs it; the reason
/// never changes the answer, which is always `EIO`.
fn wait_for_daemon(shared: &Shared, uid: u32) -> Result<Daemon, NoDaemon> {
    // The overwhelmingly common case: the daemon is already there, and
    // nothing below applies.
    if let Some(daemon) = lock(&shared.daemons).top(uid) {
        return Ok(daemon.clone());
    }
    if !lock(&shared.roots).has_root_for(uid) {
        return Err(NoDaemon::NoRoot);
    }
    let Some(_slot) = WaiterSlot::take(&shared.daemon_waiters, uid) else {
        return Err(NoDaemon::TooManyWaiters);
    };

    let deadline = Instant::now() + DAEMON_WAIT;
    let mut daemons = lock(&shared.daemons);
    loop {
        if let Some(daemon) = daemons.top(uid) {
            return Ok(daemon.clone());
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(NoDaemon::TimedOut);
        }
        let (guard, _) = shared
            .daemon_arrived
            .wait_timeout(daemons, deadline - now)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        daemons = guard;
    }
}

fn serve(shared: Arc<Shared>, listener: OwnedFd) {
    let mut failing = Throttle::new();
    // Connections are numbered here, on the one thread that
    // accepts them, in the order they were accepted — never on the
    // per-connection thread. Numbered there, two connections accepted a
    // moment apart could draw their numbers in either order, and "newer"
    // would mean "whose thread the scheduler ran first". `Registry::register`
    // relies on this order being the accept order. A local counter rather
    // than a shared one, so that nothing else can ever hand one out.
    let mut next_conn: u64 = 0;
    loop {
        let fd = match accept(listener.as_raw_fd()) {
            Ok(fd) => {
                let unreported = failing.reset();
                if unreported > 0 {
                    tracing::error!(
                        "accept works again; it had failed {unreported} more time(s) since the \
                         last report"
                    );
                }
                fd
            }
            Err(Errno::EINTR) => continue,
            Err(e) => {
                // Retrying immediately is right for a transient
                // error and catastrophic for a persistent one: on `EMFILE`
                // `accept` fails as fast as the CPU can call it, so this loop
                // pinned a core and flooded the journal at the exact moment
                // the machine was already short of descriptors. Backing off
                // costs a connection setup 50 ms and nothing else.
                if let Some(occurrences) = failing.admit() {
                    tracing::error!(
                        "accept failed: {e} ({occurrences} time(s)); retrying every \
                         {ACCEPT_BACKOFF:?}"
                    );
                }
                std::thread::sleep(ACCEPT_BACKOFF);
                continue;
            }
        };
        // SAFETY: `accept` returned a freshly opened descriptor we now own.
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        // a bounded number of connections per uid,
        // counted here, before a thread is spent on one. A peer whose
        // credentials cannot be read is not served at all — `serve_one`
        // would refuse it too.
        let slot = match getsockopt(&stream, PeerCredentials) {
            Ok(peer) => match ConnectionSlot::take(&shared.connections, peer.uid()) {
                Some(slot) => slot,
                None => {
                    let uid = peer.uid();
                    shared.refusals.report(Refusal::TooManyConnections, || {
                        format!(
                            "uid {uid} is already holding {MAX_CONNECTIONS_PER_UID} connections; \
                             closing another one as soon as it was accepted"
                        )
                    });
                    continue;
                }
            },
            Err(e) => {
                tracing::warn!("cannot read a new connection's credentials: {e}; closing it");
                continue;
            }
        };
        next_conn += 1;
        let conn = next_conn;
        let shared = Arc::clone(&shared);
        if let Err(e) = std::thread::Builder::new()
            .name("konedrive-daemon".into())
            .spawn(move || {
                let _slot = slot;
                if let Err(e) = serve_one(&shared, stream, conn) {
                    tracing::info!("daemon connection ended: {e}");
                }
            })
        {
            tracing::error!("cannot serve a new connection: {e}");
        }
    }
}

/// Runs a connection's cleanup exactly once, however the connection ends
///.
///
/// This used to be plain statements after an immediately-invoked closure, so
/// a panic anywhere in the request loop unwound straight past them. The
/// damage was not the lost log line: the `Daemon` stayed in `shared.daemons`,
/// so every later hydration for that uid was addressed to a connection
/// nobody was reading, and its suspended openers were never denied. A `Drop`
/// guard runs during unwinding as well as on the ordinary path, which is the
/// only version of this that is true regardless of what the loop did.
struct Disconnect<'a> {
    shared: &'a Shared,
    uid: u32,
    conn: u64,
    outbox: Arc<Outbox>,
}

impl Drop for Disconnect<'_> {
    fn drop(&mut self) {
        // Stops anything else queueing onto a connection that is finished,
        // and unblocks both of its threads.
        self.outbox.close();

        // This connection only, wherever it sits: an older one
        // going away leaves a newer one on top, and a newer one going away
        // hands the uid back to whichever live connection is under it.
        lock(&self.shared.daemons).deregister(self.uid, self.conn);
        // Everything *this connection* was going to hydrate now fails rather
        // than hangs. Not everything in the system: the socket is 0666, and
        // draining every pending job on any disconnect let any local user
        // fail every hydration on the machine with a connect-and-close loop.
        // `retire` marks the connection dead before it drains,
        // so a worker still holding a `Daemon` clone cannot slip a new job in
        // behind the drain.
        let (stranded, still_running) = {
            let mut jobs = lock(&self.shared.jobs);
            let stranded = jobs.retire(self.conn);
            (stranded, jobs.in_flight())
        };
        let suspended: usize = stranded.iter().map(Vec::len).sum();
        if suspended > 0 {
            tracing::warn!(
                "uid {} connection {} went away with {} hydrations in flight; denying {suspended} \
                 suspended opens EIO ({still_running} hydrations for other connections are \
                 untouched)",
                self.uid,
                self.conn,
                stranded.len()
            );
        }
        for waiters in stranded {
            for fd in waiters {
                respond_deny(self.shared, fd, libc::EIO);
            }
        }
    }
}

/// Serves one accepted connection. `conn` was assigned by [`serve`] at accept
/// time and is what orders this connection against any other
/// from the same uid.
fn serve_one(shared: &Shared, stream: UnixStream, conn: u64) -> anyhow::Result<()> {
    // `std::os::unix::net::UnixStream::peer_cred` is still unstable
    // (`peer_credentials_unix_socket`) on this toolchain, so SO_PEERCRED is
    // read via nix's getsockopt instead — the same kernel-attached
    // credential, through a stable API. SO_PEERCRED works the same way on a
    // SOCK_SEQPACKET socket as on a stream one, and the kernel attaches it at
    // connect() time, so the peer cannot forge any part of it.
    let peer = getsockopt(&stream, PeerCredentials)?;
    let uid = peer.uid();
    let pid = peer.pid();
    let owner = Owner { uid, conn };

    // Reading and writing get their own descriptor. They must: a connection
    // sitting in `recv` waiting for the daemon's next request would otherwise
    // hold whatever `HydrateRequest` needs, and the daemon would be waiting
    // for exactly that request. Interleaving is safe: the daemon's reader
    // tells `Ack` from `HydrateRequest` by variant and only pairs `Ack`s with
    // its outstanding calls (see konedrived/src/sync/helper.rs), and one
    // writer thread keeps each `send` a single datagram in queue order.
    //
    // Sending is a bounded queue plus that thread, not a mutex around the
    // socket: `Channel::send` blocks, and a peer that stops
    // reading must cost this connection, never a worker thread.
    let mut reader = Channel::new(stream.try_clone()?)?;
    let outbox =
        Arc::new(Outbox::start(stream.try_clone()?, stream, &format!("{uid}-{conn}"))?);

    // Registered before the greeting is queued, so that the cleanup is armed
    // from the first instant there is anything to clean up.
    let _disconnect =
        Disconnect { shared, uid, conn, outbox: Arc::clone(&outbox) };

    // The helper greets unprompted, before it reads anything. client
    // relies on that, and requiring a `Hello` would buy nothing: `SO_PEERCRED`
    // already tells us who the peer is, and a `Hello` carries only a version
    // number the peer could lie about.
    if outbox
        .try_send(Outgoing { message: ToDaemon::Welcome { version: PROTOCOL_VERSION }, fd: None })
        .is_err()
    {
        anyhow::bail!("cannot greet a new daemon connection");
    }
    {
        let mut daemons = lock(&shared.daemons);
        let daemon = Daemon { conn, uid, pid, outbox: Arc::clone(&outbox) };
        if !daemons.register(daemon) {
            tracing::info!(
                "uid {uid} connection {conn} registered after a newer connection from the same \
                 uid; it stays underneath, and takes over only if the newer one goes"
            );
        }
        shared.daemon_arrived.notify_all();
    }

    loop {
        // Only a transport or deserialisation failure ends the connection.
        // Anything `apply` runs into is this request's problem and comes back
        // as an errno on this request's `Ack`.
        let (message, fd) = reader.recv::<ToHelper>()?;
        // Any message at all is proof of life, and is what
        // keeps a daemon that is slow to read — rather than wedged — from
        // being disconnected by its own backpressure.
        outbox.heard_from_peer();
        let mut reply = None;
        let errno = apply(shared, owner, &outbox, message, fd, &mut reply);
        // Into the room reserved for `Ack`s. This used to end
        // the connection when the outbox was full — tearing down, on
        // backpressure, a daemon that had just proved it was alive by sending
        // this request. Now an `Ack` is never refused: a peer with more
        // replies unread than any daemon has calls in flight is made to wait
        // here, and this thread reads nothing more from it until it catches
        // up. Only a connection that is already over refuses one.
        if outbox.send_ack_with(errno, reply).is_err() {
            anyhow::bail!("the connection ended while acknowledging a request");
        }
    }
}

/// Applies one request, returning the errno to acknowledge with (0 = fine),
/// and in `reply` the descriptor the `Ack` carries, if any (`OpenByHandle`).
///
///: this never fails the connection. A `fanotify_mark` that returns
/// `ENOENT` because an evictable mark was already reclaimed is a routine
/// outcome, and turning it into a teardown took every in-flight hydration down
/// with it.
fn apply(
    shared: &Shared,
    owner: Owner,
    outbox: &Outbox,
    message: ToHelper,
    fd: Option<OwnedFd>,
    reply: &mut Option<OwnedFd>,
) -> i32 {
    let uid = owner.uid;
    let object = fd.map(File::from);
    let allowed = |object: &File| -> bool {
        match object.metadata() {
            Ok(meta) => lock(&shared.roots).may_act_on(uid, meta.dev(), meta.uid()),
            Err(e) => {
                tracing::warn!("cannot stat an object sent by uid {uid}: {e}");
                false
            }
        }
    };

    let owns_regular_file = |object: &File| -> bool {
        match object.metadata() {
            Ok(meta) => roots::may_clear_ignore(uid, meta.uid(), meta.is_file()),
            Err(e) => {
                tracing::warn!("cannot stat an object sent by uid {uid}: {e}");
                false
            }
        }
    };

    match (message, object) {
        (ToHelper::Hello { version }, _) if version == PROTOCOL_VERSION => 0,
        (ToHelper::Hello { .. }, _) => libc::EPROTO,
        (ToHelper::RegisterRoot { root_id }, Some(dir)) => register_root(shared, owner, root_id, dir),
        (ToHelper::UnregisterRoot { root_id }, _) => unregister_root(shared, uid, &root_id),
        (ToHelper::MarkDir, Some(dir)) if allowed(&dir) => act(shared.marks.mark_dir(dir.as_fd())),
        (ToHelper::UnmarkDir, Some(dir)) if allowed(&dir) => {
            act(shared.marks.unmark_dir(dir.as_fd()))
        }
        (ToHelper::MarkFile, Some(file)) if allowed(&file) => {
            // `fault-injection` builds only.
            fault::panic_on_mark_file();
            act(shared.marks.mark_file(file.as_fd()))
        }
        // On ownership of a regular file alone. Removing an
        // ignore mark can only cost an extra interception, never zeros, and
        // every punch in the daemon now asks for it whenever it has a link —
        // in a folder registered without interception too, where the uid may
        // hold no root at all.
        (ToHelper::ClearIgnore, Some(file)) if owns_regular_file(&file) => {
            act(shared.marks.clear_ignore(file.as_fd()))
        }
        (ToHelper::HydrateDone { req_id, errno }, _) => {
            // Answers its openers, and sends the hydration its credit goes to
            // next.
            let next = settle(shared, req_id, owner, errno, Finish::Reported);
            dispatch(shared, outbox, owner, next);
            0
        }
        // Authorised on the object it finds, not on the handle: see
        // `by_handle`. Refusals are not logged — they are the daemon's
        // answer, and any local user can ask.
        (ToHelper::OpenByHandle { handle_type, handle }, Some(dir)) => {
            let handle = FileHandle { kind: handle_type, bytes: handle };
            let on_a_root = |dev| lock(&shared.roots).may_act_on(uid, dev, uid);
            match by_handle::open(uid, &dir, &handle, on_a_root) {
                Ok(opened) => {
                    *reply = Some(opened);
                    0
                }
                Err(errno) => errno,
            }
        }
        _ => libc::EPERM,
    }
}

fn act(result: io::Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            tracing::warn!("a mark request failed: {e}");
            errno_of(&e)
        }
    }
}

fn errno_of(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

/// Removes a registration **and the marks it put on the tree**.
///
/// Removing the entry alone was worse than doing nothing. Every directory in
/// the tree kept its `FAN_OPEN_PERM | FAN_EVENT_ON_CHILD` mark — confirmed by
/// `/proc/<helper>/fdinfo` being byte-identical across the call — so opens
/// inside it were still intercepted, and `handle_open` then had no daemon to
/// ask, because the uid no longer owns a registered root. A placeholder in an
/// unregistered tree was answered `EIO` in 164 µs. Unregistering a root has to
/// make its tree *uninteresting*, not *unreadable*.
///
/// The marks it put on the tree include the ignore marks on every file that
/// was hydrated there, and those come off too: left behind, one turns a later
/// dehydration that never sent a `ClearIgnore` into a file that reads zeros
/// once the folder is registered again (see `marks::walk_and_unmark`).
///
/// The state file is written before anything is unmarked, and the entry goes
/// back if that write fails: a restart between the two would re-walk the root
/// and put the marks back, which is a correct, recoverable state. The reverse
/// order — unmark, then fail to save — would leave a root that is registered,
/// walked at every startup, and unmarked in between.
fn unregister_root(shared: &Shared, uid: u32, root_id: &str) -> i32 {
    let root = {
        let mut roots = lock(&shared.roots);
        let Some(root) = roots.remove_owned(uid, root_id) else {
            tracing::warn!("uid {uid} tried to unregister root {root_id}, which is not theirs");
            return libc::EPERM;
        };
        if let Err(e) = roots.save(Path::new(ROOTS_FILE)) {
            tracing::error!("cannot save {ROOTS_FILE}: {e}");
            roots.insert(root);
            return errno_of(&e);
        }
        root
    };

    // Outside the roots lock: the walk opens and marks its way through a whole
    // tree, and every other thread that wants to know whether a uid has a root
    // would be waiting behind it.
    //
    // Counted on both sides of the walk (second guard; see
    // `mark_while_hydrated`): whatever a worker or a finishing hydration read
    // before the walk ended, it does not mark behind it.
    shared.unregistrations.bump(root.uid);
    uncover_root(shared, &root);
    shared.unregistrations.bump(root.uid);
    lock(&shared.degraded_roots).remove(&root.root_id);
    0
}

/// Takes the marks off a tree whose registration has just been removed.
///
/// Failures here are logged and not returned: the registration *is* gone, the
/// state file already says so, and answering the daemon `EIO` would only
/// invite it to retry a request that would then be refused `EPERM` for a root
/// it no longer owns. What matters is that the condition is named, because a
/// tree left marked without a registration is one where every placeholder open
/// is denied.
fn uncover_root(shared: &Shared, root: &roots::Root) {
    let dir = match open_root(root) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::error!(
                "root {} ({}) was unregistered, but it could not be re-opened to remove its marks \
                 ({e}); if the directory is still there and still marked, opens inside it are \
                 still intercepted and will be denied EIO",
                root.root_id,
                root.path
            );
            return;
        }
    };
    let report = marks::walk_and_unmark(&shared.marks, dir.as_fd(), &root.path);
    if !report.degraded() {
        tracing::info!(
            "root {} ({}) unregistered; unmarked {} directories",
            root.root_id,
            root.path,
            report.marked
        );
        return;
    }
    tracing::error!(
        "root {} ({}) was unregistered but {} of its marks could not be removed ({} directories \
         were unmarked); opens in a directory that kept its mark are still intercepted and will \
         be denied EIO, and a file that kept its ignore mark must not be dehydrated without a \
         ClearIgnore",
        root.root_id,
        root.path,
        report.failures.len(),
        report.marked
    );
    for failure in &report.failures {
        tracing::error!("  {failure}");
    }
}

/// The directory must be owned by the peer, live on a
/// filesystem that can host placeholders, and neither contain nor sit inside
/// another registered root. Only then is it stored, marked, and walked.
fn register_root(shared: &Shared, owner: Owner, root_id: String, dir: File) -> i32 {
    let uid = owner.uid;
    let meta = match dir.metadata() {
        Ok(meta) => meta,
        Err(e) => return errno_of(&e),
    };
    if !meta.is_dir() || meta.uid() != uid {
        tracing::warn!("uid {uid} offered a root it does not own, or that is not a directory");
        return libc::EPERM;
    }

    // `root_id` is a string the client picks, so it names an
    // entry without owning one: before anything else, the entry it names must
    // be free or already ours. Without this, any local user could replace
    // another user's registration by reusing its id — and the victim's tree
    // would then go unwalked at the next restart, which is the "serve zeros"
    // outcome this whole component exists to prevent.
    let previous_owner = lock(&shared.roots).owner_of(&root_id);
    if previous_owner.is_some_and(|other| other != uid) {
        tracing::warn!(
            "uid {uid} tried to register root id {root_id}, which belongs to another user"
        );
        return libc::EPERM;
    }

    let path = match resolve_root_path(&dir, meta.dev(), meta.ino()) {
        Ok(path) => path,
        Err(errno) => return errno,
    };

    // Re-registering a root we already hold skips the write
    // probe. The directory is already marked from the first registration, so
    // creating the probe's temporary file inside it (`O_TMPFILE`, and so
    // nameless since, but still an `open` in a marked directory)
    // can raise a permission event aimed at this very helper (kernel fact 7
    // — `docs/kernel-behavior-7.2.md` §7) while this thread is blocked
    // inside the probe. A worker answers it
    // today, but only because the pool is a separate thread set, and under a
    // saturated pool the probe is denied `EAGAIN` and a perfectly good
    // re-registration fails. The probe told us nothing new anyway: the
    // filesystem was probed when the root was first registered, and the type
    // check below is re-run either way.
    //
    // closes the last two ways into the same hazard: a directory
    // already registered under *another* id, and one lying inside somebody's
    // registered root, are both already marked, and both are about to be
    // refused `EINVAL` by the nesting check — but the probe ran first and so
    // wrote into a marked directory anyway. Asking the same question here,
    // before the probe, costs one lock and removes it. The authoritative
    // nesting check stays where it is, under the lock that inserts; this one
    // only decides whether to probe.
    //
    // `Contains` is deliberately not in the set: a directory that contains a
    // registered root sits *above* every mark, so probing in it is safe.
    let reregistration = previous_owner == Some(uid);
    let already_marked = reregistration
        || matches!(
            lock(&shared.roots).nesting_conflict(&path, meta.dev(), meta.ino()),
            Some(roots::Nesting::SameDirectory(_)) | Some(roots::Nesting::Inside(_))
        );
    let outcome = if already_marked {
        check_filesystem_type(&dir, &path)
    } else {
        check_filesystem(&dir, &path)
    };
    if let Err(errno) = outcome {
        return errno;
    }

    let root = roots::Root { uid, dev: meta.dev(), ino: meta.ino(), path, root_id };
    {
        let mut roots = lock(&shared.roots);
        // Asked again under the lock that will do the inserting. The check
        // above ran before `resolve_root_path` and the filesystem checks, all
        // of which do I/O the lock must not be held across — and in that gap
        // another connection could have claimed this id. Re-asking here is
        // what makes the refusal airtight rather than merely likely.
        if roots.owner_of(&root.root_id).is_some_and(|other| other != uid) {
            tracing::warn!(
                "uid {uid} tried to register root id {}, which belongs to another user",
                root.root_id
            );
            return libc::EPERM;
        }
        // Our own previous entry is lifted out so the nesting check does not
        // report this root as overlapping itself; every *other* root is now
        // compared, whatever id it carries.
        let displaced = roots.take(&root.root_id);
        if let Some(conflict) = roots.nesting_conflict(&root.path, root.dev, root.ino) {
            tracing::warn!("{} cannot be registered: {conflict:?}", root.path);
            if let Some(previous) = displaced {
                roots.insert(previous);
            }
            return libc::EINVAL;
        }
        roots.insert(root.clone());
        if let Err(e) = roots.save(Path::new(ROOTS_FILE)) {
            tracing::error!("cannot save {ROOTS_FILE}: {e}");
            let _ = roots.remove_owned(uid, &root.root_id);
            if let Some(previous) = displaced {
                roots.insert(previous);
            }
            return errno_of(&e);
        }
    }

    // A newly registered root is walked, not just marked at the top: it can
    // already have a whole tree in it (the daemon registers a folder the user
    // may have been syncing with something else, or one restored from a
    // backup), and every directory in it needs its own mark or nothing inside
    // it is intercepted.
    record_walk(shared, &root, marks::walk_and_mark(&shared.marks, dir.as_fd(), &root.path));
    0
}

/// The absolute path of a directory we were handed as a descriptor, verified
/// to lead back to that same directory.
///
/// `/proc/self/fd/<n>` is the only way to name a descriptor's path, and its
/// answer cannot be trusted as-is: for an unlinked directory it appends
/// `" (deleted)"`, which stored verbatim would be a path that never resolves.
/// So the answer is checked rather than believed — it must be absolute, it
/// must be representable (a path the helper cannot write into its JSON state
/// is refused at registration rather than mangled into something else), and
/// re-opening it must land on the very same `(dev, ino)`.
fn resolve_root_path(dir: &File, dev: u64, ino: u64) -> Result<String, i32> {
    let link = format!("/proc/self/fd/{}", dir.as_raw_fd());
    let target = match std::fs::read_link(&link) {
        Ok(target) => target,
        Err(e) => {
            tracing::error!("cannot resolve the path of the offered root: {e}");
            return Err(libc::EINVAL);
        }
    };
    let Some(path) = target.to_str() else {
        tracing::error!(
            "the offered root has a path that is not valid UTF-8 ({}); refusing to register it",
            target.display()
        );
        return Err(libc::EINVAL);
    };
    if !path.starts_with('/') {
        tracing::error!("the offered root resolved to {path:?}, which is not an absolute path");
        return Err(libc::EINVAL);
    }

    // The same revalidation the startup walk does. A `" (deleted)"` suffix, a
    // path that goes through a symlink somebody can swap, a directory renamed
    // out from under us — all of them come out here as a mismatch, so the
    // stored path is one that has been shown to lead back to this directory
    // rather than one that was merely reported.
    if let Err(e) = reopen_and_verify(path, dev, ino) {
        tracing::error!("{path} does not lead back to the directory that was offered: {e}");
        return Err(libc::EINVAL);
    }
    Ok(path.to_owned())
}

/// Two checks, because they fail for different reasons and only one
/// of them works inside the helper's sandbox.
///
/// The filesystem type comes from `fstatfs` on the descriptor itself and is
/// authoritative: a network filesystem is refused outright, because a file
/// that can change on another machine cannot be intercepted here at all.
///
/// The feature probe writes a temporary file, and the helper's unit runs with
/// `ProtectHome=read-only`, so it can legitimately be refused permission to
/// write into a directory that is otherwise perfectly good. A genuine "this
/// filesystem does not implement hole punching / user xattrs" answer is fatal;
/// being refused the write is logged and the probe skipped — the daemon runs
/// its own probe in the user's own context (§10), and refusing every
/// registration because of our own sandbox would be worse than the check is
/// worth.
fn check_filesystem(dir: &File, path: &str) -> Result<(), i32> {
    check_filesystem_type(dir, path)?;

    // Probed through `/proc/self/fd/<n>` rather than by path, so the probe
    // lands in the exact directory we were handed and cannot be redirected by
    // swapping a component of the path.
    let through_fd = format!("/proc/self/fd/{}", dir.as_raw_fd());
    match probe_dir(Path::new(&through_fd)) {
        Ok(()) => Ok(()),
        Err(ProbeError::Missing { feature, .. }) => {
            tracing::error!("{path}: the filesystem does not support {feature}");
            Err(libc::EOPNOTSUPP)
        }
        Err(ProbeError::Unusable {
            why,
            errno: Some(libc::EROFS) | Some(libc::EACCES) | Some(libc::EPERM),
            ..
        }) => {
            tracing::warn!(
                "{path}: the helper's own sandbox stopped the feature probe ({why}); relying on \
                 the filesystem type check and the daemon's own probe instead"
            );
            Ok(())
        }
        Err(e) => {
            tracing::error!("{path}: {e}");
            Err(libc::EIO)
        }
    }
}

/// The half of §10's check that writes nothing.
///
/// `fstatfs` on the descriptor itself, so it is authoritative and cannot be
/// defeated by the helper's own sandbox, and it raises no fanotify event —
/// which is why this is the only half that runs at startup and on a
/// re-registration.
fn check_filesystem_type(dir: &File, path: &str) -> Result<(), i32> {
    // SAFETY: `dir` is an open descriptor and `buf` is a live, correctly sized
    // `statfs` that `fstatfs` fills in.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(dir.as_raw_fd(), &mut buf) } != 0 {
        let e = io::Error::last_os_error();
        tracing::error!("cannot identify the filesystem under {path}: {e}");
        return Err(errno_of(&e));
    }
    if let Some((_, name)) =
        REFUSED_FILESYSTEMS.iter().find(|(magic, _)| *magic == buf.f_type as i64)
    {
        tracing::error!(
            "{path} is on {name}, which konedrive cannot host placeholders on: its files can \
             change without any open on this machine, so interception would miss them"
        );
        return Err(libc::EOPNOTSUPP);
    }
    Ok(())
}

/// Filesystems a sync root may never live on. A denylist rather
/// than an allowlist: the hard requirements are sparse files with hole
/// punching and `user.*` xattrs, and `probe_dir` measures those directly on
/// whatever filesystem is actually there, so the type check only has to catch
/// the cases a probe cannot — a filesystem whose files can change behind our
/// back, which no local test can detect.
/// Magic numbers as `include/uapi/linux/magic.h` defines them.
const REFUSED_FILESYSTEMS: &[(i64, &str)] = &[
    (0x6969, "NFS"),
    (0xff53_4d42, "SMB/CIFS"),
    (0xfe53_4d42, "SMB2"),
    (0x6573_5546, "FUSE"),
    (0x4d44, "FAT"),
    (0x2011_bab0, "exFAT"),
    (0x5346_414f, "AFS"),
    (0x6b41_4653, "kAFS"),
    (0x00c3_6400, "Ceph"),
];

/// What brought us into [`finish`]. It changes nothing about what the function
/// does and everything about what "there is no such job" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Finish {
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
fn settle(
    shared: &Shared,
    req_id: u64,
    owner: Owner,
    errno: i32,
    why: Finish,
) -> Option<jobs::Dispatch> {
    let Some(jobs::Finished { waiters, since, next }) = lock(&shared.jobs).finish(req_id, owner)
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
                "request {req_id} for uid {} connection {} could not be sent to the daemon, and \
                 by the time it was drained it was already gone; its openers were answered \
                 elsewhere",
                owner.uid,
                owner.conn
            ),
        }
        return None;
    };
    answer(shared, req_id, waiters, errno, since);
    next
}

/// Answers the openers of one hydration with its outcome. `since` is the
/// count of root unregistrations when the open that started it was read.
fn answer(shared: &Shared, req_id: u64, waiters: Vec<OwnedFd>, errno: i32, since: u64) {
    if errno != 0 {
        let delivered = marks::clamp_deny_errno(errno);
        if delivered != errno {
            tracing::warn!(
                "the daemon reported errno {errno} for request {req_id}, which the kernel will \
                 not deliver; denying with {delivered} instead"
            );
        }
        for fd in waiters {
            respond_deny(shared, fd, delivered);
        }
        return;
    }

    // The ignore mark goes on only after the file's state has
    // been read again, from the event fd itself — the exact inode the opener
    // is about to get, with no path in between and so nothing to race. A
    // hydration that reports success but does not leave the file `hydrated`
    // has not put the content there as far as we can tell, and §5.2 is
    // unconditional about what happens then.
    //
    // The mark then goes through `mark_while_hydrated`, like every other
    // mark: a dehydration that began after the read above
    // takes it off again here, and a hydration that began before a root was
    // unregistered leaves no mark behind it.
    let Some(first) = waiters.first() else { return };
    let verdict = state_of(first.as_fd());
    let verdict = match verdict {
        Ok(Some(State::Hydrated)) => {
            let file = file_of(first.as_fd());
            mark_while_hydrated(shared, first.as_fd(), file, since).map_err(|now| {
                tracing::error!(
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
            for fd in waiters {
                respond_allow(shared, fd);
            }
        }
        Err(other) => {
            tracing::error!(
                "request {req_id} was reported successful but the file does not read as \
                 hydrated ({other:?}); denying EIO rather than risk serving zeros"
            );
            for fd in waiters {
                respond_deny(shared, fd, libc::EIO);
            }
        }
    }
}

/// Reads the state of the inode behind an event fd, through a duplicate so
/// that nothing here can close the descriptor that still owes a response.
/// The duplicate is closed before this returns (see `handle_open` on m1).
fn state_of(fd: BorrowedFd<'_>) -> Result<Option<State>, StateError> {
    let probe = File::from(fd.try_clone_to_owned()?);
    read_state(&probe)
}

/// [`state_of`] for the item id.
fn item_id_of(fd: BorrowedFd<'_>) -> io::Result<Option<String>> {
    read_item_id(&File::from(fd.try_clone_to_owned()?))
}

/// [`state_of`] for `fstat`.
fn metadata_of(fd: BorrowedFd<'_>) -> io::Result<std::fs::Metadata> {
    File::from(fd.try_clone_to_owned()?).metadata()
}

/// Who owns the file behind a descriptor, and which inode it is.
#[derive(Debug, Clone, Copy)]
struct FileId {
    /// `None` when it could not be read: then every unregistration since
    /// counts against the file.
    owner: Option<u32>,
    dev: u64,
    ino: u64,
}

/// [`FileId`] behind an event fd. The inode is for log lines only, and is
/// zeros if it cannot be read.
fn file_of(fd: BorrowedFd<'_>) -> FileId {
    match metadata_of(fd) {
        Ok(meta) => FileId { owner: Some(meta.uid()), dev: meta.dev(), ino: meta.ino() },
        Err(_) => FileId { owner: None, dev: 0, ino: 0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registration for connection `conn` of `uid`, backed by a real
    /// outbox on a socket pair nobody reads — the registry only ever
    /// looks at `uid` and `conn`.
    fn daemon(uid: u32, conn: u64) -> (Daemon, UnixStream) {
        daemon_of_pid(uid, conn, 1)
    }

    fn daemon_of_pid(uid: u32, conn: u64, pid: i32) -> (Daemon, UnixStream) {
        use nix::sys::socket::socketpair;
        let (ours, theirs) =
            socketpair(AddressFamily::Unix, SockType::SeqPacket, None, SockFlag::empty()).unwrap();
        let ours = UnixStream::from(ours);
        let outbox = Outbox::start(ours.try_clone().unwrap(), ours, "test").unwrap();
        (Daemon { conn, uid, pid, outbox: Arc::new(outbox) }, UnixStream::from(theirs))
    }

    fn registered(daemons: &Registry, uid: u32) -> Option<u64> {
        daemons.top(uid).map(|d| d.conn)
    }

    /// The interleaving the VM suite hit. Two connections from
    /// one uid: connection 1, accepted first, is a throwaway that connects
    /// and drops; connection 2, accepted second, is the live daemon. Their
    /// threads run in the opposite order, so the live daemon registers first
    /// and the throwaway registers *last* — and then goes away.
    ///
    /// Before the fix the late insert replaced the live daemon, the
    /// throwaway's cleanup then found itself registered and removed it, and
    /// the uid was left with no daemon while its daemon's socket was still
    /// open: every placeholder open waited `DAEMON_WAIT` and was denied.
    #[test]
    fn an_older_connection_that_registers_late_does_not_evict_a_live_daemon() {
        let mut daemons = Registry::default();
        let (live, _live_peer) = daemon(1000, 2);
        let (throwaway, throwaway_peer) = daemon(1000, 1);

        assert!(daemons.register(live), "the first registration always lands");
        let late = daemons.register(throwaway);

        // The throwaway goes away; its `Disconnect` guard runs.
        drop(throwaway_peer);
        daemons.deregister(1000, 1);
        assert_eq!(
            registered(&daemons, 1000),
            Some(2),
            "the live daemon must still be the one registered after the throwaway's cleanup"
        );
        assert!(
            !late,
            "an older connection must not replace a newer one, whatever order they arrive in"
        );
    }

    /// The other half, which must keep working: a newer connection from the
    /// same uid — a daemon that reconnected — does replace the older one, and
    /// the older one's cleanup then leaves it alone.
    #[test]
    fn a_newer_connection_replaces_an_older_one_and_survives_its_cleanup() {
        let mut daemons = Registry::default();
        let (old, _old_peer) = daemon(1000, 1);
        let (new, _new_peer) = daemon(1000, 2);

        assert!(daemons.register(old));
        assert!(daemons.register(new), "a reconnecting daemon must take over");
        daemons.deregister(1000, 1);
        assert_eq!(registered(&daemons, 1000), Some(2));

        daemons.deregister(1000, 2);
        assert_eq!(registered(&daemons, 1000), None, "and its own cleanup removes it");
    }

    /// The sequential case H120 left open. A process of the
    /// daemon's own uid connects after it — newest wins, so it takes over —
    /// and then goes away. The live daemon underneath must get the uid back:
    /// its socket is still open, so nothing will ever make it reconnect, and
    /// a uid left with no registration has every open wait `DAEMON_WAIT` and
    /// then be denied `EIO`, until the daemon restarts.
    #[test]
    fn a_newer_connection_that_goes_away_hands_the_uid_back_to_the_live_one() {
        let mut daemons = Registry::default();
        let (live, _live_peer) = daemon(1000, 1);
        let (transient, _transient_peer) = daemon(1000, 2);

        daemons.register(live);
        daemons.register(transient);
        assert_eq!(registered(&daemons, 1000), Some(2), "the newer connection takes over");

        daemons.deregister(1000, 2);
        assert_eq!(
            registered(&daemons, 1000),
            Some(1),
            "when the newer connection goes, the live daemon underneath must be the one \
             hydrations go to again"
        );
    }

    /// The exemption follows the top. One pid per
    /// uid is exempt at any moment — the one the uid's hydrations go to —
    /// and it passes back down when the connection above it goes. A live
    /// connection underneath is not exempt while it is not the top, and no
    /// connection is ever exempt for another uid.
    #[test]
    fn only_the_top_connections_pid_is_exempt() {
        let mut daemons = Registry::default();
        let (live, _live_peer) = daemon_of_pid(1000, 1, 10);
        let (transient, _transient_peer) = daemon_of_pid(1000, 2, 20);
        daemons.register(live);
        assert!(daemons.is_top_pid(1000, 10), "a lone daemon is exempt for its uid");

        daemons.register(transient);
        assert!(daemons.is_top_pid(1000, 20), "the newer connection is on top");
        assert!(!daemons.is_top_pid(1000, 10), "and the one underneath is no longer exempt");
        assert!(!daemons.is_top_pid(1001, 20), "nor is anybody exempt for another uid");

        daemons.deregister(1000, 2);
        assert!(daemons.is_top_pid(1000, 10), "the exemption goes back down with the top");
        assert!(!daemons.is_top_pid(1000, 20), "and leaves with the connection that left");
    }

    /// Removing a connection from the middle of a stack keeps the order of
    /// the rest, and a uid whose last connection goes holds no entry at all.
    #[test]
    fn a_connection_leaves_the_stack_from_wherever_it_sits() {
        let mut daemons = Registry::default();
        let mut peers = Vec::new();
        for conn in 1..=3 {
            let (d, peer) = daemon(1000, conn);
            peers.push(peer);
            assert!(daemons.register(d), "each newer connection lands on top");
        }
        daemons.deregister(1000, 2);
        assert_eq!(registered(&daemons, 1000), Some(3), "the top is untouched");
        daemons.deregister(1000, 3);
        assert_eq!(registered(&daemons, 1000), Some(1), "and the oldest is under it");
        daemons.deregister(1000, 1);
        assert_eq!(registered(&daemons, 1000), None);
        assert!(daemons.by_uid.is_empty(), "no empty stack is kept");
        daemons.deregister(1000, 1);
        assert!(daemons.by_uid.is_empty(), "and removing it twice is harmless");
    }

    /// Ordering is per uid: another uid's newer connection is not a reason to
    /// refuse this one.
    #[test]
    fn connection_order_is_compared_only_within_one_uid() {
        let mut daemons = Registry::default();
        let (other, _other_peer) = daemon(1001, 5);
        let (ours, _our_peer) = daemon(1000, 3);
        assert!(daemons.register(other));
        assert!(daemons.register(ours));
        assert_eq!(registered(&daemons, 1000), Some(3));
        assert_eq!(registered(&daemons, 1001), Some(5));
    }

    /// However a run of refusals is split into lines, the lines
    /// add up to the refusals: the first is written at once, the rest of its
    /// interval only counted, and the count after the last line is written
    /// when the interval ends — by `flush`, since a burst that has stopped
    /// has no next refusal to carry it. The event loop's throttle, which this
    /// one is modelled on, never wrote that last count.
    #[test]
    fn a_throttle_accounts_for_every_occurrence() {
        let every = Duration::from_millis(50);
        let mut throttle = Throttle::every(every);
        let mut written: Vec<u64> = Vec::new();
        written.extend(throttle.admit());
        assert_eq!(written, [1], "the first occurrence is written at once");
        for _ in 0..99 {
            written.extend(throttle.admit());
        }
        assert_eq!(written, [1], "the rest of its interval is only counted");
        assert_eq!(throttle.flush(), None, "and not written before the interval is over");

        std::thread::sleep(every * 2);
        written.extend(throttle.flush());
        assert_eq!(written, [1, 99], "the tail is written though nothing came after it");
        assert_eq!(throttle.flush(), None, "once");

        written.extend(throttle.admit());
        std::thread::sleep(every * 2);
        written.extend(throttle.flush());
        assert_eq!(written.iter().sum::<u64>(), 101, "every occurrence is in some line");
    }

    /// A condition that clears — descriptors come back, `accept` works again
    /// — hands back the count no line had written, so the recovery line can
    /// say it; the next occurrence is then written at once again.
    #[test]
    fn a_throttle_reset_hands_back_what_it_had_not_written() {
        let mut throttle = Throttle::every(Duration::from_secs(60));
        assert_eq!(throttle.admit(), Some(1));
        assert_eq!(throttle.admit(), None);
        assert_eq!(throttle.admit(), None);
        assert_eq!(throttle.reset(), 2, "the two nobody wrote down");
        assert_eq!(throttle.admit(), Some(1), "and the next one is written at once");
    }

    /// Each kind of refusal has a throttle of its own, found by its index, so
    /// that a flood of one never silences another; and each summary keeps the
    /// words of its per-occurrence line, which is what anyone searching the
    /// journal — the VM suite included — looks for.
    #[test]
    fn every_kind_of_refusal_has_its_own_throttle_and_keeps_its_words() {
        for (index, kind) in Refusal::ALL.iter().enumerate() {
            assert_eq!(*kind as usize, index, "{kind:?} would share another kind's throttle");
        }
        let refusals = Refusals::new();
        assert_eq!(refusals.throttles.len(), Refusal::ALL.len());
        refusals.report(Refusal::PoolFull, || "first".into());
        assert_eq!(
            lock(&refusals.throttles[Refusal::NoRoot as usize]).admit(),
            Some(1),
            "another kind's first refusal is still written at once"
        );
        for (kind, words) in [
            (Refusal::PoolFull, "workers busy and"),
            (Refusal::NoRoot, "no registered root"),
            (Refusal::TooManyWaiters, "waiter backstop"),
            (Refusal::TimedOut, "did not connect within"),
            (Refusal::Undeliverable, "could not be queued"),
            (Refusal::StrayDone, "ignoring HydrateDone"),
            (Refusal::TooManyConnections, "already holding"),
            (Refusal::Unopenable, "could not hand over"),
            (Refusal::EventFdFailed, "could not open the descriptor"),
        ] {
            assert!(kind.summary().contains(words), "{kind:?}'s summary lost {words:?}");
        }
    }

    /// second guard, kept per uid: an unregistration withholds
    /// ignore marks only from its own user's files, so nobody can keep other
    /// users' files unmarked by unregistering roots of their own in a loop.
    #[test]
    fn an_unregistration_counts_against_its_own_users_files_only() {
        let unregistrations = Unregistrations::new();
        let read = unregistrations.now();
        assert!(!unregistrations.since(read, Some(1000)), "nothing has happened yet");

        unregistrations.bump(1000);
        assert!(unregistrations.since(read, Some(1000)), "the walk began after the read");
        assert!(!unregistrations.since(read, Some(1001)), "and it was not user 1001's root");
        assert!(unregistrations.since(read, None), "a file whose owner is unknown counts it");
        let later = unregistrations.now();
        unregistrations.bump(1000);
        assert!(unregistrations.since(later, Some(1000)), "the walk's end counts too");
        assert!(!unregistrations.since(unregistrations.now(), Some(1000)), "read after both");
    }

    /// Past what is remembered, nobody can say whose an unregistration was,
    /// so it counts against everyone.
    #[test]
    fn an_unregistration_that_is_no_longer_remembered_counts_against_everyone() {
        let unregistrations = Unregistrations::new();
        let read = unregistrations.now();
        for _ in 0..=UNREGISTRATIONS_REMEMBERED {
            unregistrations.bump(1000);
        }
        assert!(unregistrations.since(read, Some(1001)));
    }

    /// one uid's connections are bounded, another
    /// uid's are not affected, and a place comes back when its connection
    /// goes.
    #[test]
    fn one_uid_holds_at_most_its_connections_and_no_more() {
        let counters = Arc::new(Mutex::new(HashMap::new()));
        let held: Vec<ConnectionSlot> = (0..MAX_CONNECTIONS_PER_UID)
            .map(|_| ConnectionSlot::take(&counters, 1001).expect("under the bound"))
            .collect();
        assert!(ConnectionSlot::take(&counters, 1001).is_none(), "the bound refuses the next");
        let other = ConnectionSlot::take(&counters, 1000);
        assert!(other.is_some(), "another uid is not affected");
        drop(held);
        assert!(ConnectionSlot::take(&counters, 1001).is_some(), "and places come back");
        drop(other);
    }

    fn waiting_for(counters: &Mutex<HashMap<u32, usize>>, uid: u32) -> usize {
        lock(counters).get(&uid).copied().unwrap_or(0)
    }

    /// `wait_for_daemon` is the only place a worker sleeps for
    /// tens of seconds, so it is the only lever an unprivileged caller has on
    /// the pool. The cap is what makes "open other people's placeholders
    /// while their daemon is down" cost at most `MAX_DAEMON_WAITERS` workers
    /// instead of every one of them.
    #[test]
    fn at_most_eight_opens_wait_for_a_daemon_at_once() {
        let counters = Mutex::new(HashMap::new());
        let held: Vec<WaiterSlot<'_>> = (0..MAX_DAEMON_WAITERS)
            .map(|_| WaiterSlot::take(&counters, 1000).expect("under the cap"))
            .collect();
        assert_eq!(waiting_for(&counters, 1000), MAX_DAEMON_WAITERS);
        assert!(WaiterSlot::take(&counters, 1000).is_none(), "the cap must refuse the next one");

        drop(held);
        assert_eq!(waiting_for(&counters, 1000), 0, "every slot is released");
        assert!(lock(&counters).is_empty(), "and a uid with nobody waiting holds no entry");
        assert!(WaiterSlot::take(&counters, 1000).is_some(), "and the next open may wait again");
    }

    /// The cap is one budget **per uid**, not one for the machine.
    ///
    /// As a single counter it was itself the denial of service it was meant to
    /// prevent: a local user could hold all eight slots by opening another
    /// user's placeholders, and a third user's legitimate early-boot open —
    /// one whose own daemon was seconds from connecting — was then refused
    /// `EIO` without waiting at all.
    #[test]
    fn one_uid_filling_its_slots_does_not_stop_another_waiting() {
        let counters = Mutex::new(HashMap::new());
        let hogged: Vec<WaiterSlot<'_>> = (0..MAX_DAEMON_WAITERS)
            .map(|_| WaiterSlot::take(&counters, 1000).expect("under the cap"))
            .collect();
        assert!(WaiterSlot::take(&counters, 1000).is_none(), "that uid has spent its budget");

        let victim = WaiterSlot::take(&counters, 1001);
        assert!(victim.is_some(), "another uid's open must still be allowed to wait");
        assert_eq!(waiting_for(&counters, 1001), 1);
        assert_eq!(waiting_for(&counters, 1000), MAX_DAEMON_WAITERS, "and budgets do not mix");

        drop(hogged);
        assert_eq!(waiting_for(&counters, 1000), 0);
        assert_eq!(waiting_for(&counters, 1001), 1, "releasing one uid's slots frees only its own");
    }

    /// The follow-up to: the per-uid cap alone restored fairness
    /// between uids at the cost of the flat pool bound the machine used to
    /// have — the worst case became `MAX_DAEMON_WAITERS` times the number of
    /// uids with a registered root whose daemon is down, which is unbounded
    /// on a machine with enough such uids. Five uids spending their whole
    /// budget (`5 * MAX_DAEMON_WAITERS == 40`) comfortably exceeds
    /// `GLOBAL_MAX_DAEMON_WAITERS` (32) while every one of them stays inside
    /// its own per-uid cap the whole time, so nothing here depends on the
    /// per-uid cap ever tripping.
    #[test]
    fn a_global_backstop_binds_even_though_every_uid_stays_under_its_own_cap() {
        let counters = Mutex::new(HashMap::new());
        let mut held = Vec::new();
        for uid in 1000..1005 {
            for _ in 0..MAX_DAEMON_WAITERS {
                if let Some(slot) = WaiterSlot::take(&counters, uid) {
                    held.push(slot);
                }
            }
        }
        assert_eq!(
            held.len(),
            GLOBAL_MAX_DAEMON_WAITERS,
            "the machine-wide backstop must bind before every uid reaches its own cap \
             (5 uids * {MAX_DAEMON_WAITERS} each = 40, which must not all be granted)"
        );

        // The discriminator: a uid that has spent none of its own budget is
        // still refused once the backstop is full. A per-uid-only cap would
        // grant this.
        assert!(
            WaiterSlot::take(&counters, 1005).is_none(),
            "a uid with an entirely unspent budget must still be refused once the \
             machine-wide backstop is full"
        );

        drop(held);
        assert!(lock(&counters).is_empty(), "every slot is released");
        assert!(WaiterSlot::take(&counters, 1005).is_some(), "and the backstop frees up again");
    }

    /// A slot is released however its holder leaves, including by panicking —
    /// otherwise one panicking worker would permanently shrink the number of
    /// opens that may ever wait.
    #[test]
    fn a_waiter_slot_is_released_even_if_its_holder_panics() {
        let counters = Mutex::new(HashMap::new());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _slot = WaiterSlot::take(&counters, 1000).expect("under the cap");
            panic!("as a worker might");
        }));
        assert!(outcome.is_err());
        assert_eq!(waiting_for(&counters, 1000), 0);
    }

    /// And the governing property of this whole component: an
    /// application must never read zeros where real content should be.
    ///
    /// `fanotify(7)` allows every outstanding permission event when the
    /// group's descriptor closes, so the helper exiting is **silent data
    /// loss**, while the helper denying is an errno the application sees. That
    /// asymmetry is what makes running out of descriptors — a resource
    /// problem, and a self-correcting one, since the descriptors are held by
    /// hydrations that are all going to finish — something to survive rather
    /// than something to die of.
    #[test]
    fn running_out_of_descriptors_does_not_end_the_process() {
        assert_eq!(
            classify_read_failure(Errno::EMFILE),
            ReadFailure::Exhausted,
            "this process being out of descriptors must not close the fanotify group"
        );
        assert_eq!(
            classify_read_failure(Errno::ENFILE),
            ReadFailure::Exhausted,
            "nor must the machine being out of them"
        );
    }

    /// The other arms, so that "survivable" did not quietly become
    /// "everything is survivable": an errno the group fd itself reports still
    /// ends the loop, because a group that cannot be read cannot be answered.
    #[test]
    fn the_event_loops_other_read_failures_keep_their_meaning() {
        assert_eq!(classify_read_failure(Errno::EAGAIN), ReadFailure::Drained);
        assert_eq!(classify_read_failure(Errno::EINTR), ReadFailure::Interrupted);
        for errno in [Errno::EBADF, Errno::EINVAL, Errno::EFAULT] {
            assert_eq!(classify_read_failure(errno), ReadFailure::Fatal, "{errno} is not handled");
        }
    }

    /// The kernel opens each event's
    /// descriptor with the group's `O_RDWR` against the **opener's** mount,
    /// and when that open fails `read()` of the group returns its errno for
    /// that one event — which the kernel has already denied. Measured:
    /// `EROFS` for an open through a read-only mount (a Flatpak app with
    /// `home:ro`), `ETXTBSY` for a second open of a running executable. As
    /// `Fatal`, either one ended the helper, and every suspended open was
    /// then allowed onto its unfilled placeholder: 65 536 zero bytes. Any
    /// errno but the three the group descriptor itself can report is one
    /// event's, and the loop goes on reading.
    #[test]
    fn an_event_the_kernel_could_not_hand_over_does_not_end_the_helper() {
        for errno in [
            Errno::EROFS,
            Errno::ETXTBSY,
            Errno::EACCES,
            Errno::EPERM,
            Errno::EIO,
            Errno::ENOMEM,
            Errno::ENXIO,
            Errno::ENODEV,
            Errno::EOVERFLOW,
            Errno::ESTALE,
        ] {
            assert_ne!(
                classify_read_failure(errno),
                ReadFailure::Fatal,
                "{errno}: one event the kernel could not hand over must not close the group — \
                 closing it allows every suspended open onto its placeholder"
            );
        }
    }

    /// As `register_root` asks it: the id names an entry but does
    /// not own one, so an entry already held by somebody else is refused and
    /// a user's own is a re-registration.
    #[test]
    fn a_root_id_held_by_another_user_is_refused() {
        let mut roots = roots::Roots::default();
        roots.insert(roots::Root {
            uid: 1000,
            dev: 42,
            ino: 7,
            path: "/home/alice/OneDrive".into(),
            root_id: "shared-id".into(),
        });
        let refused = |uid: u32| roots.owner_of("shared-id").is_some_and(|other| other != uid);
        assert!(refused(1001), "another user must not take over the id");
        assert!(!refused(1000), "the owner re-registering its own root must not be refused");
        assert!(
            !roots.owner_of("unused-id").is_some_and(|other| other != 1001),
            "an unused id is free for anyone"
        );
    }
}
