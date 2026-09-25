//! The watcher (`docs/design/writes.md` §3, amended below): the daemon's own,
//! unprivileged fanotify notification group over a read-write folder, which
//! turns local changes into batches for the examination (`sync::local`).
//!
//! - **The group** (`fan`): `FAN_CLASS_NOTIF | FAN_REPORT_DFID_NAME_TARGET`,
//!   an inode mark on every directory (`FAN_CREATE | FAN_DELETE |
//!   FAN_RENAME | FAN_CLOSE_WRITE | FAN_ATTRIB`, on children and on
//!   directories), the root also `FAN_DELETE_SELF | FAN_MOVE_SELF`. A group
//!   per filesystem id (§3.6); but a directory on another device than the
//!   folder's (a nested Btrfs subvolume, a mount) is not watched at all:
//!   nothing in it is uploaded (the examination's `other-device`), and the
//!   helper cannot mark it.
//! - **The directory map** (`map`): handle → parent and name for every
//!   directory, built by the bring-up walk with `name_to_handle_at` and kept
//!   current from the directory events. It is how an event's directory
//!   handle becomes a path beneath the root, since only the helper could open
//!   a handle.
//! - **The reader** (`reader`): a thread that walks the folder once (every
//!   directory `MarkDir`ed through the helper, then marked), then drains the
//!   groups, settles directory events against the disk (new directories:
//!   `MarkDir`, this group's mark, a scan), drops the daemon's own events by
//!   pid, and gathers what the rest made dirty (`dirt`), by directory handle
//!   and object handle. A `MarkDir` the helper did not answer is asked again.
//! - **Batches**: handed over [`QUIET`] after the last event, at the latest
//!   [`CEILING`] after the first, or at once on [`Watcher::flush`]. An
//!   overflow is a Full local scan and a walk of the map. The bring-up hands
//!   over a Full local scan.
//! - **The examiner**: a second thread that hands what was handed over to a
//!   [`Sink`] (the daemon's is [`ExamineSink`], the examination), merged,
//!   feeds back what it asks to see again after [`RECHECK`], retries a batch
//!   it could not take yet (no completed listing, an error), and runs a Full
//!   local scan every [`DEGRADED_SCAN`] while part of the folder cannot be
//!   watched (the mark budget, the group cap, a filesystem id with no group).
//!   The reader walks the folder on the same beat then, so a directory made
//!   where no event is raised still gets its `MarkDir`.
//! - **The root's own events** stop the watcher and say so in
//!   [`WatchStatus::root_gone`]; nothing is deleted in the cloud because the
//!   folder went.
//!
//! [`Watcher::start`] returns at once: the walk runs on the reader's thread,
//! and [`Watcher::walked`] says when every directory is marked.
//! [`Watcher::stop`] waits for both threads, the examination under way
//! included.

mod dirt;
mod fan;
mod map;
mod reader;
mod service;
#[cfg(test)]
mod tests;

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub use reader::UNKNOWN_WALK;
pub use service::ExamineSink;

pub use crate::sync::local::{CEILING, QUIET, RECHECK};
use crate::sync::listing::LinkCell;
use crate::sync::local::Batch;
use crate::sync::root::SyncRoot;

/// While part of the folder cannot be watched, a Full local scan and a walk
/// this often find what its events would have shown (provisional, §3.3).
pub const DEGRADED_SCAN: Duration = Duration::from_secs(600);
/// A batch the examination could not take yet (no completed listing) is
/// offered again after this long; one it failed on, after this long doubled
/// at each failure in a row, up to [`DEGRADED_SCAN`].
pub const RETRY: Duration = Duration::from_secs(5);
/// A `MarkDir` the helper did not answer is asked again after this long.
pub const MARK_RETRY: Duration = Duration::from_secs(60);

/// The watcher's clocks. The defaults are the design's; tests shorten them.
#[derive(Debug, Clone)]
pub struct Timing {
    pub quiet: Duration,
    pub ceiling: Duration,
    pub recheck: Duration,
    pub retry: Duration,
    pub degraded_scan: Duration,
    pub mark_retry: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self { quiet: QUIET, ceiling: CEILING, recheck: RECHECK, retry: RETRY, degraded_scan: DEGRADED_SCAN, mark_retry: MARK_RETRY }
    }
}

/// What a batch is handed to.
pub trait Sink: Send {
    fn handle(&mut self, batch: &Batch) -> Handled;
}

