//! What the helper's threads share, by subject: the roots, the hydrations
//! in hand, the connected daemons, the connections' places, and the log of
//! refusals. Each subject keeps its own lock behind its methods; no method
//! here takes one subject's lock while holding another's.

mod daemons;
mod refusals;
mod registrations;
mod slots;

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use konedrive_helper::jobs::{self, Owner};
use konedrive_helper::marks;
use konedrive_helper::pending::PendingOpen;
use konedrive_helper::roots;

pub(crate) use daemons::{
    Daemon, Daemons, NoDaemon, DAEMON_WAIT, GLOBAL_MAX_DAEMON_WAITERS, MAX_CONNECTIONS_PER_UID,
    MAX_DAEMON_WAITERS,
};
pub(crate) use refusals::{
    Refusal, Refusals, Throttle, EVENT_FD_FAILED, FLUSH_EVERY, UNOPENABLE,
};
pub(crate) use registrations::{Registrations, Unregistrations, ROOTS_FILE};
pub(crate) use slots::UidSlots;

/// The target of every line this module logs, whichever of its files writes
/// it.
const LOG: &str = module_path!();

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

/// The bound on threads answering permission events, and on how many opens may
/// be waiting for one.
///
/// Both numbers are **provisional**: they were chosen to be obviously enough
/// for interactive use and obviously bounded, not measured. The VM suite's
/// burst scenario (several thousand concurrent opens) is what should settle them —
/// it measures thread count, memory and whether any opener is lost, which is
/// exactly the evidence these two constants need and which no unit test on the
/// host can produce.
pub(crate) const EVENT_WORKERS: usize = 64;
pub(crate) const EVENT_QUEUE_DEPTH: usize = 1024;

/// How long to wait before reading the fanotify group again after the process
/// ran out of file descriptors. Long enough not to spin a core, short enough
/// that interception resumes the moment descriptors come back.
pub(crate) const EXHAUSTION_BACKOFF: Duration = Duration::from_millis(50);

/// The same, for a failing `accept`. `EMFILE` there used to be a 100% CPU
/// spin, because the loop logged and retried immediately.
pub(crate) const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);

/// How long the thread that accepts connections waits before it accepts
/// again after a panic. Long enough that a panic that repeats does not flood
/// the journal; a daemon that connects meanwhile waits in the listener's
/// backlog.
pub(crate) const ACCEPT_RESTART: Duration = Duration::from_secs(1);

/// Deliberate panics, for the unwind paths nothing input-reachable can
/// exercise any more, and one deliberate stall, for a race window too narrow
/// to hit by chance — compiled in **only** with the `fault-injection` cargo
/// feature.
///
/// What they prove is what happens *after* a panic: a worker that panicked
/// answers its opener `EIO` and the pool stays at strength, and a connection
/// whose request loop panicked still runs its `Disconnect` guard, so its
/// suspended openers are denied instead of being left in the kernel forever.
/// Both are only true if something unwinds, and — by design — nothing in the
/// helper panics on any input any more. The VM scenario suite
/// (`tests/vm/scenarios/faults.rs`) therefore restarts the helper with one of these
/// armed and then asserts on what the *opener* got, which is the only
/// evidence either claim can have.
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

    /// Whether the file the variable `armed_by` names is there, which arms
    /// a fault for one occurrence: the file is removed as it is found.
    fn triggered(armed_by: &str) -> bool {
        std::env::var_os(armed_by).is_some_and(|path| std::fs::remove_file(path).is_ok())
    }

    /// Panic in the event loop, with an intercepted open in hand, once for
    /// each time the file the variable names is created.
    pub fn panic_in_event_loop() {
        if triggered("KONEDRIVE_FAULT_PANIC_IN_EVENT_LOOP") {
            panic!("KONEDRIVE_FAULT_PANIC_IN_EVENT_LOOP: deliberate panic in the event loop");
        }
    }

    /// Panic on the thread that accepts connections, with a connection just
    /// accepted, once for each time the file the variable names is created.
    pub fn panic_on_accept() {
        if triggered("KONEDRIVE_FAULT_PANIC_ON_ACCEPT") {
            panic!("KONEDRIVE_FAULT_PANIC_ON_ACCEPT: deliberate panic on the accept thread");
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
    pub fn panic_in_event_loop() {}

    #[inline(always)]
    pub fn panic_on_accept() {}

    #[inline(always)]
    pub fn delay_before_ignore_mark() {}
}

/// The hydrations in hand: every open suspended for a daemon's answer.
///
/// The event fds that owe a response are owned by the jobs in here, not by
/// the lock (see [`lock`]).
pub(crate) struct Hydrations(Mutex<jobs::Jobs<PendingOpen>>);

impl Hydrations {
    fn new() -> Self {
        Self(Mutex::new(jobs::Jobs::default()))
    }

    /// Joins `open` to the hydration of `inode`, or starts one
    /// (`Jobs::enroll`).
    pub(crate) fn enroll(
        &self,
        inode: (u64, u64),
        owner: Owner,
        open: PendingOpen,
        since: u64,
    ) -> jobs::Enrollment<PendingOpen> {
        lock(&self.0).enroll(inode, owner, open, since)
    }

    /// Takes a finished hydration out, if it is `owner`'s (`Jobs::finish`).
    pub(crate) fn finish(&self, req_id: u64, owner: Owner) -> Option<jobs::Finished<PendingOpen>> {
        lock(&self.0).finish(req_id, owner)
    }

    /// Marks connection `conn` dead and drains what it had in hand
    /// (`Jobs::retire`): the openers of each of its hydrations, and how many
    /// hydrations of other connections are still in flight.
    pub(crate) fn retire(&self, conn: u64) -> (Vec<Vec<PendingOpen>>, usize) {
        let mut jobs = lock(&self.0);
        let stranded = jobs.retire(conn);
        (stranded, jobs.in_flight())
    }
}

pub(crate) struct Shared {
    /// Shared with every open that still owes an answer (`PendingOpen`),
    /// which is answered through the group.
    pub(crate) marks: Arc<marks::Marks>,
    pub(crate) roots: Registrations,
    /// Every root unregistration's walk, as it begins and as it ends: what
    /// `mark_while_hydrated` looks at to leave no ignore mark behind a walk
    /// that has already passed (second guard).
    pub(crate) unregistrations: Unregistrations,
    pub(crate) jobs: Hydrations,
    /// Every live connection, by uid, and the opens waiting for one.
    pub(crate) daemons: Daemons,
    /// Live connections per uid; see [`MAX_CONNECTIONS_PER_UID`].
    pub(crate) connections: Arc<UidSlots>,
    /// The throttled log of refused opens.
    pub(crate) refusals: Refusals,
}

impl Shared {
    pub(crate) fn new(marks: marks::Marks, roots: roots::Roots) -> Self {
        Self {
            marks: Arc::new(marks),
            roots: Registrations::new(roots),
            unregistrations: Unregistrations::new(),
            jobs: Hydrations::new(),
            daemons: Daemons::new(),
            connections: daemons::connection_slots(),
            refusals: Refusals::new(),
        }
    }
}