/// What became of a batch.
#[derive(Debug)]
pub enum Handled {
    /// Examined. `recheck` is handed back after [`Timing::recheck`].
    Done { recheck: Batch },
    /// Not examined yet (no listing has completed): offered again after
    /// [`Timing::retry`], with whatever came meanwhile.
    NotYet,
    /// Not examined: offered again later, backing off.
    Failed(String),
    /// The folder was moved or deleted: nothing more is examined.
    RootGone,
}

/// What the watcher is doing, for `LastError` and for tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchStatus {
    /// Directories in the map, the root included.
    pub directories: usize,
    /// Of those, the ones no notification mark could be placed on.
    pub unwatched: usize,
    /// Notification groups: one per filesystem id.
    pub groups: usize,
    /// Directories the helper did not mark when asked: asked again.
    pub uncovered: usize,
    /// Directories on another device than the folder's (a nested subvolume,
    /// a mount): nothing in them is uploaded, and so nothing from OneDrive is
    /// placed there, which the helper could not protect.
    pub other_device: usize,
    /// The watcher ended with nobody asking it to (a bug): nothing more is
    /// looked for until the folder's sync starts again.
    pub stopped: bool,
    /// Why part of the folder is found only by the periodic scan.
    pub degraded: Option<String>,
    /// The folder was moved or deleted; the watcher has stopped.
    pub root_gone: bool,
    pub overflows: u64,
    /// Batches handed to the examiner.
    pub handed_over: u64,
    /// Batches the sink examined.
    pub examined: u64,
}

impl WatchStatus {
    /// What `LastError` says about the watcher, if anything.
    pub fn note(&self) -> Option<String> {
        if self.root_gone {
            return Some(
                "the OneDrive folder was moved or deleted: local changes are no longer uploaded, and nothing is \
                 removed from OneDrive because of it"
                    .into(),
            );
        }
        let mut parts = Vec::new();
        if self.stopped {
            parts.push("local changes are no longer looked for: the watcher stopped (see the log)".to_owned());
        }
        if self.other_device > 0 {
            parts.push(format!(
                "{} folder(s) are on another device than the OneDrive folder (a nested subvolume, or a mount): nothing \
                 in them is uploaded",
                self.other_device
            ));
        }
        if let Some(why) = &self.degraded {
            parts.push(format!(
                "not every local change is noticed at once ({why}); the folder is scanned every {} minutes instead",
                DEGRADED_SCAN.as_secs() / 60
            ));
        }
        if self.uncovered > 0 {
            parts.push(format!(
                "{} new folder(s) are not yet protected by the konedrive helper, so a file moved into one reads empty \
                 until it is; asking again",
                self.uncovered
            ));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// Called from the watcher's threads whenever [`WatchStatus::note`] may have
/// changed: degraded, directories not marked for interception, or the root
/// gone.
pub type StatusHook = Arc<dyn Fn(&WatchStatus) + Send + Sync>;

/// How far the bring-up walk got ([`Watcher::walked`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkState {
    Walking,
    /// Every directory is marked, for interception and for events.
    Done,
    /// The walk did not finish (a stop, the root gone, a failure): the lock
    /// stays on a folder turning read-write.
    Cut,
}

pub struct WatchConfig {
    /// The folder, proved by its root id when the watcher starts.
    pub root: SyncRoot,
    /// The helper link, for `MarkDir`. Read at each one.
    pub link: LinkCell,
    /// Where `MarkDir` runs (the helper link is async).
    pub runtime: tokio::runtime::Handle,
    /// Events with this pid are the daemon's own (§3.2). The daemon's pid by
    /// default; `None` drops nothing (tests that change the folder from the
    /// test process itself).
    pub own_pid: Option<i32>,
    pub timing: Timing,
    pub on_status: Option<StatusHook>,
    /// A mark budget below the kernel's, to reach the degraded mode without
    /// root (tests only).
    #[cfg(test)]
    pub mark_limit: Option<usize>,
}

impl WatchConfig {
    pub fn new(root: SyncRoot, link: LinkCell, runtime: tokio::runtime::Handle) -> Self {
        Self {
            root,
            link,
            runtime,
            own_pid: Some(std::process::id() as i32),
            timing: Timing::default(),
            on_status: None,
            #[cfg(test)]
            mark_limit: None,
        }
    }
}

/// State the watcher's threads and its owner share.
pub(crate) struct Shared {
    stop: AtomicBool,
    /// An eventfd the reader polls with its groups: written to wake it.
    wake: OwnedFd,
    status: Mutex<WatchStatus>,
    on_status: Option<StatusHook>,
    /// True once the bring-up walk has marked every directory.
    walked: tokio::sync::watch::Sender<WalkState>,
    /// The reader thread has ended: nothing will answer a flush.
    reader_done: AtomicBool,
    /// Flushes asked for and not yet handed to the examiner.
    flushes: Mutex<Vec<mpsc::Sender<bool>>>,
    helper_back: AtomicBool,
    #[cfg(test)]
    paused: AtomicBool,
    /// The reader has seen `paused` and reads nothing until it is cleared.
    #[cfg(test)]
    idle: AtomicBool,
}

impl Shared {
    fn new(on_status: Option<StatusHook>) -> io::Result<Self> {
        // SAFETY: plain syscall; the descriptor is owned here.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            stop: AtomicBool::new(false),
            // SAFETY: `fd` was just returned and nothing else owns it.
            wake: unsafe { OwnedFd::from_raw_fd(fd) },
            status: Mutex::new(WatchStatus::default()),
            on_status,
            walked: tokio::sync::watch::Sender::new(WalkState::Walking),
            reader_done: AtomicBool::new(false),
            flushes: Mutex::new(Vec::new()),
            helper_back: AtomicBool::new(false),
            #[cfg(test)]
            paused: AtomicBool::new(false),
            #[cfg(test)]
            idle: AtomicBool::new(false),
        })
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake();
    }

    /// Wakes the reader from its `poll`. It takes the wake-up back unless it
    /// is stopping, which then wakes it for good.
    fn wake(&self) {
        let one: u64 = 1;
        // SAFETY: writes 8 bytes from a live u64 to our own eventfd.
        unsafe { libc::write(self.wake.as_raw_fd(), (&one as *const u64).cast(), 8) };
    }

    fn wake_fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    fn take_flushes(&self) -> Vec<mpsc::Sender<bool>> {
        std::mem::take(&mut *self.flushes.lock().unwrap())
    }

    fn take_helper_back(&self) -> bool {
        self.helper_back.swap(false, Ordering::SeqCst)
    }

    /// Whether the reader is to stay off the queue; says it does, too.
    #[cfg(test)]
    fn paused(&self) -> bool {
        let paused = self.paused.load(Ordering::SeqCst);
        self.idle.store(paused, Ordering::SeqCst);
        paused
    }

    fn status(&self) -> WatchStatus {
        self.status.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn degraded(&self) -> bool {
        self.status.lock().unwrap_or_else(|p| p.into_inner()).degraded.is_some()
    }

    fn update(&self, change: impl FnOnce(&mut WatchStatus)) {
        let (before, after) = {
            let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
            let before = status.note();
            change(&mut status);
            (before, status.clone())
        };
        if before != after.note() {
            if let Some(hook) = &self.on_status {
                hook(&after);
            }
        }
    }

    /// Part of the folder is found only by the periodic scan. The first
    /// reason is kept.
    fn degrade(&self, why: String) {
        tracing::warn!("the folder is watched only in part: {why}; it is scanned every {} minutes", DEGRADED_SCAN.as_secs() / 60);
        self.update(|s| {
            s.degraded.get_or_insert(why);
        });
    }

    fn root_gone(&self) {
        self.update(|s| s.root_gone = true);
    }
}

/// To the examiner thread.
pub(crate) enum ToExaminer {
    Batch(Batch),
    Full,
    /// Examine what is pending now, then answer whether all of it was.
    Flush(mpsc::Sender<bool>),
    /// Look at `stopping`.
    Wake,
}

/// What may be asked of a running watcher by whoever does not own it (the
/// switch to read-only's flush, the helper coming back). Asking a stopped
/// watcher does nothing.
#[derive(Clone)]
pub struct WatchHandle {
    shared: Arc<Shared>,
    tx: mpsc::Sender<ToExaminer>,
}

impl WatchHandle {
    /// A Full local scan, as soon as the examiner is free (§4.11: the ignore
    /// list shrank).
    pub fn full_scan(&self) {
        let _ = self.tx.send(ToExaminer::Full);
    }

    /// The helper is back (§3.3): its registration walk marked every
    /// directory there is, so what it did not mark before is asked again,
    /// and a Full local scan finds what changed while it was away.
    pub fn helper_back(&self) {
        self.shared.helper_back.store(true, Ordering::SeqCst);
        self.shared.wake();
        self.full_scan();
    }

    /// Hands over at once what the events so far made dirty, and waits until
    /// the examiner has examined everything handed over, so the outbox holds
    /// every change made until now (the switch to read-only asks it before
    /// `PendingUploads`). `false` when that could not be done within
    /// `within`: the watcher stopped, or the examination could not take it
    /// (no completed listing, an error). Blocks: call it off the async
    /// runtime.
    pub fn flush(&self, within: Duration) -> bool {
        if self.shared.stopping() || self.shared.status().root_gone {
            return false;
        }
        let (ack, answer) = mpsc::channel();
        self.shared.flushes.lock().unwrap().push(ack);
        // A reader that ended meanwhile drops what is left in `flushes`, and
        // so the ack: the wait ends at once (the watcher).
        if self.shared.reader_done.load(Ordering::SeqCst) {
            self.shared.flushes.lock().unwrap().clear();
            return false;
        }
        self.shared.wake();
        answer.recv_timeout(within).unwrap_or(false)
    }

    pub fn status(&self) -> WatchStatus {
        self.shared.status()
    }

    /// Examines `batch` as soon as the examiner is free: places the daemon itself
    /// changed — a conflict copy, a folder made local — whose events it drops by pid.
    pub fn examine(&self, batch: Batch) {
        let _ = self.tx.send(ToExaminer::Batch(batch));
    }
}

/// A running watcher: the reader and the examiner threads.
pub struct Watcher {
    shared: Arc<Shared>,
    tx: mpsc::Sender<ToExaminer>,
    threads: Vec<JoinHandle<()>>,
}

impl Watcher {
    /// Starts watching: the group made and the root marked here, then the
    /// bring-up walk (every directory `MarkDir`ed and marked) and the reading
    /// on the reader's thread, whose first batch is a Full local scan. Does
    /// not wait for the walk ([`walked`](Self::walked) says when it is done),
    /// so it can be called where nothing may block.
    ///
    /// Fails when the folder is not the registered root any more, or its
    /// filesystem gives no file handles. A spent mark budget or group cap is
    /// not a failure: the watcher runs degraded, as its status says.
    pub fn start(config: WatchConfig, sink: Box<dyn Sink>) -> io::Result<Self> {
        let root: File = config
            .root
            .open_registered()?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{} no longer carries its root id", config.root.path.display())))?;
        let shared = Arc::new(Shared::new(config.on_status.clone())?);
        let reader = reader::Reader::new(
            root,
            config.own_pid,
            config.timing.clone(),
            config.link,
            config.runtime,
            Arc::clone(&shared),
            #[cfg(test)]
            config.mark_limit,
        )?;
        let (tx, rx) = mpsc::channel();
        let examiner_shared = Arc::clone(&shared);
        let timing = config.timing;
        let examining = std::thread::Builder::new().name("konedrive-examine".into()).spawn(move || examine(rx, sink, timing, examiner_shared))?;
        let reader_tx = tx.clone();
        let reading = match std::thread::Builder::new().name("konedrive-watch".into()).spawn(move || reader.run(reader_tx)) {
            Ok(thread) => thread,
            Err(e) => {
                // The examiner ends once its senders are gone.
                shared.stop();
                drop(tx);
                let _ = examining.join();
                return Err(e);
            }
        };
        Ok(Self { shared, tx, threads: vec![reading, examining] })
    }

    /// True once the bring-up walk has marked every directory (for
    /// interception and for events): the switch to read-write lifts the lock
    /// only then.
    pub fn walked(&self) -> tokio::sync::watch::Receiver<WalkState> {
        self.shared.walked.subscribe()
    }

    /// What others may ask of this watcher without owning it.
    pub fn handle(&self) -> WatchHandle {
        WatchHandle { shared: Arc::clone(&self.shared), tx: self.tx.clone() }
    }

    /// See [`WatchHandle::full_scan`].
    pub fn full_scan(&self) {
        self.handle().full_scan();
    }

    /// See [`WatchHandle::helper_back`].
    pub fn helper_back(&self) {
        self.handle().helper_back();
    }

    /// See [`WatchHandle::flush`].
    pub fn flush(&self, within: Duration) -> bool {
        self.handle().flush(within)
    }

    pub fn status(&self) -> WatchStatus {
        self.shared.status()
    }

    /// Stops both threads and waits for them: the examination under way
    /// finishes first. What was gathered and not handed over is dropped
    /// ([`flush`](WatchHandle::flush) first to keep it); the next start's
    /// Full local scan finds it. Blocks: call it off the async runtime.
    pub fn stop(mut self) {
        self.shared.stop();
        let _ = self.tx.send(ToExaminer::Wake);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }

    /// Holds the reader off the queue, so that events pile up (tests: a real
    /// overflow without root). Returns once the reader has stopped reading.
    #[cfg(test)]
    fn pause(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::SeqCst);
        self.shared.wake();
        let deadline = Instant::now() + Duration::from_secs(10);
        while paused && !self.shared.idle.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Watcher {
    /// A watcher dropped without [`stop`](Watcher::stop) still ends: its
    /// threads see the stop and finish on their own.
    fn drop(&mut self) {
        self.shared.stop();
        let _ = self.tx.send(ToExaminer::Wake);
    }
}

/// The examiner thread: hands batches to `sink`, one examination at a time,
/// merging whatever queued meanwhile.
fn examine(rx: mpsc::Receiver<ToExaminer>, mut sink: Box<dyn Sink>, timing: Timing, shared: Arc<Shared>) {
    let mut pending = Batch::new();
    let mut acks: Vec<mpsc::Sender<bool>> = Vec::new();
    let mut retry_at: Option<Instant> = None;
    let mut failures: u32 = 0;
    let mut rechecks: Vec<(Instant, Batch)> = Vec::new();
    let mut next_scan: Option<Instant> = None;
    let absorb = |message: ToExaminer, pending: &mut Batch, acks: &mut Vec<mpsc::Sender<bool>>| match message {
        ToExaminer::Batch(batch) => pending.merge(batch),
        ToExaminer::Full => pending.merge(Batch::full()),
        ToExaminer::Flush(ack) => acks.push(ack),
        ToExaminer::Wake => {}
    };
    loop {
        if shared.stopping() {
            return;
        }
        // Everything that queued, as one batch.
        loop {
            match rx.try_recv() {
                Ok(message) => absorb(message, &mut pending, &mut acks),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        let now = Instant::now();
        if shared.degraded() {
            let at = *next_scan.get_or_insert(now + timing.degraded_scan);
            if at <= now {
                pending.merge(Batch::full());
                next_scan = Some(now + timing.degraded_scan);
            }
        }
        let mut waiting = Vec::new();
        for (at, batch) in rechecks.drain(..) {
            if at <= now {
                pending.merge(batch);
            } else {
                waiting.push((at, batch));
            }
        }
        rechecks = waiting;
        let flushing = !acks.is_empty();
        if !pending.is_empty() && (flushing || retry_at.is_none_or(|at| at <= now)) {
            let batch = std::mem::take(&mut pending);
            let examined = match sink.handle(&batch) {
                Handled::Done { recheck } => {
                    retry_at = None;
                    failures = 0;
                    shared.update(|s| s.examined += 1);
                    if !recheck.is_empty() {
                        rechecks.push((Instant::now() + timing.recheck, recheck));
                    }
                    true
                }
                Handled::NotYet => {
                    tracing::debug!("the folder has no completed listing yet; its local changes wait");
                    pending.merge(batch);
                    retry_at = Some(Instant::now() + timing.retry);
                    false
                }
                Handled::Failed(why) => {
                    failures = failures.saturating_add(1);
                    let wait = timing.retry.saturating_mul(1 << failures.min(16).saturating_sub(1)).min(timing.degraded_scan);
                    tracing::warn!("local changes could not be examined: {why}; trying again in {} s", wait.as_secs());
                    pending.merge(batch);
                    retry_at = Some(Instant::now() + wait);
                    false
                }
                Handled::RootGone => {
                    tracing::warn!("the OneDrive folder was moved or deleted; nothing more is examined");
                    shared.root_gone();
                    for ack in acks.drain(..) {
                        let _ = ack.send(false);
                    }
                    return;
                }
            };
            for ack in acks.drain(..) {
                let _ = ack.send(examined);
            }
            continue;
        }
        for ack in acks.drain(..) {
            let _ = ack.send(true);
        }
        let wake = [(!pending.is_empty()).then_some(retry_at).flatten(), rechecks.iter().map(|(at, _)| *at).min(), next_scan]
            .into_iter()
            .flatten()
            .min();
        let timeout = wake.map_or(Duration::from_secs(3600), |at| at.saturating_duration_since(Instant::now()));
        match rx.recv_timeout(timeout) {
            Ok(message) => absorb(message, &mut pending, &mut acks),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}
