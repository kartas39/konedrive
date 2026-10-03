//! Everything this sub-project adds to the daemon: the helper link, the
//! content source, the hydration loop, and `SyncService` — the `org.konedrive.Folder`
//! D-Bus surface's own half of the work (`dbus.rs` is the thin zbus wrapper
//! around it, the same split `crate::account`/`crate::dbus` uses for
//! `Account`). There is one `SyncService` per account; the helper link, its
//! supervisor and the per-inode locks are the daemon's, in `hub.rs`.

pub mod activity;
pub mod baloo;
pub mod conditions;
pub mod dbus;
pub mod disk;
pub mod graph_source;
pub mod helper;
pub mod helper_status;
pub mod hub;
pub mod kept_back;
pub mod listing;
pub mod live;
pub mod local;
pub mod local_scan;
pub mod materialize;
pub mod network;
pub mod outbox_api;
pub mod pin;
pub mod root;
pub mod running;
pub mod source;
pub mod thumbs;
pub mod totals;
pub mod upload;
pub mod watcher;
pub mod write_mode;

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::future::Future;
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::MetadataExt;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use activity::{Kind, Report, Tracked};
use async_trait::async_trait;
use baloo::Baloo;
use futures_util::FutureExt;
use helper::{Clearance, HelperError, HelperLink, HydrateRequest, NotCleared};
use helper_status::{HelperState, HelperUnit};
use konedrive_fs::placeholder::{read_stamp, read_state, stamp_matches, State, StateError, XATTR_STATE};
use root::{DehydrateError, RecoveryError, RecoveryReport, RegisterError, SyncRoot};
use source::{Answered, ContentSource, Fetched, FillError, LocalDir, SourceError};
use tokio::sync::{watch, Notify};
use tokio_util::sync::CancellationToken;

use crate::config::{ConfigStore, Mode, RootConfig};
use crate::state::{SignInState, StateHandle};

/// Hydration requests taken off the queue at once: the helper's whole credit
/// (`konedrive_proto::MAX_OUTSTANDING_HYDRATIONS`). Each is routed to its account and then
/// waits for a slot of that account's transfer pool (`crate::pool`, `Class::Open`).
pub const FILL_ADMISSION: usize = konedrive_proto::MAX_OUTSTANDING_HYDRATIONS;

/// Answers hydration requests until the helper goes away. Each request is routed
/// to its account first, then takes a slot of that account's transfer pool — an
/// open goes before any background work and may use the pool's reserve — and no
/// request is ever dropped silently.
///
/// An admission permit ([`FILL_ADMISSION`]) is acquired *before* spawning, not
/// inside the spawned task. Acquiring nothing would drain the bounded mpsc
/// of hydration requests into an unbounded pile of tasks — each holding a
/// suspended open's event descriptor — as fast as the helper could send
/// them, destroying the backpressure the channel exists to provide. The pool's
/// slot is taken inside the task, after routing, so that one account's full pool
/// never holds up another account's open.
/// Blocking here, before `recv()` is called again, propagates that
/// backpressure all the way back to the helper — but only as far as the
/// request queue. It must never reach the socket: the reader thread that
/// fills the queue is also the one that reads the `Ack` each fill below
/// waits for before it lets go of its permit, so a reader stopped by a full
/// queue with requests still ahead of an `Ack` in the socket wedges the
/// fills for good. The helper keeps at most
/// `konedrive_proto::MAX_OUTSTANDING_HYDRATIONS` requests outstanding on a
/// connection and the queue is exactly that deep, so the reader never stops;
/// beyond that the helper holds further hydrations back itself, and sends
/// each as one of these finishes.
///
/// Every fill is tracked in a `JoinSet` and the set is drained before this
/// returns, so a shutdown (or a helper that disconnects) lets the fills that
/// are already running finish and answer, rather than cutting them mid-write
/// and leaving `state=hydrating` behind on disk. That drain can take as long
/// as a download, so nothing that must react to the connection ending may
/// wait for this to return: [`supervise_helper`] runs it as a task of its
/// own and waits on [`HelperLink::closed`] instead.
///
/// # Per-inode serialization
///
/// Dehydration (`docs/design/hydration.md` §8) promises "the daemon
/// serializes operations per inode, so a
/// hydration request for a file being dehydrated runs after the dehydration
/// finishes", but nothing enforced that: `grep` finds no such lock anywhere
/// in this crate before this, because nothing before it ever ran
/// `serve_hydrations` and `root::dehydrate` at once — `main.rs` called
/// neither. This is what wires both into the same running daemon (see
/// [`SyncService::dehydrate`], which shares the same `locks` table), so this
/// is the first point at which two fills of the *same* file — one a
/// hydration, one a dehydration's punch — could run concurrently and tear
/// it.
///
/// `locks` is keyed by `(st_dev, st_ino)` read from the descriptor itself,
/// which is what "per inode" means and the only key the
/// two sides can be made to agree on. The version this replaces keyed on a
/// path string — `readlink("/proc/self/fd/<n>")` here, `canonicalize()` on
/// the D-Bus side — and two names for one inode therefore did not serialize
/// at all: measured, `ln f.bin g.bin` plus one `Hydrate` call on each name
/// put **two fills in flight on the same inode**, where a failing fetch's
/// roll-back (`online-only` + `punch_all`) lands on top of the other fill's
/// committed `hydrated`, leaving a file labelled `hydrated` over a hole —
/// which the helper then allows *and* ignore-marks. `readlink` also answers
/// `"<path> (deleted)"` for an unlinked file, and any rename between the two
/// sides' key computations desynchronised them.
pub async fn serve_hydrations(
    link: HelperLink,
    requests: tokio::sync::mpsc::Receiver<HydrateRequest>,
    source: Arc<dyn ContentSource>,
    locks: InodeLocks,
) {
    let nowhere = Report::nowhere();
    serve_hydrations_reporting(link, requests, source, locks, nowhere).await;
}

/// [`serve_hydrations`], reporting each fill into `report`: a
/// `Transfers` entry while it downloads, then a `downloaded` or `failed`
/// event, and a new measurement of the folder's space. The daemon runs the
/// same loop with every fill routed to its account ([`hub::supervise`]);
/// what a fill answers the opener is the same either way, and it is
/// answered before anything is recorded.
pub async fn serve_hydrations_reporting(
    link: HelperLink,
    requests: tokio::sync::mpsc::Receiver<HydrateRequest>,
    source: Arc<dyn ContentSource>,
    locks: InodeLocks,
    report: Report,
) {
    let pool = crate::pool::TransferPool::new(crate::pool::DEFAULT_CEILING);
    serve(link, requests, locks, Fillers::One(source, report, pool)).await;
}

/// Who fills a hydration request, and where it is reported.
#[derive(Clone)]
enum Fillers {
    /// One source, one report, one pool, whatever the file (tests, the VM suite).
    One(Arc<dyn ContentSource>, Report, Arc<crate::pool::TransferPool>),
    /// The account the file belongs to ([`hub::HelperHub::route`]): the
    /// daemon's.
    Routed(Arc<hub::HelperHub>),
}

impl Fillers {
    async fn route(&self, fd: &std::os::fd::OwnedFd) -> Option<(Arc<dyn ContentSource>, Report, Arc<crate::pool::TransferPool>)> {
        match self {
            Fillers::One(source, report, pool) => Some((Arc::clone(source), report.clone(), Arc::clone(pool))),
            Fillers::Routed(hub) => hub.route(fd).await.map(hub::filler),
        }
    }
}

/// The loop behind [`serve_hydrations_reporting`] and the hub's: at most
/// [`FILL_ADMISSION`] requests taken at once, each filled in a slot of its account's pool.
async fn serve(
    link: HelperLink,
    mut requests: tokio::sync::mpsc::Receiver<HydrateRequest>,
    locks: InodeLocks,
    fillers: Fillers,
) {
    let permits = Arc::new(tokio::sync::Semaphore::new(FILL_ADMISSION));
    let mut running = tokio::task::JoinSet::new();
    while let Some(HydrateRequest { req_id, fd }) = requests.recv().await {
        // Reap whatever finished while we were waiting; the set must not
        // accumulate the results of completed fills for the life of the
        // daemon.
        while running.try_join_next().is_some() {}
        // A request that arrives, or reaches the front, after its
        // connection ended is not filled: the helper answered
        // its opener `EIO` when the connection went (its disconnect guard
        // takes every job the connection had), so a fill would download a
        // file for nobody — and, while four fill slots are taken, keep this
        // loop from noticing the end at all.
        let permit = tokio::select! {
            permit = Arc::clone(&permits).acquire_owned() => permit.expect("semaphore closed"),
            () = link.closed() => {
                tracing::warn!(
                    "hydration request {req_id} came from a helper connection that has ended; its \
                     opener was answered then, so it is not filled"
                );
                continue;
            }
        };
        if link.is_closed() {
            tracing::warn!(
                "hydration request {req_id} came from a helper connection that has ended; its \
                 opener was answered then, so it is not filled"
            );
            continue;
        }
        let link = link.clone();
        let fillers = fillers.clone();
        let locks = locks.clone();
        running.spawn(async move {
            let permit = permit;
            // Which account's file this is (design §2.4). One that is in no
            // account's folder is denied rather than filled from a guess;
            // the next open tries again.
            let Some((source, report, pool)) = fillers.route(&fd).await else {
                tracing::warn!(
                    "hydration request {req_id} is for a file in none of the folders ({}); \
                     denying that open with EIO",
                    fd_path(&fd)
                );
                if let Err(e) = link.hydrate_done(req_id, libc::EIO).await {
                    tracing::error!("cannot report hydration {req_id}: {e}");
                }
                drop(permit);
                return;
            };
            // Routed first, then a slot of that account's pool: an open goes before
            // any background work there, and may use the pool's reserve. A connection
            // that ends meanwhile had its opener answered by the helper. The placeholder's
            // size says whether it is a large transfer: counted as one, never held by the
            // large-file limit.
            let bytes = nix::sys::stat::fstat(&fd).map_or(0, |stat| stat.st_size.max(0) as u64);
            let mut slot = tokio::select! {
                slot = pool.acquire_sized(crate::pool::Class::Open, crate::pool::Size::of(bytes)) => slot,
                () = link.closed() => {
                    tracing::warn!("hydration request {req_id} waited for a transfer slot until its helper connection ended; not filled");
                    return;
                }
            };
            // The identity the lock is taken on: `fstat` on the event fd
            // itself, read before the fd is handed to `hydrate` (which
            // consumes it). A descriptor whose identity cannot be read at
            // all still gets filled — nothing here is dropped for the sake
            // of the lock — but that is a degradation, not a detail, so it
            // is logged rather than silently taken (the version this
            // replaces read `/proc/self/fd/<n>` and said nothing when the
            // readlink failed).
            let key = match InodeKey::of_fd(&fd) {
                Ok(key) => Some(key),
                Err(e) => {
                    tracing::warn!(
                        "cannot read the identity of the file in request {req_id}: {e}; filling \
                         it without the per-inode lock, so a concurrent dehydration of the same \
                         file is not serialized against this fill"
                    );
                    None
                }
            };
            let inode_guard = match key {
                Some(key) => Some(locks.lock(key).await),
                None => None,
            };
            // Only what is shown: the name the kernel has for
            // the file right now, read before `answer_request` takes the fd.
            let shown = fd_path(&fd);
            let tracked = Tracked::opening(Arc::clone(&source), report.transfers.clone(), shown.clone());
            // A panic anywhere in the fill — including inside a
            // `ContentSource` we did not write — must not become an
            // unanswerable event in the kernel. Unwinding out of here would
            // close the event fd and produce no errno at all, so
            // `hydrate_done` would never be called and the suspended
            // `open()` would wait forever: §5.2's 30 s bound covers only "the
            // owner's daemon is not connected", and this daemon is connected.
            // Degrading it to an `EIO` denial costs the user one failed open.
            //
            // What the request finds under the lock decides what it does
            //: a file filled while the request waited is
            // answered as it is — see `source::answer_request`.
            //
            // A file taken off the disk meanwhile, because OneDrive removed
            // its item, stops its fill where it is (issue #104): its opener is
            // told it is gone.
            let filled = unless_removed(inode_guard.as_ref(), AssertUnwindSafe(source::answer_request(fd, &tracked, Some(&link))).catch_unwind()).await;
            let size = tracked.fetched();
            // Whatever came of it, the download is over.
            drop(tracked);
            let (errno, event) = match filled {
                None => {
                    tracing::info!("the hydration of request {req_id} stopped: its file was removed in OneDrive");
                    (libc::ENOENT, Some(activity::event(Kind::Failed, shown, "removed in OneDrive".to_owned())))
                }
                Some(Ok(answered)) => {
                    if matches!(answered, Answered::Filled) {
                        slot.succeeded();
                    }
                    (answered.errno(), fill_event(&answered, &shown, size))
                }
                Some(Err(_)) => {
                    tracing::error!(
                        "the hydration of request {req_id} panicked; denying that open with EIO \
                         rather than leaving it suspended forever"
                    );
                    (libc::EIO, Some(activity::event(Kind::Failed, shown, activity::failure_reason(libc::EIO))))
                }
            };
            if let Err(e) = link.hydrate_done(req_id, errno).await {
                tracing::error!("cannot report hydration {req_id}: {e}");
            }
            // The slot goes back before anything is recorded: a record that
            // waits (the log is SQLite) must not keep a fifth request from
            // being filled.
            drop(inode_guard);
            drop(slot);
            drop(permit);
            if let Some(event) = event {
                report.activity.record(vec![event]).await;
                report.space.kick();
            }
        });
    }
    while running.join_next().await.is_some() {}
}

/// The name the kernel has for an open file, for showing it:
/// `/proc/self/fd/<n>`, read, never followed. Empty if it cannot be read.
fn fd_path(fd: &impl AsRawFd) -> String {
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

/// What a fill of `path` records: `downloaded` with its size
/// when something was downloaded, `failed` with why when a fill ran and
/// failed, and nothing when there was nothing to do.
fn fill_event(answered: &Answered, path: &str, size: Option<u64>) -> Option<activity::Event> {
    match answered {
        Answered::Filled => Some(activity::event(Kind::Downloaded, path, activity::human_size(size.unwrap_or(0)))),
        Answered::Failed(FillError::Errno(errno)) => Some(activity::event(Kind::Failed, path, activity::failure_reason(*errno))),
        Answered::Failed(FillError::NotCleared(why)) => Some(activity::event(Kind::Failed, path, why.to_string())),
        Answered::AlreadyThere | Answered::NotOurs => None,
    }
}

/// A file's identity, the way means "per inode": the `(st_dev,
/// st_ino)` pair, read from an open descriptor and never spelled as a name
///. Two links to one inode share a key; a rename changes no
/// key at all.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct InodeKey {
    dev: u64,
    ino: u64,
}

impl InodeKey {
    /// The identity of an already-open file. Both sides of the lock have a
    /// descriptor by construction: `serve_hydrations` is handed the event
    /// fd, and `SyncService` opens through `SyncRoot::open_inside` *before*
    /// it takes the lock.
    pub fn of(file: &File) -> io::Result<Self> {
        let meta = file.metadata()?;
        Ok(Self { dev: meta.dev(), ino: meta.ino() })
    }

    /// As [`of`](Self::of), for a descriptor that is not a `File` —
    /// `serve_hydrations` must not consume the event fd to read its
    /// identity, since `source::hydrate` takes ownership of it afterwards.
    pub fn of_fd(fd: impl AsFd) -> io::Result<Self> {
        let stat = nix::sys::stat::fstat(fd)
            .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
        Ok(Self { dev: stat.st_dev as u64, ino: stat.st_ino as u64 })
    }
}

/// One inode's slot: the mutex itself, and how many callers are holding or
/// waiting for it.
struct Slot {
    mutex: Arc<tokio::sync::Mutex<()>>,
    /// Cancelled when the inode is taken off the disk because OneDrive
    /// removed its item ([`InodeLocks::cancel`]): a fill that holds the lock
    /// stops (issue #104).
    cancel: CancellationToken,
    /// Incremented before the caller starts waiting and decremented when it
    /// lets go — whether it acquired the lock or was cancelled while parked
    /// (see [`Row`]). The row is removed when this reaches zero.
    users: usize,
}

type LockTable = Arc<Mutex<HashMap<InodeKey, Slot>>>;

/// Serializes hydration and dehydration of the same inode: promise
/// that nothing enforced before this task (see [`serve_hydrations`]'s doc
/// comment). A file being hydrated and dehydrated at the same time is a torn
/// file.
///
/// Cheap to hold onto for the life of the daemon: each row is dropped from
/// the table the moment its last user lets go, so the table never grows past
/// the number of inodes genuinely in flight.
///
/// The bookkeeping is an explicit `users` count rather than an
/// `Arc::strong_count` heuristic. The count is exact — every caller adds
/// one before it waits and removes one when it lets go — so "is anyone else
/// using this row?" has an answer that does not depend on how many `Arc`
/// clones a particular code path happens to keep alive, on the drop order of
/// a struct's fields, or on whether a cancelled waiter's in-flight future
/// has been dropped yet. The strong-count form this replaces got the last of
/// those wrong: a waiter whose future was dropped left its row in the table
/// forever (measured), and its threshold could not be raised or lowered by
/// one without either leaking rows or handing two callers different mutexes
/// for the same inode.
#[derive(Clone, Default)]
pub struct InodeLocks {
    inner: LockTable,
}

impl InodeLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Waits for exclusive use of `key`. Held until the returned guard is
    /// dropped.
    pub async fn lock(&self, key: InodeKey) -> InodeGuard {
        let mutex = {
            let mut map = self.inner.lock().unwrap();
            let slot = map
                .entry(key)
                .or_insert_with(|| Slot { mutex: Arc::new(tokio::sync::Mutex::new(())), cancel: CancellationToken::new(), users: 0 });
            slot.users += 1;
            (Arc::clone(&slot.mutex), slot.cancel.clone())
        };
        let (mutex, cancel) = mutex;
        // Armed *before* the await, so a caller whose future is dropped
        // while it is parked below still takes itself out of the count. A
        // D-Bus method's future is dropped whenever its caller goes away,
        // and `hydrate_now` can park here for as long as another fill of the
        // same file takes, which has no time limit.
        let row = Row { table: Arc::clone(&self.inner), key };
        let guard = mutex.lock_owned().await;
        InodeGuard { _guard: guard, cancel, _row: row }
    }

    /// Exclusive use of `key` if nobody holds or awaits it now, and `None`
    /// otherwise — without waiting. Startup recovery takes it this way
    ///: a fill or a free-up of the same file is running in this
    /// daemon, and waiting for it would hold the whole reconnect behind a
    /// download (the very thing took away), while the file it is
    /// busy with is one recovery leaves alone anyway.
    pub fn try_lock(&self, key: InodeKey) -> Option<InodeGuard> {
        let mutex = {
            let mut map = self.inner.lock().unwrap();
            let slot = map
                .entry(key)
                .or_insert_with(|| Slot { mutex: Arc::new(tokio::sync::Mutex::new(())), cancel: CancellationToken::new(), users: 0 });
            slot.users += 1;
            (Arc::clone(&slot.mutex), slot.cancel.clone())
        };
        let (mutex, cancel) = mutex;
        // Counted like any other user until it gives up: dropped with the
        // refusal, it takes itself out of the count and, if it was the only
        // one, the row out of the table.
        let row = Row { table: Arc::clone(&self.inner), key };
        let guard = mutex.try_lock_owned().ok()?;
        Some(InodeGuard { _guard: guard, cancel, _row: row })
    }

    /// The inode `key` is being taken off the disk because OneDrive removed
    /// its item (issue #104): whoever holds or awaits its lock — a fill — is
    /// told to stop ([`InodeGuard::cancelled`]). Whether anyone was.
    pub fn cancel(&self, key: InodeKey) -> bool {
        match self.inner.lock().unwrap().get_mut(&key) {
            Some(slot) => {
                slot.cancel.cancel();
                // Whoever comes for the lock from now on gets a token of its
                // own: a download that starts after the stop is not stopped
                // by it.
                slot.cancel = CancellationToken::new();
                true
            }
            None => false,
        }
    }

    /// How many inodes the table is tracking. Tests only: the table growing
    /// without bound is the failure mode this number exists to rule out.
    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// How many callers are holding or waiting for one inode. Tests only,
    /// and specifically so that a test about *two* callers can wait until
    /// the second one has genuinely arrived: guessing with `yield_now` makes
    /// a test that measures the cleanup rule measure nothing at all when the
    /// waiter has not started yet.
    #[cfg(test)]
    fn users(&self, key: InodeKey) -> usize {
        self.inner.lock().unwrap().get(&key).map_or(0, |slot| slot.users)
    }
}

/// One caller's place in the count for one inode. Removes itself — and the
/// row, if it was the last — however it goes away.
struct Row {
    table: LockTable,
    key: InodeKey,
}

impl Drop for Row {
    fn drop(&mut self) {
        let mut map = self.table.lock().unwrap();
        if let std::collections::hash_map::Entry::Occupied(mut slot) = map.entry(self.key) {
            debug_assert!(slot.get().users > 0, "a row cannot have fewer than one user");
            slot.get_mut().users = slot.get().users.saturating_sub(1);
            if slot.get().users == 0 {
                slot.remove();
            }
        }
    }
}

/// Holds one inode's slot in [`InodeLocks`]. Releases the lock, then leaves
/// the count, when dropped.
pub struct InodeGuard {
    // Never read: its entire job is to stay alive, and locked, until this
    // guard drops. Declared first so the mutex is released before `_row`
    // leaves the count — a waiter woken by that release has already counted
    // itself, so its row cannot be removed from under it either way.
    _guard: tokio::sync::OwnedMutexGuard<()>,
    cancel: CancellationToken,
    _row: Row,
}

impl InodeGuard {
    /// Done when the inode is being taken off the disk ([`InodeLocks::cancel`]).
    pub async fn cancelled(&self) {
        self.cancel.cancelled().await
    }
}

/// Runs `fill` — a download into a file — unless the file is taken off the
/// disk meanwhile because OneDrive removed its item: then it is dropped where
/// it is, and `None` says so (issue #104). Without a guard it runs to its end.
pub(crate) async fn unless_removed<T>(guard: Option<&InodeGuard>, fill: impl std::future::Future<Output = T>) -> Option<T> {
    match guard {
        Some(guard) => tokio::select! {
            done = fill => Some(done),
            () = guard.cancelled() => None,
        },
        None => Some(fill.await),
    }
}

// --- the folder's interfaces' own half of the work ------------------------
//
// `dbus.rs` is the thin zbus wrapper (the same split `crate::account` /
// `crate::dbus` uses for `Account`); everything that actually does
// something lives here, so it can be exercised without a bus at all.

/// What `Folder.State` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootState {
    /// No root is registered.
    None,
    /// A root is registered and, as far as this daemon knows, healthy.
    Ready,
    /// A root is registered, but **nothing intercepts opens inside it**
    ///: it was registered through
    /// `RegisterWithoutInterception`, so a placeholder nobody fills
    /// reads as zeros until it is hydrated by hand. Distinct from `ready`
    /// precisely because a client must be able to tell the two apart. A
    /// folder registered that way because no helper was connected leaves
    /// this state when one connects (`SyncService::upgrade`).
    NoInterception,
    /// A root is registered, but something about it needs attention: startup
    /// recovery could not finish, could not even run, or the helper went
    /// away. See `LastError`.
    Error,
}

impl RootState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Ready => "ready",
            Self::NoInterception => "no-interception",
            Self::Error => "error",
        }
    }
}

/// The observable sync state; `sync::dbus` turns changes into
/// `PropertiesChanged`, exactly as `state::AccountSnapshot` does for
/// `Account`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSnapshot {
    pub root_path: String,
    pub root_state: RootState,
    pub last_error: String,
    /// An initial or `410` listing of the drive is running.
    pub listing: bool,
    pub items_listed: u64,
    pub items_placed: u64,
    pub skipped_count: u64,
    /// What the folder's sync last ran into; `None` once a cycle succeeds.
    pub sync_trouble: Option<SyncTrouble>,
    /// Why files changed in the cloud are not updated here yet.
    pub replacement_note: String,
    /// `LastChecked`: unix seconds of the last cycle that
    /// succeeded, 0 for never.
    pub last_checked: i64,
    /// `LocalBytes`: what the folder's files take on disk, as last measured
    /// (`activity::LocalSpace`).
    pub local_bytes: u64,
    /// `Conflicts.Count`: conflicts whose rescued file is still there.
    pub conflict_count: u32,
    /// `PinnedCount`: files and folders with a pin of their own
    /// ([`pin::Pins`]).
    pub pinned_count: u32,
    /// `HelperState` (HS1).
    pub helper_state: HelperState,
    /// The registered folder needs the helper and does not have it (HS2,
    /// HS3): a folder with interception whose link is down, or one that
    /// shows OneDrive and is not intercepted yet. `RootState` reads `error`
    /// then, and `LastError` begins with what [`HelperState::advice`] says.
    pub waits_for_helper: bool,
    /// What the watcher of a read-write folder says while it runs: that part
    /// of the folder is found only by a periodic scan, or that new folders
    /// wait for the helper's mark (`watcher::WatchStatus::note`). Empty
    /// otherwise. The folder moved or deleted is said in `last_error`, since
    /// it outlasts the watcher.
    pub watch_note: String,
    /// The folder's filesystem changed since its file handles were recorded,
    /// and they were taken again: said until the next Full local scan
    /// that finds them current. Empty otherwise.
    pub handles_note: String,
    /// What keeps the outbox's changes from going: the write gate
    /// closed under a read-write folder, or a read-only one whose sync holds its cycles while
    /// changes wait. Empty otherwise.
    pub outbox_note: String,
    /// `PendingCount`, `PendingBytes`, `BlockedCount`: the outbox as its
    /// worker last saw it.
    pub pending_count: u32,
    pub pending_bytes: u64,
    pub blocked_count: u32,
    /// `HeldCount`: removals the mass-delete guard holds for `ConfirmDeletes`
    /// or `RestoreDeletes` (the outbox on the bus).
    pub held_count: u32,
    /// `Paused` and `PausedUntil`: `Some(until)` while paused, unix seconds,
    /// 0 meaning until resumed (`outbox_api`).
    pub paused_until: Option<i64>,
    /// `HeldBack`: why the account holds its background work back by itself
    /// (`running::Hold`), empty when it does not.
    pub held_back: String,
    /// `LiveChanges`: whether changes made in OneDrive arrive at once, through the
    /// notification socket (`live`).
    pub live_changes: live::LiveChanges,
    /// `Transfers.Uploads`: (full path, bytes sent, bytes in all), as `Downloads`.
    pub uploads: Vec<(String, u64, u64)>,
    /// `QuotaFull`: OneDrive is full and no content goes up (issue #2).
    pub quota_full: bool,
    /// `QuotaWaitingCount`, `QuotaWaitingBytes`: while full, the changes
    /// that send content; `TooBigCount`: files too big for the space left.
    pub space_waiting_count: u32,
    pub space_waiting_bytes: u64,
    pub too_big_count: u32,
    /// Their size: not on the bus, but taken off what is left to upload ([`totals`]).
    pub too_big_bytes: u64,
    /// `DownloadSpeed`, `UploadSpeed`, `PoolInUse`, `PoolSize`, `PoolCeiling`, `LargeStreams`,
    /// `LargeStreamLimit`, `RetryAfter`: the account's transfer pool, once a second while
    /// anything moves or a `Retry-After` runs. `ActiveDownloads`, `ActiveUploads` and
    /// `LargeFiles` count the files of `Transfers.Downloads` and `Uploads` instead (issue #50).
    pub throughput: crate::pool::Throughput,
    /// The pinned files waiting to download (not those under way), and their size
    /// ([`pin::Pins`]).
    pub pinned_waiting: (u32, u64),
    /// `DownloadLeftCount`, `DownloadLeftBytes`, `DownloadDoneBytes`, `DownloadTimeLeft` and
    /// the same four for uploads: counted from the rest by [`totals::run`].
    pub queue: totals::QueueTotals,
    /// `LocalScan`'s `State`, `Reason`, `Started`, `Directories`, `Files`,
    /// `Expected`, `Finished`, `Took`: the Full local scan (issue #8).
    pub scan: local_scan::LocalScan,
}

impl SyncSnapshot {
    /// Whether the account's background work stops: paused by the user, or held back.
    pub fn stopped(&self) -> bool {
        self.paused_until.is_some() || !self.held_back.is_empty()
    }
}

impl Default for SyncSnapshot {
    fn default() -> Self {
        Self {
            root_path: String::new(),
            root_state: RootState::None,
            last_error: String::new(),
            listing: false,
            items_listed: 0,
            items_placed: 0,
            skipped_count: 0,
            sync_trouble: None,
            replacement_note: String::new(),
            last_checked: 0,
            local_bytes: 0,
            conflict_count: 0,
            pinned_count: 0,
            helper_state: HelperState::Unknown,
            waits_for_helper: false,
            watch_note: String::new(),
            handles_note: String::new(),
            outbox_note: String::new(),
            pending_count: 0,
            pending_bytes: 0,
            blocked_count: 0,
            held_count: 0,
            paused_until: None,
            held_back: String::new(),
            live_changes: live::LiveChanges::Off,
            uploads: Vec::new(),
            quota_full: false,
            space_waiting_count: 0,
            space_waiting_bytes: 0,
            too_big_count: 0,
            too_big_bytes: 0,
            throughput: crate::pool::Throughput::default(),
            pinned_waiting: (0, 0),
            queue: totals::QueueTotals::default(),
            scan: local_scan::LocalScan::default(),
        }
    }
}

/// What the folder's sync last ran into. `blocking` trouble —
/// signed out, another account, an unusable store — makes `RootState` read
/// `error`; the rest (no network) is said in `LastError` and retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncTrouble {
    pub text: String,
    pub blocking: bool,
}

/// What a registered folder shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSource {
    /// Filled from a directory with `PopulateFromDirectory`, as in part 1.
    Local,
    /// Listed from the signed-in drive, locked, and kept in step with it.
    OneDrive,
}

impl RootSource {
    fn as_str(self) -> &'static str {
        match self {
            RootSource::Local => "local",
            RootSource::OneDrive => "onedrive",
        }
    }

    fn parse(value: &str) -> Self {
        if value == "onedrive" {
            RootSource::OneDrive
        } else {
            RootSource::Local
        }
    }
}

/// Where a OneDrive folder's own files live.
#[derive(Debug, Clone)]
pub struct SyncPaths {
    pub tree_db: PathBuf,
    pub rescue_dir: PathBuf,
    /// The freedesktop thumbnail cache. `None` runs no thumbnail
    /// filler at all: the VM suite's real-account run, which must not fetch
    /// a thumbnail of every image in the drive.
    pub thumbnails: Option<PathBuf>,
}

/// Where a folder is recorded so that it survives a restart: its account's
/// `[accounts.root]` in `config.toml`, written only through the daemon's one
/// [`ConfigStore`].
#[derive(Clone)]
pub struct Persist {
    pub store: Arc<ConfigStore>,
    /// The account's id.
    pub account: String,
}

/// `RootState` as published: the registration's state, unless
/// the folder waits for the helper or the sync is blocked (`error`), or an
/// initial listing runs (`listing`).
///
/// `listing` stands only for `ready`: a folder
/// without interception keeps saying `no-interception`, the one word that
/// warns its files read as zeros. Since HS2 such a folder never lists
/// anyway — it is local, or it shows OneDrive and waits for the helper.
pub fn published_state(s: &SyncSnapshot) -> &'static str {
    let blocked = s.waits_for_helper || s.sync_trouble.as_ref().is_some_and(|t| t.blocking);
    match s.root_state {
        RootState::Ready | RootState::NoInterception if blocked => "error",
        RootState::Ready if s.listing => "listing",
        other => other.as_str(),
    }
}

/// `LastError` as published: what the helper's absence means, the
/// registration's text, the sync's and the replacement note, in that order
/// — problems only. Where local work was moved out of the way is a conflict
/// (`Conflicts.List()`, `Conflicts.Count`), not a problem, and is not said here
///: said here, it stayed until a Forget, and a folder that
/// ever had a conflict read as trouble for good.
///
/// The helper's part (HS3) is worked out from `HelperState` whenever that
/// changes, never frozen when the link dropped: "not running" becomes
/// "failed" when systemd says so.
pub fn published_error(s: &SyncSnapshot) -> String {
    let helper = if s.waits_for_helper { s.helper_state.advice().unwrap_or("") } else { "" };
    [
        helper,
        s.last_error.as_str(),
        s.sync_trouble.as_ref().map_or("", |t| t.text.as_str()),
        s.replacement_note.as_str(),
        s.watch_note.as_str(),
        s.handles_note.as_str(),
        s.outbox_note.as_str(),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(". ")
}

/// Shared, observable sync state (see `state::StateHandle`, the same shape
/// for `Account`).
#[derive(Clone)]
pub struct SyncStateHandle {
    tx: Arc<watch::Sender<SyncSnapshot>>,
}

impl SyncStateHandle {
    pub fn new(initial: SyncSnapshot) -> Self {
        let (tx, _rx) = watch::channel(initial);
        Self { tx: Arc::new(tx) }
    }

    pub fn get(&self) -> SyncSnapshot {
        self.tx.borrow().clone()
    }

    pub fn update(&self, change: impl FnOnce(&mut SyncSnapshot)) {
        self.tx.send_modify(change);
    }

    /// The transfer pool's throughput, told only when it changed.
    pub fn set_throughput(&self, throughput: crate::pool::Throughput) {
        self.tx.send_if_modified(|s| std::mem::replace(&mut s.throughput, throughput) != throughput);
    }

    /// The queue totals, told only when they changed.
    pub fn set_queue(&self, queue: totals::QueueTotals) {
        self.tx.send_if_modified(|s| std::mem::replace(&mut s.queue, queue) != queue);
    }

    /// `LiveChanges`, told only when it changed.
    pub fn set_live_changes(&self, live: live::LiveChanges) {
        self.tx.send_if_modified(|s| std::mem::replace(&mut s.live_changes, live) != live);
    }

    pub fn subscribe(&self) -> watch::Receiver<SyncSnapshot> {
        self.tx.subscribe()
    }
}

/// Everything the folder can refuse, flattened from `RegisterError` and
/// `DehydrateError` plus the failures that only exist at this layer (no
/// root registered, no helper connected, a path outside the root).
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("the folder must be empty")]
    NotEmpty,
    #[error("{0}")]
    Unsupported(String),
    #[error("the file is in use")]
    InUse,
    #[error("no sync root is registered")]
    NoRoot,
    #[error("the konedrive helper is not connected")]
    NoHelper,
    #[error("not a OneDrive file")]
    NotManaged,
    #[error("the file is not downloaded")]
    NotHydrated,
    #[error("the file was modified locally")]
    ModifiedLocally,
    #[error("not a plain file inside this sync root")]
    OutsideRoot,
    /// A folder that remembers another account's drive (design §8.3);
    /// `Folder` answers it `NotEmpty`.
    #[error("this folder holds another OneDrive account's files; choose an empty folder")]
    ForeignFolder,
    /// A folder that is, is inside, or contains another account's folder
    /// (design §8.3); the other account's label.
    #[error("this folder is, is inside, or contains the folder of the account '{0}'")]
    Overlaps(String),
    #[error("a sync root is already registered; forget it first")]
    AlreadyRegistered,
    #[error("nobody is signed in")]
    NotSignedIn,
    #[error("no content source is registered; call PopulateFromDirectory first")]
    NoSource,
    #[error("no conflict is recorded for {0}")]
    NoConflict(String),
    /// A free-up of something a pin keeps on this device: the message is
    /// [`pin::refusal`]'s, naming the path refused and what pins it.
    #[error("{0}")]
    NotAllowed(String),
    /// A free-up of a file with a change waiting to be uploaded (write design
    /// §3.8): the message names it.
    #[error("{0} is not uploaded yet, so freeing it up would lose the changes made here")]
    NotUploaded(String),
    /// `WebUrl` of a file or folder that carries no item id: OneDrive does not
    /// have it yet, so it has no page there. `Files` answers it `NotUploaded`.
    #[error("{0} is not uploaded yet, so it has no page in OneDrive")]
    NotInOneDrive(String),
    /// OneDrive did not answer (`WebUrl`): no network, Graph kept refusing, the
    /// secret storage is locked, or the answer could not be read. The message
    /// is that cause alone; the clients put their own sentence in front of it.
    #[error("{0}")]
    Unreachable(String),
    /// An argument no value of which makes sense (`SetIgnorePatterns`).
    #[error("{0}")]
    InvalidArgs(String),
    /// A Forget, or `Accounts.Remove`, while changes wait to be uploaded:
    /// the tree store holding them would go. The message says how many, and what to do.
    #[error("{0}")]
    PendingUploads(String),
    #[error("{0}")]
    Io(String),
}

/// A Forget's refusal while `waiting` changes wait to be uploaded.
fn refuse_waiting(waiting: u64) -> Result<(), SyncError> {
    match waiting {
        0 => Ok(()),
        n => Err(SyncError::PendingUploads(format!(
            "{n} change(s) made here have not been uploaded yet, and would be lost with the folder's \
             record; wait until they are uploaded, or drop them with a forced switch of the account \
             to read-only (the files stay here as they are), then try again"
        ))),
    }
}

impl From<RegisterError> for SyncError {
    fn from(e: RegisterError) -> Self {
        match e {
            RegisterError::NotADirectory => SyncError::Unsupported("not a directory".into()),
            RegisterError::NotEmpty => SyncError::NotEmpty,
            RegisterError::Unsupported(why) => SyncError::Unsupported(why),
            RegisterError::Helper(why) => SyncError::Io(why),
        }
    }
}

impl From<DehydrateError> for SyncError {
    fn from(e: DehydrateError) -> Self {
        match e {
            DehydrateError::NotManaged => SyncError::NotManaged,
            DehydrateError::NotHydrated => SyncError::NotHydrated,
            DehydrateError::ModifiedLocally => SyncError::ModifiedLocally,
            DehydrateError::InUse => SyncError::InUse,
            DehydrateError::OutsideRoot => SyncError::OutsideRoot,
            DehydrateError::HelperNotConnected => SyncError::NoHelper,
            DehydrateError::Io(why) => SyncError::Io(why),
        }
    }
}

/// One account's folder: registration, the manual `PopulateFromDirectory`
/// fill, and per-file hydrate/dehydrate/state — what that account's
/// `org.konedrive.Folder` and its sibling interfaces expose, and what `org.konedrive.Files` routes to it.
///
/// # Why `hydrate_now` fills directly rather than only through interception
///
/// The original design describes a helper-connected `Hydrate()`
/// as opening the file and letting the kernel's `FAN_OPEN_PERM` interception
/// carry the request to `serve_hydrations`, the same path a real
/// application's `open()` takes — which is the right design *when a real
/// privileged helper has actually marked the root* (production, or a
/// `vng` test). It is unreachable in an unprivileged `cargo test`: nothing
/// unprivileged can hold `CAP_SYS_ADMIN`, so no fanotify group is ever
/// installed, `MarkDir`/`MarkFile` acks from the fake helper this crate's
/// own D-Bus tests use are just protocol replies, and an `open()` under
/// such a "marked" directory is not intercepted at all — it returns
/// immediately with whatever the placeholder already holds, i.e. nothing.
/// Relying on interception here would make `Hydrate()` either silently
/// serve zeros (an open that succeeds without being filled) or hang forever
/// in a real deployment that starts this method before the helper has
/// finished marking — both worse than what this does instead: hydrate_now
/// always fills the file itself, synchronously, through the same
/// `ContentSource`/`source::hydrate` the interception path uses, under the
/// same per-inode lock `serve_hydrations` takes. `SyncService` doubles as
/// that `ContentSource` (see the `ContentSource` impl below) precisely so
/// `serve_hydrations` can be started once at daemon startup, before any
/// root exists, and pick up whatever gets registered later.
pub struct SyncService {
    /// The link to the helper, its state and the per-inode locks, shared by
    /// every account of the daemon (design §2.1).
    hub: Arc<hub::HelperHub>,
    /// The hub's link cell: replaceable, because the helper can go away and
    /// come back — [`hub::supervise`] swaps it for `None` the moment the
    /// connection drops and back to a live link when it reconnects. Shared
    /// with a OneDrive folder's sync, which reads it at every reconcile.
    link: listing::LinkCell,
    /// The account interface's own state, on the same object path. §3.1
    /// refuses `RegisterRoot` when nobody is signed in, and
    /// this is what it asks. `None` only where nothing wired it up.
    account: Option<StateHandle>,
    /// The account's one quota (`crate::quota`), which the outbox's space check reads and
    /// adjusts ([`set_quota`](Self::set_quota)): until one is set, the quota kept in
    /// `account`'s state, or one of its own without an account.
    quota: Mutex<crate::quota::Quota>,
    /// Where the registered root is persisted, so it survives a restart
    /// (§3.1): the account's entry in `config.toml`. `None` disables
    /// persistence entirely.
    persist: Option<Persist>,
    state: SyncStateHandle,
    root: Mutex<Option<Registration>>,
    /// Taken for writing by everything that changes which root is registered
    /// or how — `register_root`, `register_root_without_interception`,
    /// `unregister_root`, `resume` — for the whole of the change, and for
    /// reading by what decides from the root's mode what to ask of the
    /// helper and then acts on it: `dehydrate` and `populate_from_directory`.
    ///
    /// zbus runs every method call in a task of its own, so without it two
    /// registrations both passed the "no root yet" check before either had
    /// committed, both reached the helper, and the last commit won — leaving
    /// the helper holding a root the daemon did not: a folder still marked,
    /// which a later registration without interception of that folder would
    /// hold with nothing intercepting opens in it.
    ///
    /// A OneDrive folder's sync shares this very lock (`ListingContext::
    /// lifecycle`): a reconcile holds it for reading while it changes the
    /// folder, so no registration changes under it.
    lifecycle: Arc<tokio::sync::RwLock<()>>,
    source: Mutex<Option<Arc<dyn ContentSource>>>,
    /// The hub's lock table: one inode belongs to one account only.
    locks: InodeLocks,
    /// A read-only Graph client, for a folder that shows OneDrive. `None`
    /// until `main` sets it; without it every folder is local.
    drive: Mutex<Option<crate::drive::DriveClient>>,
    sync_paths: Mutex<Option<SyncPaths>>,
    schedule: Mutex<listing::Schedule>,
    /// The running sync of a OneDrive folder. Shared with the task that
    /// nudges it when the account signs in ([`nudge_on_sign_in`]). Started
    /// and stopped only under `lifecycle` held for writing — except the stop
    /// a Forget makes before it takes that lock (see `unregister_root`).
    syncing: Arc<Mutex<Option<Syncing>>>,
    /// Its tree store, for `Skipped()`.
    ///
    /// The store's files are removed (`remove_tree_store`) only with
    /// `lifecycle` held for writing and the sync stopped, and nothing may be
    /// reading them then. So no clone of the store outlives
    /// [`stop_sync`](SyncService::stop_sync): the sync's own go when it
    /// returns (`Poller::stop` waits for every task that holds one). Any
    /// other clone is taken, and dropped, with `lifecycle` held for reading
    /// (`skipped`).
    store: Mutex<Option<crate::tree::Store>>,
    /// Keeps KDE's Baloo indexer out of a fresh OneDrive folder, and lets a
    /// forgotten one back in (`sync::baloo`). Starts as
    /// [`Baloo::disabled`], which runs no program at all — only `main`
    /// installs the real `balooctl6`; a test that forgets `set_baloo` must
    /// never reach the user's own indexer settings.
    baloo: Mutex<Arc<Baloo>>,
    /// Why this account's folder is held back (design §3.1: `config.toml`
    /// gives it what an earlier account has), if it is: it is not brought
    /// up, and no registration is made.
    held: Mutex<Option<String>>,
    /// The activity log, the conflicts, the downloads under way and the
    /// folder's space, shared with the hydration loop and a
    /// OneDrive folder's sync. Its store is a clone of `store`'s, attached by
    /// [`start_sync`](Self::start_sync) and detached by
    /// [`stop_sync`](Self::stop_sync), after which no write holds it.
    report: Report,
    /// "Always keep on this device": the pins and the downloads they ask
    /// for. Shared with a OneDrive folder's sync, which queues what it places
    /// under a pin and sweeps after every Full reconcile.
    pins: Arc<pin::Pins>,
    /// The account's mode as the folder follows it (`docs/design/writes.md` §2, §2.2):
    /// read-only keeps a OneDrive folder under the lock, read-write lifts it.
    /// Changed only by [`write_mode`]'s switch, with `lifecycle` held for
    /// writing and the sync stopped, so a running sync never sees it change.
    mode: Mutex<Mode>,
    /// This service, for the watcher's status hook, which may have to stop
    /// the sync from the watcher's thread (the folder moved or deleted).
    me: std::sync::Weak<SyncService>,
    /// The per-root tree lock (`docs/design/writes.md` §9): the outbox worker holds it
    /// across each commit that touches `items`, and a cycle must hold it from
    /// staging to swap, or the swap reverts the commit.
    tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// The account's ignore list (`docs/design/writes.md` §4.4), from `config.toml`: the
    /// watcher's examination reads it, `SetIgnorePatterns` changes it.
    ignore: local::ignore::SharedIgnore,
    /// The one timer that ends a timed pause on the bus (`outbox_api`).
    pause_timer: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// How many times the pause was shown: the timer ends it on the bus only
    /// if no `Pause` or `Resume` came after it read the store (the outbox on the bus).
    pause_shown: std::sync::atomic::AtomicU64,
    /// `NotUploadedSummary()` as the outbox worker last summed it (issue #38):
    /// answered from memory while the worker runs.
    kept_back: Mutex<Option<Vec<kept_back::SummaryRow>>>,
    /// Set by a forced switch's drop of the outbox (`PendingUploads::drop_pending_uploads`):
    /// the folder's turn to read-only drops what its watcher recorded since, and only then
    /// does a turn to read-only drop anything.
    drop_at_read_only: std::sync::atomic::AtomicBool,
    /// The account was switched to read-write, and the watcher that follows has not started
    /// yet: its Full local scan says so (`LocalScan.Reason`).
    switched_to_read_write: std::sync::atomic::AtomicBool,
    /// Works the account's mode out again when the write gate closes under the outbox worker
    /// ([`set_mode_check`](Self::set_mode_check)). None in tests.
    mode_check: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Told the drive the account's token reaches when a cycle finds it is not the folder's:
    /// the account's own, set where the account is wired up.
    drive_seen: Mutex<Option<listing::DriveSeen>>,
    /// The account's transfer pool (`crate::pool`): every download, upload and change of
    /// an item takes a slot of it. The drive set with [`set_drive`](Self::set_drive) reports
    /// into it.
    pool: Arc<crate::pool::TransferPool>,
    /// The large pinned files downloading in parts, and how many streams each has: who is
    /// due the next free large slot of `pool` (`source::parts`, issue #28).
    parts: Arc<source::Share>,
    /// What background work runs now (`running`): the one place every reader of the pause
    /// asks, with the account's settings from `config.toml`.
    running: Arc<running::Running>,
}

/// A OneDrive folder's sync while it runs.
struct Syncing {
    poller: listing::Poller,
    /// [`nudge_on_sign_in`], stopped with the poller.
    sign_in_watch: Option<tokio::task::JoinHandle<()>>,
    /// The thumbnail filler and the token that stops it: started
    /// and stopped with the poller, so a Forget leaves no clone of the tree
    /// store with it either. `None` when [`SyncPaths::thumbnails`] is.
    thumbnails: Option<(tokio::task::JoinHandle<()>, CancellationToken)>,
    /// A read-write folder's watcher: started in the same critical
    /// section that publishes this `Syncing`, and stopped by whoever takes it,
    /// so it lives exactly as long as the sync. `None` for a
    /// read-only folder.
    watcher: Option<write_mode::Watcher>,
    /// A read-write folder's outbox worker, which sends the rows the
    /// watcher's examination records: started and stopped with the watcher,
    /// in the same places. `None` for a read-only folder.
    outbox: Option<upload::OutboxWorker>,
}

/// A registered root and how — or whether — opens inside it are intercepted.
#[derive(Clone)]
struct Registration {
    root: SyncRoot,
    /// False only for a root registered through
    /// `RegisterWithoutInterception`.
    intercepted: bool,
    /// Whether its last recovery left interrupted files as found because a
    /// helper was running that this daemon had no link to, so
    /// the next link runs it again ([`SyncService::resume`]).
    recovery_deferred: bool,
    /// What it shows, decided when it was first registered and
    /// kept with it for good.
    source: RootSource,
    /// Registered and recovered ([`SyncService::commit`]), so that a OneDrive
    /// folder's sync may run. False for a root only held until its helper is
    /// back ([`SyncService::hold`]), and for one kept after a registration
    /// that failed ([`SyncService::abandon`]).
    brought_up: bool,
    /// Whether *this daemon* excluded the root from Baloo, so
    /// [`unregister_root`](SyncService::unregister_root) knows whether to
    /// take that exclusion back off. Always false outside
    /// [`SyncService::commit`]: `hold` and `abandon`'s kept-registered branch
    /// construct a `Registration` before `commit` has run, so nothing has
    /// been added to Baloo yet either.
    baloo_excluded: bool,
    /// Registered without interception only because no helper was connected
    ///, so it switches to interception when one connects
    /// ([`SyncService::upgrade`]). False for every intercepted root, and for
    /// one registered without interception on purpose — with a helper
    /// connected.
    upgrade_when_helper: bool,
    /// The device the folder is on, read once when the registration is made,
    /// for the hub's router: never a path looked at per request. `None` when
    /// the folder could not be looked at then.
    dev: Option<u64>,
}

/// A root as `config.toml` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Persisted {
    path: PathBuf,
    /// Empty in a config written before the id was recorded.
    root_id: String,
    intercepted: bool,
    source: RootSource,
    /// Whether this daemon is the one that excluded the root from Baloo
    ///; `false` in a config written before this existed.
    baloo_excluded: bool,
    /// [`Registration::upgrade_when_helper`].
    upgrade_when_helper: bool,
}

impl Persisted {
    fn of(
        root: &SyncRoot,
        intercepted: bool,
        source: RootSource,
        baloo_excluded: bool,
        upgrade_when_helper: bool,
    ) -> Self {
        Self {
            path: root.path.clone(),
            root_id: root.root_id.clone(),
            intercepted,
            source,
            baloo_excluded,
            upgrade_when_helper,
        }
    }
}

/// How a switch to interception ([`SyncService::upgrade`]) that did not go
/// through left the folder.
enum NotSwitched {
    /// As it was, without interception: the helper holds nothing of it. Why.
    Kept(String),
    /// Intercepted, waiting for the next connect: the helper may hold it.
    Held,
}

/// What `LastError` says while a root is registered without interception.
/// Spelled out rather than hinted at: this mode's whole risk is that a file
/// looks present and reads as zeros, so the one thing a user must not have
/// to infer is that they are in it.
pub const NO_INTERCEPTION_WARNING: &str =
    "this folder is registered WITHOUT interception: nothing fills a placeholder when it is \
     opened, so files in this folder read as zeros until they are explicitly hydrated";

/// What `LastError` adds when a folder registered without the helper could
/// not be switched to interception once the helper connected.
const SWITCH_FAILED: &str =
    "the konedrive helper is connected, but switching this folder to interception failed";

impl SyncService {
    /// `account` gates `RegisterRoot` on somebody being signed in (§3.1);
    /// `persist` is where the registered root is persisted so it survives a
    /// restart. Both are `None` in tests that exercise neither. The service
    /// has a hub of its own, holding `link`: the one account of a daemon.
    pub fn new(
        link: Option<HelperLink>,
        account: Option<StateHandle>,
        persist: Option<Persist>,
    ) -> Arc<Self> {
        Self::on_hub(&hub::HelperHub::with_link(link), account, persist)
    }

    /// One account's folder, on the daemon's `hub`, after every account the
    /// hub has already.
    pub fn on_hub(hub: &Arc<hub::HelperHub>, account: Option<StateHandle>, persist: Option<Persist>) -> Arc<Self> {
        hub.join(|helper_state| {
            let state = SyncStateHandle::new(SyncSnapshot { helper_state, ..SyncSnapshot::default() });
            let pool = crate::pool::TransferPool::new(crate::pool::DEFAULT_CEILING);
            let shown = state.clone();
            pool.set_observer(Arc::new(move |throughput| shown.set_throughput(throughput)));
            // The pins' downloads go through this very service, which they must
            // not keep alive: a weak reference.
            Arc::new_cyclic(|me: &std::sync::Weak<Self>| Self {
                pins: pin::Pins::new(state.clone(), me.clone(), Arc::clone(&pool)),
                pool,
                parts: source::Share::new(),
                hub: Arc::clone(hub),
                link: hub.link_cell(),
                quota: Mutex::new(match &account {
                    Some(account) => crate::quota::Quota::new(account.clone(), None),
                    None => crate::quota::Quota::detached(),
                }),
                account,
                ignore: outbox_api::configured_ignore(persist.as_ref()),
                running: Arc::new(running::Running::new(
                    persist.as_ref().and_then(|p| p.store.account(&p.account)).map(|a| running::Settings::of(&a)).unwrap_or_default(),
                )),
                pause_timer: Mutex::new(None),
                pause_shown: std::sync::atomic::AtomicU64::new(0),
                kept_back: Mutex::new(None),
                drop_at_read_only: std::sync::atomic::AtomicBool::new(false),
                switched_to_read_write: std::sync::atomic::AtomicBool::new(false),
                mode_check: Mutex::new(None),
                persist,
                report: Report::new(state.clone()),
                state,
                root: Mutex::new(None),
                lifecycle: Arc::new(tokio::sync::RwLock::new(())),
                source: Mutex::new(None),
                locks: hub.locks(),
                drive: Mutex::new(None),
                sync_paths: Mutex::new(None),
                schedule: Mutex::new(listing::Schedule::default()),
                syncing: Arc::new(Mutex::new(None)),
                store: Mutex::new(None),
                baloo: Mutex::new(Arc::new(Baloo::disabled())),
                held: Mutex::new(None),
                mode: Mutex::new(Mode::ReadOnly),
                me: me.clone(),
                tree_lock: Arc::new(tokio::sync::Mutex::new(())),
                drive_seen: Mutex::new(None),
            })
        })
    }

    /// The account's quota, which the uploads' space check reads and adjusts: the one
    /// `Account` serves (`AccountService::quota`).
    pub fn set_quota(&self, quota: crate::quota::Quota) {
        *self.quota.lock().unwrap() = quota;
    }

    /// The account's quota.
    pub fn quota(&self) -> crate::quota::Quota {
        self.quota.lock().unwrap().clone()
    }

    /// The link to the helper this account shares with the daemon's others.
    pub fn hub(&self) -> &Arc<hub::HelperHub> {
        &self.hub
    }

    /// What `HelperState` asks while there is no link (HS1), for the hub.
    pub fn set_helper_unit(&self, unit: Arc<dyn HelperUnit>) {
        self.hub.set_unit(unit);
    }

    /// `HelperState` (HS1), the hub's.
    pub fn helper_state(&self) -> String {
        self.hub.state().as_str().to_owned()
    }

    /// Works the hub's `HelperState` out again ([`hub::HelperHub::check`]).
    pub async fn check_helper(&self) {
        self.hub.check().await;
    }

    /// The drive a folder registered while signed in shows.
    /// Without one, every folder is local.
    pub fn set_drive(&self, drive: crate::drive::DriveClient) {
        *self.drive.lock().unwrap() = Some(drive.with_pool(Arc::clone(&self.pool)));
    }

    /// The account's transfer pool.
    pub fn pool(&self) -> &Arc<crate::pool::TransferPool> {
        &self.pool
    }

    /// The emergency ceiling of the account's transfer pool (`[transfers] max`) and its
    /// large-file limit (`[transfers] large`).
    pub fn set_transfer_limits(&self, ceiling: usize, large: usize) {
        self.pool.set_limits(ceiling, large);
    }

    /// What a cycle tells when the account's token reaches another drive than the folder's:
    /// the account records it and works its mode out again.
    pub fn set_drive_seen(&self, seen: listing::DriveSeen) {
        *self.drive_seen.lock().unwrap() = Some(seen);
    }

    /// What this folder's cycles ask of, or tell, the rest of the daemon.
    fn neighbours(&self) -> listing::Neighbours {
        let me = self.me.clone();
        listing::Neighbours {
            claimed: self.claims(),
            drive_seen: Arc::new(move |drive| {
                let seen = me.upgrade().and_then(|service| service.drive_seen.lock().unwrap().clone());
                if let Some(seen) = seen {
                    seen(drive);
                }
            }),
        }
    }

    /// What keeps a fresh OneDrive folder out of KDE's Baloo indexer.
    /// Without this call it is [`Baloo::disabled`], which runs no
    /// program at all: `main` installs [`Baloo::default`] (`balooctl6`);
    /// tests point this at a fake so the real indexer settings are never
    /// touched.
    pub fn set_baloo(&self, baloo: Baloo) {
        *self.baloo.lock().unwrap() = Arc::new(baloo);
    }

    /// Where a OneDrive folder's tree store, rescues and thumbnails go.
    /// Without them, every folder is local.
    pub fn set_sync_paths(&self, paths: SyncPaths) {
        *self.sync_paths.lock().unwrap() = Some(paths);
    }

    /// Puts `source` in place of the folder's content source — the VM suite's
    /// real-account scenarios wrap the Graph source to record and
    /// break fetches. Nothing in the daemon calls it.
    pub fn replace_content_source(&self, source: Arc<dyn ContentSource>) {
        *self.source.lock().unwrap() = Some(source);
    }

    /// How often a OneDrive folder is synced; takes effect at the next start
    /// of its sync.
    pub fn set_schedule(&self, schedule: listing::Schedule) {
        *self.schedule.lock().unwrap() = schedule;
    }

    /// Where the helper's socket is, for the hub. Defaults to
    /// `konedrive_proto::SOCKET_PATH`.
    pub fn set_helper_socket(&self, path: impl Into<PathBuf>) {
        self.hub.set_socket(path);
    }

    /// What a punch goes by when nothing ties it to a link of its own
    /// (local rule, on [`Clearance`]): the live link if there
    /// is one, the helper's socket if not.
    fn clearance(&self) -> Clearance {
        self.hub.clearance()
    }

    pub fn state(&self) -> &SyncStateHandle {
        &self.state
    }

    /// Where every download, free-up and reconcile reports to.
    pub fn report(&self) -> &Report {
        &self.report
    }

    /// The lock table `serve_hydrations` must share with this service, so
    /// that a real interception-driven hydration and a `dehydrate()` (or a
    /// direct `hydrate_now()`) of the same inode can never run at once.
    pub fn locks(&self) -> InodeLocks {
        self.locks.clone()
    }

    /// The live helper link, if there is one right now.
    pub fn link(&self) -> Option<HelperLink> {
        self.link.lock().unwrap().clone()
    }

    /// Publishes a new helper link, or its loss — and so
    /// `HelperState` (HS1): `connected` at once, or, on a loss, `unknown`
    /// until [`watch_helper`] has asked systemd.
    pub fn set_link(&self, link: Option<HelperLink>) {
        self.hub.set_link(link);
    }

    /// The drive `config.toml` records for this account, if it records one.
    fn account_drive(&self) -> Option<String> {
        let persist = self.persist.as_ref()?;
        persist.store.account(&persist.account).map(|a| a.drive_id).filter(|drive| !drive.is_empty())
    }

    /// The device the registered folder is on, as it was when it was
    /// registered, for the hub's router; `None` with no folder, or one that
    /// could not be looked at.
    fn root_device(&self) -> Option<u64> {
        self.registration().and_then(|reg| reg.dev)
    }

    /// Whether this account has a folder the router cannot place: one that is
    /// held back, recorded but not registered yet (a registration under way
    /// writes its folder down first), or whose device is unknown. An open in
    /// such a folder could be taken for another account's by device alone.
    fn has_unplaced_folder(&self) -> bool {
        match self.registration() {
            Some(reg) => reg.dev.is_none(),
            None => self.persisted_root().is_some(),
        }
    }

    fn registration(&self) -> Option<Registration> {
        self.root.lock().unwrap().clone()
    }

    fn require_registration(&self) -> Result<Registration, SyncError> {
        self.registration().ok_or(SyncError::NoRoot)
    }

    fn require_link(&self) -> Result<HelperLink, SyncError> {
        self.link().ok_or(SyncError::NoHelper)
    }

    pub fn root(&self) -> Option<SyncRoot> {
        self.registration().map(|r| r.root)
    }

    /// `RootState` as published ([`published_state`]).
    pub fn root_state(&self) -> String {
        published_state(&self.state.get()).to_owned()
    }

    /// `LastError` as published ([`published_error`]).
    pub fn last_error(&self) -> String {
        published_error(&self.state.get())
    }

    /// `RootSource`.
    pub fn root_source(&self) -> String {
        self.registration().map(|r| r.source.as_str().to_owned()).unwrap_or_default()
    }

    /// Binds an empty (or previously-registered) folder to the
    /// account, then runs startup recovery on it (recovery
    /// always runs *after* registration, on the same live helper link, so a
    /// file left `dehydrating` mid-`ClearIgnore` can still be cleaned up).
    ///
    /// Refused before anything is touched when nobody is signed in, or when
    /// a root is already registered (§3.1). The second of those
    /// used to be accepted: a second `register_root` returned `Ok(())` and
    /// silently replaced the root, leaving the first one registered with the
    /// helper — still marked, still walked — while `ItemState` started
    /// calling its files `not-managed`.
    ///
    /// Refused without a helper, too: no helper means no
    /// interception, and a placeholder nobody intercepts reads as zeros.
    /// [`register_root_without_interception`](Self::register_root_without_interception)
    /// is the explicit way to ask for that anyway.
    pub async fn register_root(&self, path: &Path) -> Result<(), SyncError> {
        let _lifecycle = self.lifecycle.write().await;
        self.restore_locked().await;
        self.check_held()?;
        self.require_sign_in()?;
        self.check_no_root_yet()?;
        self.require_link()?;
        let _registering = self.hub.registering.lock().await;
        self.check_overlap(path)?;
        self.bind(path, true, true).await
    }

    /// The developer's mode (HS2): `RegisterRoot`'s folder
    /// checks, root id and placeholders, and nothing intercepting anything.
    /// The folder is always local — filled with `PopulateFromDirectory`,
    /// never from OneDrive, whoever is signed in: a OneDrive folder is kept in
    /// step only with the helper (HS2). Without the helper, a file that is
    /// not downloaded reads as zeros.
    ///
    /// A separate method rather than a second argument to `RegisterRoot`:
    /// D-Bus has no optional arguments, so a flag would change the signature
    /// of a method clients already call — and, more to the point, a separate
    /// name is what an introspection dump, a `busctl` transcript and a bug
    /// report all show. Nobody reaches this mode by fumbling a boolean, and
    /// nobody reaches it by accident when the helper merely happens to be
    /// down.
    /// # Why this one does not ask whether anybody is signed in
    ///
    /// `RegisterRoot` does, because §3.1 binds a folder "to the signed-in
    /// drive". This method is the developer's, for a machine with no drive
    /// and no helper: the folder is driven from a local directory
    /// (`PopulateFromDirectory`) rather than from OneDrive. Requiring a
    /// Microsoft sign-in here would put the one path that works without the
    /// cloud behind the cloud, which is the whole thing set out
    /// to unblock. Nothing in this mode touches the account: the content
    /// comes from a directory the caller names.
    ///
    /// # Made with no helper connected, it switches when one connects
    ///
    /// The window calls this when `RegisterRoot` was refused for want of a
    /// helper ("Use Without the Helper"), and a helper installed later used
    /// to change nothing: the folder read as zeros until a Forget and a new
    /// registration. So a registration made with no helper connected is
    /// recorded as one to switch, and [`resume`](Self::resume) switches it
    /// to interception when the helper connects (`upgrade`). Made with a
    /// helper connected, the mode was chosen with interception on offer, and
    /// it stays. (A folder that shows OneDrive left without interception by
    /// a daemon from before HS2 switches whatever it was recorded as.)
    pub async fn register_root_without_interception(
        &self,
        path: &Path,
    ) -> Result<(), SyncError> {
        let _lifecycle = self.lifecycle.write().await;
        self.restore_locked().await;
        self.check_held()?;
        self.check_no_root_yet()?;
        let _registering = self.hub.registering.lock().await;
        self.check_overlap(path)?;
        self.bind(path, false, true).await
    }

    /// Holds this account's folder back (design §3.1): `config.toml` gives
    /// the account an id, a label, a drive or a folder an earlier account
    /// has. The folder is not brought up, `RootState` reads `error`,
    /// `LastError` says why, and a registration is refused the same way.
    pub fn hold_back(&self, why: &str) {
        let message = format!("this account is held back: {why}; correct config.toml and start konedrived again");
        tracing::warn!("{message}");
        let path = self.persisted_root().map(|p| p.path.display().to_string()).unwrap_or_default();
        *self.held.lock().unwrap() = Some(message.clone());
        self.state.update(|s| {
            s.root_path = path;
            s.root_state = RootState::Error;
            s.last_error = message;
        });
    }

    fn check_held(&self) -> Result<(), SyncError> {
        match self.held.lock().unwrap().clone() {
            Some(why) => Err(SyncError::Io(why)),
            None => Ok(()),
        }
    }

    /// Says again why the account is held back, once its folder is
    /// forgotten: a Forget clears everything published about the folder.
    fn publish_held(&self) {
        if let Some(message) = self.held.lock().unwrap().clone() {
            self.state.update(|s| {
                s.root_state = RootState::Error;
                s.last_error = message;
            });
        }
    }

    /// No registration, bring-up or switch for this account from now on
    /// (`Accounts.Remove`). Called with `lifecycle` held for writing.
    fn retire_locked(&self) {
        *self.held.lock().unwrap() = Some("this account is being removed".into());
    }

    /// The folder a held-back account records, as a registration to forget
    /// through: its folder is never brought up (§3.1), but one registered with
    /// interception in an earlier session is still the helper's until the
    /// helper lets go of it. `None` for an account not held back, or one that
    /// records no folder. The root id comes from `config.toml` or, for a
    /// config written before the id was recorded, from the folder, as
    /// [`hold`](Self::hold) finds it; an intercepted folder with neither
    /// cannot be named to the helper, and is refused.
    async fn recorded_for_forget(&self) -> Result<Option<Registration>, SyncError> {
        if self.held.lock().unwrap().is_none() {
            return Ok(None);
        }
        let Some(persisted) = self.persisted_root() else { return Ok(None) };
        let root_id = if root::looks_like_a_root_id(&persisted.root_id) {
            persisted.root_id.clone()
        } else if let Some(root_id) = root::recorded_root_id(&persisted.path).await {
            root_id
        } else if !persisted.intercepted {
            persisted.root_id.clone()
        } else {
            return Err(SyncError::Io(format!(
                "cannot forget {}: config.toml does not record its root id, and the folder carries \
                 none that can be read",
                persisted.path.display()
            )));
        };
        Ok(Some(Registration {
            dev: None,
            root: SyncRoot { path: persisted.path, root_id },
            intercepted: persisted.intercepted,
            recovery_deferred: false,
            source: persisted.source,
            brought_up: false,
            baloo_excluded: persisted.baloo_excluded,
            upgrade_when_helper: false,
        }))
    }

    /// Design §8.3: a folder that is, is inside, or contains another
    /// account's folder is refused, naming that account. Called with the
    /// hub's `registering` held, so that two accounts cannot both pass it.
    /// The helper would refuse an intercepted overlap anyway (`EINVAL`);
    /// checking first names the refusal, and covers a folder registered
    /// without interception, which the helper never sees.
    fn check_overlap(&self, path: &Path) -> Result<(), SyncError> {
        match self.hub.overlapping(self, path) {
            Some(label) => Err(SyncError::Overlaps(label)),
            None => Ok(()),
        }
    }

    /// The account's label, as `config.toml` has it — for a refusal that
    /// names it.
    fn label(&self) -> String {
        self.persist
            .as_ref()
            .and_then(|persist| persist.store.account(&persist.account))
            .map(|account| account.label)
            .unwrap_or_else(|| "another account".into())
    }

    /// Every folder this account holds or records: the registered one, and
    /// the one `config.toml` names (held, or not brought up yet).
    fn folders(&self) -> Vec<PathBuf> {
        let mut folders: Vec<PathBuf> = self.registration().map(|reg| reg.root.path).into_iter().collect();
        folders.extend(self.persisted_root().map(|p| p.path));
        folders
    }

    /// §3.1: a root is bound to the signed-in drive, so there has to be one.
    fn require_sign_in(&self) -> Result<(), SyncError> {
        match &self.account {
            Some(account) if account.get().state != SignInState::SignedIn => {
                Err(SyncError::NotSignedIn)
            }
            _ => Ok(()),
        }
    }

    /// §3.1's refusal on overlap, which holds whichever way a root is
    /// registered: this daemon keeps exactly one.
    fn check_no_root_yet(&self) -> Result<(), SyncError> {
        if self.registration().is_some() {
            return Err(SyncError::AlreadyRegistered);
        }
        Ok(())
    }

    /// A folder registered while signed in, with a drive
    /// configured, and with interception, shows OneDrive; any other is
    /// local. Asked only of a new registration — a folder brought back keeps
    /// what `config.toml` records, whoever is signed in by then.
    ///
    /// "With interception" is HS2's: a registration without it is always
    /// the developer's local folder, filled with `PopulateFromDirectory`. A
    /// OneDrive folder nobody intercepts would read as zeros wherever a file
    /// is not downloaded, and is never made any more.
    fn fresh_source(&self, intercepted: bool) -> RootSource {
        let signed_in = self.account.as_ref().is_some_and(|a| a.get().state == SignInState::SignedIn);
        let configured = self.drive.lock().unwrap().is_some() && self.sync_paths.lock().unwrap().is_some();
        if signed_in && configured && intercepted {
            RootSource::OneDrive
        } else {
            RootSource::Local
        }
    }

    /// Registers `path`, recovers it, and publishes the result — the half
    /// shared by a `RegisterRoot` call, a `RegisterWithoutInterception`
    /// call, and a root brought back up at startup or after the helper
    /// reconnected. `fresh` is true for the first two: a registration the
    /// daemon did not hold before this call.
    ///
    /// # Nothing is committed until nothing can still fail
    ///
    /// The root is stored and published only once registration *and*
    /// recovery are done. The version this replaces stored the root and
    /// published `RootPath`/`RootState = ready` first and then returned
    /// `Err` on a `RecoveryError` — a call that failed, having already
    /// committed. With `RegisterRoot` now refusing a second root, that would
    /// be worse than untidy: the failed call would leave a root behind that
    /// makes every retry answer "already registered".
    ///
    /// A per-file recovery failure (`report.failed > 0`) does *not* fail
    /// this call — the root itself is registered and usable — but it does
    /// flip `RootState` to `error` and fill `LastError` with what could not
    /// be fixed: a refused `ClearIgnore` leaves a file in the state
    /// calls silently unrecoverable, so it must not stay silent here.
    ///
    /// # Whatever the helper holds, the daemon holds (link 2)
    ///
    /// A folder the helper holds and the daemon does not is a folder the
    /// daemon will accept for `RegisterWithoutInterception` — and then
    /// free up files in with no `ClearIgnore`, while the helper still has
    /// them ignore-marked. So a fresh intercepted registration:
    ///
    /// - is written to `config.toml` **before** the helper is told, and is
    ///   refused outright if it cannot be. A crash anywhere after that — in
    ///   the helper's walk, in recovery's — leaves a root the next start
    ///   restores as intercepted and holds until the helper is back, which is
    ///   the direction to fail in. The version this replaces wrote it last,
    ///   after recovery had walked the whole tree, and H70's own comment
    ///   notes that a helper timeout could already leave the helper holding
    ///   a root the daemon could not see;
    /// - is undone *at the helper* when it fails after the helper may have
    ///   saved it, and is kept instead when the helper cannot confirm it let
    ///   go ([`abandon`](Self::abandon)).
    ///
    /// A root brought back up is already the daemon's, in memory and on
    /// disk, and a failure leaves it exactly as it was.
    ///
    /// A root registered without interception is never announced to the
    /// helper: not registered, not marked, not
    /// unregistered. What its recovery may still ask is `ClearIgnore`, by the
    /// same local rule every punch follows.
    ///
    /// # What the folder shows
    ///
    /// A new registration shows OneDrive when it is made signed in with a
    /// drive configured ([`fresh_source`](Self::fresh_source)); a folder
    /// brought back shows what it showed before — the registration held, or
    /// else what `config.toml` records — whoever is signed in by then.
    async fn bind(&self, path: &Path, intercepted: bool, fresh: bool) -> Result<(), SyncError> {
        // A OneDrive folder remembers its account's drive (design §8.3):
        // one forgotten by another account is that account's files, and a
        // folder carrying a root id may be registered again without being
        // empty — so it is refused unless the drive is this account's, or
        // the folder is empty: then there is nothing to adopt, and the stale
        // drive comes off (Remove, then Add, on the same folder).
        if fresh && !root::drive_allows(path, self.account_drive()).await {
            return Err(SyncError::ForeignFolder);
        }
        let source = if fresh {
            self.fresh_source(intercepted)
        } else {
            self.registration()
                .map(|reg| reg.source)
                .or_else(|| self.persisted_root().map(|p| p.source))
                .unwrap_or(RootSource::Local)
        };
        if fresh && source == RootSource::OneDrive {
            // A tree store left by a folder forgotten earlier describes
            // another folder.
            self.remove_tree_store().await;
        }

        if !intercepted {
            // A new registration without interception made with no
            // helper connected is the window's fallback, and switches to
            // interception when one connects; made with one connected, it is
            // a choice, and stays. A folder brought back keeps what it had.
            let upgrade_when_helper = if fresh {
                self.link().is_none()
            } else {
                self.registration()
                    .map(|reg| reg.upgrade_when_helper)
                    .or_else(|| self.persisted_root().map(|p| p.upgrade_when_helper))
                    .unwrap_or(false)
            };
            let root = root::register_root_unprotected(path).await?;
            let recovery = root::recover(&self.clearance(), &root, &self.locks).await;
            let report = self.recovered(&root, recovery)?;
            self.commit(root, false, source, fresh, upgrade_when_helper, report).await;
            return Ok(());
        }

        let link = self.require_link()?;
        let (dir, root) = root::prepare(path).await?;
        let previous = if fresh {
            let previous = self.persisted_root();
            // Baloo is not checked yet at this point (it runs, at most, once
            // `commit` below has recovery's word that the registration
            // stuck); `commit`'s own `remember` corrects this the moment it
            // knows.
            self.save_root(Some(&Persisted::of(&root, true, source, false, false))).map_err(SyncError::Io)?;
            Some(previous)
        } else {
            None
        };
        let registered = link
            .register_root(&dir, &root.root_id)
            .await
            .map_err(|e| SyncError::from(RegisterError::Helper(e.to_string())));
        drop(dir);
        let outcome = match registered {
            Ok(()) => {
                let clearance = Clearance::Link(link.clone());
                self.recovered(&root, root::recover(&clearance, &root, &self.locks).await)
            }
            Err(e) => Err(e),
        };
        match outcome {
            Ok(report) => {
                self.commit(root, true, source, fresh, false, report).await;
                Ok(())
            }
            Err(error) => {
                if let Some(previous) = previous {
                    self.abandon(&link, root, source, previous, &error).await;
                }
                Err(error)
            }
        }
    }

    /// Recovery's report, or — when recovery could not run at all — its
    /// error, published before it is returned.
    fn recovered(
        &self,
        root: &SyncRoot,
        recovery: Result<RecoveryReport, RecoveryError>,
    ) -> Result<RecoveryReport, SyncError> {
        recovery.map_err(|e| {
            let message = e.to_string();
            tracing::error!("startup recovery on {}: {message}", root.path.display());
            self.state.update(|s| {
                s.root_state = RootState::Error;
                s.last_error = message.clone();
            });
            SyncError::Io(message)
        })
    }

    /// Stores, records and publishes a registration that has been made and
    /// recovered, and starts — or nudges — a OneDrive folder's sync.
    async fn commit(
        &self,
        root: SyncRoot,
        intercepted: bool,
        source: RootSource,
        fresh: bool,
        upgrade_when_helper: bool,
        report: RecoveryReport,
    ) {
        let mut trouble = None;
        if report.failed > 0 {
            trouble = Some(format!(
                "startup recovery could not reset {} of {} managed file(s); they are left \
                 exactly as found for the next start (reset {}, skipped {})",
                report.failed, report.scanned, report.reset, report.skipped
            ));
            tracing::error!("{}", trouble.as_deref().unwrap_or_default());
        } else if report.skipped > 0 {
            trouble = Some(format!(
                "startup recovery could not inspect {} item(s); the root may not be fully \
                 recovered",
                report.skipped
            ));
            tracing::warn!("{}", trouble.as_deref().unwrap_or_default());
        } else if report.busy > 0 {
            // Not an error: what has such a file
            // open is, as often as not, the very open that will fill it. So
            // it is logged and not said in `LastError` (B-M11): said there,
            // it stayed for the whole session, long after the file was
            // filled.
            tracing::info!(
                "startup recovery left {} interrupted file(s) as they were because they were in \
                 use; each is filled when it is next opened, or reset at the next start",
                report.busy
            );
        } else if report.deferred > 0 {
            // Not an error either: recovery runs again the
            // moment the link is up.
            trouble = Some(format!(
                "startup recovery left {} interrupted file(s) as they were: a konedrive helper \
                 is running and this daemon is not connected to it yet; they are reset once it is",
                report.deferred
            ));
            tracing::info!("{}", trouble.as_deref().unwrap_or_default());
        }

        // A OneDrive folder is kept out of Baloo, unless it — or
        // a directory above it — is excluded already, in which case nothing
        // is added and nothing this daemon did not add is ever taken off
        // (`unregister_root`). A folder brought back up that `config.toml`
        // records as excluded by this daemon carries that forward, so a
        // restart between a registration and its Forget still gets the
        // Forget right. Any other is asked again at every commit, fresh or
        // not: the exclusion of a fresh folder can
        // have failed or timed out, been cut off by a kill before it was
        // recorded, or been refused by a registration kept after it failed
        // (`abandon`); or Baloo came later. It only ever adds.
        let baloo_excluded = if source != RootSource::OneDrive {
            false
        } else if !fresh && self.persisted_root().is_some_and(|p| p.baloo_excluded) {
            true
        } else {
            let baloo = Arc::clone(&self.baloo.lock().unwrap());
            if baloo.is_excluded(&root.path).await {
                false
            } else {
                baloo.exclude(&root.path).await
            }
        };

        // The folder remembers its account's drive once the drive is known
        // (design §8.3): from its registration on, and at the first
        // bring-up of a folder from before multiple accounts.
        if source == RootSource::OneDrive {
            if let Some(drive) = self.account_drive() {
                let (marked, shown) = (root.clone(), root.path.display().to_string());
                match tokio::task::spawn_blocking(move || root::mark_drive(&marked, &drive)).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => tracing::warn!("cannot record the drive on {shown}: {e}"),
                    Err(e) => tracing::warn!("the task recording the drive on {shown} failed: {e}"),
                }
            }
        }

        // `config.toml` names what is registered now: a fresh root without
        // interception is written down here (a fresh intercepted one already
        // was, before the helper heard of it), and a root brought back up
        // under an id other than the recorded one is corrected.
        self.remember(&Persisted::of(&root, intercepted, source, baloo_excluded, upgrade_when_helper));
        let path = root.path.display().to_string();
        let recovery_deferred = report.deferred > 0;
        let dev = hub::device_of(&root.path);
        *self.root.lock().unwrap() = Some(Registration {
            root,
            intercepted,
            recovery_deferred,
            source,
            brought_up: true,
            baloo_excluded,
            upgrade_when_helper,
            dev,
        });
        self.state.update(|s| {
            s.root_path = path;
            // A recovery that could not reset a file outranks the mode in
            // `RootState`, because it is the louder of the two problems;
            // `LastError` below still carries the no-interception warning.
            s.root_state = if report.failed > 0 {
                RootState::Error
            } else if intercepted {
                RootState::Ready
            } else {
                RootState::NoInterception
            };
            s.last_error = match (intercepted, &trouble) {
                (true, None) => String::new(),
                (true, Some(trouble)) => trouble.clone(),
                (false, None) => NO_INTERCEPTION_WARNING.to_owned(),
                (false, Some(trouble)) => format!("{NO_INTERCEPTION_WARNING}. {trouble}"),
            };
            // HS2: a folder that shows OneDrive is kept in step only with
            // interception; one registered without it before HS waits for
            // the helper, and switches when it connects (`resume`).
            s.waits_for_helper = !intercepted && source == RootSource::OneDrive;
        });
        // `LocalBytes` for the folder now registered.
        self.report.space.kick();
        if source == RootSource::OneDrive && intercepted {
            self.start_sync().await;
        } else {
            // The sweep at start. A OneDrive folder's sync sweeps after its
            // first reconcile, which is Full; any other folder is swept here,
            // in the background: the walk must not hold up the registration.
            let (pins, root) = (Arc::clone(&self.pins), PathBuf::from(&self.state.get().root_path));
            tokio::spawn(async move {
                pins.sweep(root).await;
            });
        }
    }

    /// Undoes a fresh intercepted registration that failed after the helper
    /// may have saved it: the helper is told to let go, and `config.toml` is
    /// put back the way it was (on both sides).
    ///
    /// If the helper cannot confirm it let go — the link dropped, the call
    /// timed out, anything but an answer — the root is kept instead, as
    /// intercepted and published as an error. A folder the helper may still
    /// hold must never be one the daemon holds nothing of: the next thing it
    /// would accept for that folder is a registration without interception.
    /// It leaves the way every intercepted root leaves, through the helper
    ///, and a retry is answered "already registered" until it
    /// has.
    ///
    /// `EPERM` counts as having let go: the helper answers it when it holds
    /// no root of this uid under that id, which is what a registration it
    /// refused leaves behind.
    ///
    /// A root kept is kept with the `source` `config.toml` now records for
    /// it, and with no sync: that starts when the root is brought up.
    async fn abandon(
        &self,
        link: &HelperLink,
        root: SyncRoot,
        source: RootSource,
        previous: Option<Persisted>,
        why: &SyncError,
    ) {
        match link.unregister_root(&root.root_id).await {
            Ok(()) | Err(HelperError::Refused(libc::EPERM)) => {
                self.persist_or_log(previous.as_ref());
            }
            Err(e) => {
                let message = format!(
                    "registering {} failed ({why}), and the helper could not be told to let go \
                     of it ({e}); it stays registered, with interception, until a Forget \
                     reaches the helper",
                    root.path.display()
                );
                tracing::error!("{message}");
                let path = root.path.display().to_string();
                let dev = hub::device_of(&root.path);
                *self.root.lock().unwrap() = Some(Registration {
                    root,
                    intercepted: true,
                    recovery_deferred: false,
                    source,
                    brought_up: false,
                    baloo_excluded: false,
                    upgrade_when_helper: false,
                    dev,
                });
                self.state.update(|s| {
                    s.root_path = path;
                    s.root_state = RootState::Error;
                    s.last_error = message;
                });
            }
        }
    }

    /// Forgets the root and clears the published state. The files themselves
    /// are left exactly as they are.
    ///
    /// # An intercepted root is forgotten through the helper, or not at all
    ///
    /// The helper's `UnregisterRoot` is the one thing that takes an
    /// intercepted root's marks off — its directory marks, and the ignore
    /// mark on every hydrated file in it. This used to tell the helper only
    /// *if* a link happened to be up, and forget the root either way: the
    /// helper kept the registration, the marks and the ignore marks, the
    /// orphan never cleared (`resume` has nothing to renew for a root the
    /// daemon no longer holds), and a folder registered again without
    /// interception then had its files freed up with no `ClearIgnore` while
    /// they were still ignored — measured in the VM suite: a reader got
    /// 65536 zero bytes and nothing was fetched. So with no link this
    /// refuses `NoHelper`, exactly as `RegisterRoot` does, and nothing
    /// changes.
    ///
    /// The helper answering `EPERM` is not a refusal to forget: it holds no
    /// root of this uid under that id — it lost it, or never kept it — so no
    /// mark of that registration is left to take off, and keeping the root
    /// would only make it impossible to forget. Any other failure keeps it,
    /// because then the helper may well still hold it.
    ///
    /// # A root registered without interception never involves the helper
    ///
    /// It was never announced to the helper, so there is nothing to tell it.
    /// Telling it anyway made such a root impossible to forget while a helper
    /// was connected: the helper refuses `EPERM` to unregister a root the uid
    /// does not hold, and the daemon kept the registration (measured).
    ///
    /// # A OneDrive folder
    ///
    /// Its sync is stopped first, the read-only lock is taken off the folder
    /// once it is forgotten, and its tree store is dropped. A Forget that is
    /// refused leaves it registered — so it is kept in step again.
    ///
    /// The sync is stopped before `lifecycle` is taken for writing: a
    /// reconcile holds it for reading while it changes the folder, and checks
    /// for a stop between its steps, so stopped first it lets go at its next
    /// step rather than at the end of the whole reconcile. A helper's
    /// reconnect that takes the lock in between brings the folder up again,
    /// and so starts its sync again; that one is stopped under the lock, where
    /// stopping cannot wait for a reconcile — none can hold the lock.
    pub async fn unregister_root(&self) -> Result<(), SyncError> {
        self.forget(false).await
    }

    /// `Accounts.Remove`'s first step: the folder forgotten exactly as
    /// [`unregister_root`](Self::unregister_root) forgets it — refused under
    /// the same rule — and, under the same `lifecycle` lock so that nothing
    /// comes in between, the account retired: no registration, bring-up or
    /// switch is made for it from then on. An account with no folder is
    /// retired all the same.
    pub async fn retire(&self) -> Result<(), SyncError> {
        self.forget(true).await
    }

    /// [`unregister_root`](Self::unregister_root) and [`retire`](Self::retire).
    ///
    /// Refused `PendingUploads` while changes wait to be uploaded: the tree
    /// store that holds them goes with the folder. Asked before anything changes — the watcher
    /// hands over what it holds first — and again once the sync has stopped.
    async fn forget(&self, retire: bool) -> Result<(), SyncError> {
        // Without the lifecycle lock: a reconcile, or a switch waiting for it, must not keep
        // the Forget from stopping the sync first.
        self.flush_watcher().await;
        refuse_waiting(self.changes_in_store().await?)?;
        // The tasks only, outside the lock; the activity is let go of under
        // it (B-M1), where no reconnect can have started a sync meanwhile.
        let was_syncing = self.stop_tasks().await;
        let _lifecycle = self.lifecycle.write().await;
        let was_syncing = self.stop_tasks().await || was_syncing;
        if was_syncing {
            self.let_go_of_activity().await;
        }
        if let Err(refused) = self.changes_in_store().await.and_then(refuse_waiting) {
            if was_syncing {
                self.start_sync().await;
            }
            return Err(refused);
        }
        self.restore_locked().await;
        // A held-back account never brings its folder up, but a folder the
        // helper may still hold leaves through the helper all the same: its
        // record is the only name the helper holds it by.
        let (reg, recorded) = match self.registration() {
            Some(reg) => (reg, false),
            None => match self.recorded_for_forget().await? {
                Some(reg) => (reg, true),
                None if retire => {
                    self.retire_locked();
                    return Ok(());
                }
                None => return Err(SyncError::NoRoot),
            },
        };
        // A move out of the folder still in its store goes with it (the count above leaves none):
        // what it left outside is tidied first, while the helper still holds the folder, and the
        // hub stops routing its ids.
        if reg.source == RootSource::OneDrive {
            self.drop_moved_out(&reg.root).await;
        }
        let result = self.forget_locked(&reg).await;
        if result.is_ok() {
            if retire {
                self.retire_locked();
            } else if recorded {
                self.publish_held();
            }
        }
        if reg.source == RootSource::OneDrive {
            match &result {
                Ok(()) => {
                    self.let_go_of_onedrive(&reg.root).await;
                    // Only when *this daemon* is the one that
                    // excluded the folder from Baloo — never a folder that
                    // arrived already excluded, and this survives a restart
                    // in between (`commit` carries `baloo_excluded` forward
                    // from `config.toml` for a root that is brought back up,
                    // not freshly registered).
                    if reg.baloo_excluded {
                        let baloo = Arc::clone(&self.baloo.lock().unwrap());
                        baloo.include_again(&reg.root.path).await;
                    }
                }
                Err(_) if was_syncing => self.start_sync().await,
                Err(_) => {}
            }
        }
        result
    }

    /// The Forget itself, under `lifecycle` held for writing: through the
    /// helper for an intercepted root, then everything published about the
    /// root and its sync cleared at once.
    async fn forget_locked(&self, reg: &Registration) -> Result<(), SyncError> {
        if reg.intercepted {
            let link = self.require_link()?;
            match link.unregister_root(&reg.root.root_id).await {
                Ok(()) => {}
                Err(HelperError::Refused(libc::EPERM)) => tracing::warn!(
                    "the helper holds no root {} for {}; forgetting it here",
                    reg.root.root_id,
                    reg.root.path.display()
                ),
                Err(e) => return Err(SyncError::Io(e.to_string())),
            }
        }
        *self.root.lock().unwrap() = None;
        *self.source.lock().unwrap() = None;
        self.persist_or_log(None);
        // One update, so that nothing is ever published about a folder that
        // is no longer registered.
        self.state.update(|s| {
            s.root_path.clear();
            s.root_state = RootState::None;
            s.last_error.clear();
            s.listing = false;
            s.items_listed = 0;
            s.items_placed = 0;
            s.skipped_count = 0;
            s.sync_trouble = None;
            s.replacement_note.clear();
            s.outbox_note.clear();
            s.last_checked = 0;
            s.local_bytes = 0;
            s.conflict_count = 0;
            s.waits_for_helper = false;
        });
        // The pins stay on the files; what they queued is dropped, and
        // `PinnedCount` reads 0.
        self.pins.clear();
        // A Forget drops the activity: a OneDrive folder's with
        // its store, which `stop_sync` has already let go of, and a local
        // folder's from memory here — after the folder stopped being the one
        // registered, so that a download still ending in it records nothing
        // from now on. Its walker stops too.
        self.report.space.stop();
        let report = self.report.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || report.activity.detach()).await {
            tracing::warn!("the task forgetting the activity failed: {e}");
        }
        Ok(())
    }

    /// What a Forget adds for a OneDrive folder, still under
    /// `lifecycle` held for writing: the read-only lock taken off the whole
    /// folder — whose files stay — and its tree store dropped.
    /// The folder still carries its root id (a Forget leaves it), which is
    /// what proves it is still this folder before anything in it is changed.
    async fn let_go_of_onedrive(&self, root: &SyncRoot) {
        let root = root.clone();
        let unlocked = tokio::task::spawn_blocking(move || {
            disk::Disk::open(&root, false)
                .and_then(|disk| disk.unlock_tree())
                .map_err(|e| format!("cannot take the read-only lock off {}: {e}", root.path.display()))
        })
        .await
        .unwrap_or_else(|e| Err(format!("the unlock task failed: {e}")));
        if let Err(e) = unlocked {
            tracing::warn!("{e}");
        }
        self.remove_tree_store().await;
    }

    /// Starts — or, when it runs already, nudges — the sync of the OneDrive
    /// folder that is registered now. Called with `lifecycle` held for
    /// writing, so that no Forget can come in between.
    async fn start_sync(&self) {
        if let Some(syncing) = self.syncing.lock().unwrap().as_ref() {
            syncing.poller.refresh();
            return;
        }
        let Some(reg) = self.registration() else { return };
        let configured = (self.drive.lock().unwrap().clone(), self.sync_paths.lock().unwrap().clone());
        let (Some(drive), Some(paths)) = configured else {
            let text = format!("{} shows OneDrive, but no drive is configured; it is not kept in step", reg.root.path.display());
            return self.cannot_start(&reg.root, text).await;
        };
        // The lock as the mode wants it (`docs/design/writes.md` §2.2): a walk a switch did not finish —
        // the daemon stopped, or the folder was not up — is finished here. A read-only folder
        // is locked before anything else, never left writable until a Full reconcile, which
        // needs Graph. A read-write one is unlocked below, once its watcher has
        // marked every directory, and before this sync's first cycle can change the folder:
        // the cycle takes `lifecycle`, which the caller holds (the watcher).
        let mut writable = self.mode() == Mode::ReadWrite;
        if !writable {
            self.ensure_locked(&reg.root).await;
        }
        // Files are downloaded from the drive whether or not the folder can
        // be kept in step.
        let source: Arc<dyn ContentSource> = Arc::new(graph_source::GraphSource::new(drive.clone()));
        *self.source.lock().unwrap() = Some(Arc::clone(&source));
        let tree_db = paths.tree_db.clone();
        let store = match tokio::task::spawn_blocking(move || crate::tree::TreeStore::open(&tree_db)).await {
            Ok(Ok(store)) => crate::tree::Store::new(store),
            Ok(Err(e)) => return self.cannot_start(&reg.root, format!("the tree store cannot be opened: {e}")).await,
            Err(e) => return self.cannot_start(&reg.root, format!("the tree store cannot be opened: {e}")).await,
        };
        // A read-only folder that still holds changes waiting to upload (a switch nobody
        // forced) runs no cycle while they wait: what a read-write cycle
        // deferred stays deferred for the read-write cycle that sends them.
        let waiting = !writable && store.call(|s| s.outbox_len()).await.map_or(true, |n| n > 0);
        if !writable && !waiting {
            // Changes a read-write cycle deferred are the base's now: a read-only cycle knows
            // none. Nothing at all for a folder that never was read-write.
            match store.call(|s| s.apply_deferred()).await {
                Ok(0) => {}
                Ok(n) => tracing::info!("{n} change(s) from OneDrive that waited for local changes are applied now"),
                Err(e) => tracing::warn!("cannot apply the changes from OneDrive that waited: {e}"),
            }
        }
        // The activity log and the conflicts are kept in this
        // store from now on, and `LastChecked` is where the last run left it.
        // Every caller holds `lifecycle` for writing, so no other start can
        // attach a store of its own meanwhile.
        let (report, attached, folder) = (self.report.clone(), store.clone(), reg.root.path.clone());
        let last_checked = tokio::task::spawn_blocking(move || {
            report.activity.attach(attached.clone(), &folder);
            attached.call_blocking(move |s| s.meta("last_checked")).ok().flatten().and_then(|v| v.parse::<i64>().ok())
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(0);
        self.state.update(|s| s.last_checked = last_checked);
        // Local changes are looked for in a read-write folder only (`docs/design/writes.md` §3.1): from
        // now on by the watcher, and once in full, for what changed while nothing watched —
        // at every bring-up, and after a switch to read-write; the watcher's walk ends in that
        // Full local scan. Started before the sync is published, and kept in it: whoever
        // stops the sync stops it. A read-write folder whose watcher cannot start
        // runs this sync locked, as a read-only one: no directory is ever made in it unwatched
        // (the watcher).
        // Its first examination is the Full local scan, which the folder's first delta cycle
        // waits for (`docs/design/writes.md` §3).
        let (scanned, first_scan) = tokio::sync::watch::channel(false);
        let watcher = if writable { self.start_watcher_scanned(&reg.root, &store, Some(scanned)) } else { None };
        if writable && watcher.is_none() {
            writable = false;
            self.ensure_locked(&reg.root).await;
        }
        // The account's drive, as `config.toml` keeps it (A-M5, design §8.1):
        // the same-account check then survives a tree store rebuilt empty.
        let drive_record = self.persist.clone().map(|persist| {
            let recorded = persist.store.account(&persist.account).map(|a| a.drive_id).filter(|d| !d.is_empty());
            listing::DriveRecord { store: persist.store, account: persist.account, recorded }
        });
        // Nudges the thumbnail filler right after a cycle, rather than making
        // it wait out its own idle timer.
        let kick = Arc::new(Notify::new());
        // A read-write folder's cycle shares the tree lock with its outbox worker, and its
        // first one waits for the watcher's Full local scan (`docs/design/writes.md` §3, §9).
        let writes = writable.then(|| self.cycle_writes(Some(first_scan)));
        let listing = listing::Listing::new(listing::ListingContext {
            root: reg.root.clone(),
            intercepted: reg.intercepted,
            store: store.clone(),
            drive: drive.clone(),
            drive_record,
            source,
            link: Arc::clone(&self.link),
            locks: self.locks.clone(),
            state: self.state.clone(),
            lifecycle: Arc::clone(&self.lifecycle),
            rescue_dir: paths.rescue_dir.clone(),
            full_threshold: listing::FULL_THRESHOLD,
            after_cycle: Some(Arc::clone(&kick)),
            report: self.report.clone(),
            pins: Arc::clone(&self.pins),
            locked: !writable,
            writes,
            // Another account's objects are never removed here, and the
            // account hears of a drive that is not the folder's (m2).
            neighbours: Some(self.neighbours()),
            running: Arc::clone(&self.running),
        });
        let schedule = self.schedule.lock().unwrap().clone();
        // Checked again and kept in one critical section: a second start that
        // passed the check at the top while the store opened must leave the
        // first sync alone. Replacing it would drop a `Poller` that runs on
        // with nothing left to stop it; the lifecycle lock every caller holds
        // is what keeps two starts apart, not this.
        let published = {
            let mut syncing = self.syncing.lock().unwrap();
            if syncing.is_some() {
                Err(watcher)
            } else {
                *self.store.lock().unwrap() = Some(store.clone());
                let poller = listing::Poller::start(listing, schedule);
                let sign_in_watch = self
                    .account
                    .as_ref()
                    .map(|account| tokio::spawn(nudge_on_sign_in(account.subscribe(), Arc::clone(&self.syncing))));
                // The outbox worker: it sends the rows the watcher's
                // examination records, and looks at those already there as it
                // starts.
                let outbox = if writable { self.start_outbox(&reg.root, &store, &drive) } else { None };
                // Its own task, stopped with the poller: a slow thumbnail request
                // never holds up the reconcile. None at all without a cache to fill.
                let thumbnails = paths.thumbnails.clone().map(|cache| {
                    let cancel = CancellationToken::new();
                    let task = thumbs::ThumbnailFiller::new(drive, store, reg.root.clone(), cache, Arc::clone(&self.running))
                        .spawn(kick, cancel.clone());
                    (task, cancel)
                });
                let walked = watcher.as_ref().map(write_mode::Watcher::walked);
                *syncing = Some(Syncing { poller, sign_in_watch, thumbnails, watcher, outbox });
                Ok(walked)
            }
        };
        // `Paused` as the store keeps it, and a timer for a pause that ends.
        self.show_pause();
        match published {
            // No directory is made in the folder before it is watched (write design Z2).
            Ok(Some(walked)) => self.ensure_unlocked(&reg.root, walked).await,
            Ok(None) => {}
            // Another start won: this one's watcher goes.
            Err(watcher) => {
                if let Some(watcher) = watcher {
                    self.stop_watcher(watcher).await;
                }
            }
        }
    }

    /// Why a OneDrive folder is not kept in step, said as blocking trouble:
    /// nothing retries it on its own; a `Refresh()` does, as
    /// does bringing the folder up again.
    /// [`sync_cannot_start`](Self::sync_cannot_start), and the lock put back on
    /// the folder, whatever its mode: with no sync, no watcher looks at it, so
    /// a read-write folder a run left unlocked must not stay so (the watcher re-review
    /// R2-1), and a read-only one never is.
    async fn cannot_start(&self, root: &SyncRoot, text: String) {
        self.sync_cannot_start(text);
        self.ensure_locked(root).await;
    }

    fn sync_cannot_start(&self, text: String) {
        tracing::error!("{text}");
        self.state.update(|s| s.sync_trouble = Some(SyncTrouble { text, blocking: true }));
    }

    /// The daemon is stopping (issue #84): the outbox worker, if one runs,
    /// takes nothing more and lets the requests in flight return. The future
    /// ends when it has; the caller bounds the wait (`crate::stop`).
    pub fn close_outbox(&self) -> Option<impl std::future::Future<Output = ()> + Send + 'static> {
        self.syncing.lock().unwrap().as_ref().and_then(|s| s.outbox.as_ref()).map(|outbox| outbox.close())
    }

    /// Stops the sync and waits for it: a Forget's, and tests'. (At the
    /// daemon's stop only the outbox is wound down, [`close_outbox`]; the
    /// rest ends with the process.)
    ///
    /// [`close_outbox`]: Self::close_outbox
    /// Whether one was running. Once this returns, no clone of the tree store
    /// is left with the sync (see `store`).
    pub async fn stop_sync(&self) -> bool {
        let stopped = self.stop_tasks().await;
        if stopped {
            self.let_go_of_activity().await;
        }
        stopped
    }

    /// The first half of [`stop_sync`](Self::stop_sync): the poller, the
    /// sign-in watch and the thumbnail filler, stopped and waited for.
    /// Safe without the lifecycle lock (a Forget's first stop).
    async fn stop_tasks(&self) -> bool {
        let syncing = self.syncing.lock().unwrap().take();
        let Some(syncing) = syncing else { return false };
        // Told before the poller is waited for, so that both wind down at
        // once.
        if let Some((_, cancel)) = &syncing.thumbnails {
            cancel.cancel();
        }
        // The poller first (the read-write reconcile): a cycle may hold the tree lock while it waits for
        // `lifecycle`, which the caller may hold for writing, and the watcher's examination
        // waits for that tree lock — stopping the poller ends that cycle, and its lock with it.
        syncing.poller.stop().await;
        // Its outbox worker next: a request under way is cut off, and its row replayed when
        // the worker starts again (`docs/design/writes.md` §10) — and a commit it holds the tree lock for,
        // waiting on a file a fill holds, does not keep the examination below waiting for as
        // long as that download (the read-write reconcile).
        if let Some(outbox) = syncing.outbox {
            outbox.stop().await;
            // Its counts go with it (the outbox on the bus); the next worker counts again.
            self.clear_outbox_counts();
        }
        // A read-write folder's watcher goes with its sync (`docs/design/writes.md` §3): this call took
        // the sync, so it stops the watcher that came with it, and no other call can.
        if let Some(watcher) = syncing.watcher {
            self.stop_watcher(watcher).await;
        }
        if let Some(watch) = syncing.sign_in_watch {
            watch.abort();
        }
        if let Some((task, _)) = syncing.thumbnails {
            let _ = task.await;
        }
        true
    }

    /// The second half of [`stop_sync`](Self::stop_sync): the activity
    /// log's clone of the store goes too, once no write holds it (see
    /// `store`), and so does the walker measuring the folder — a kick starts
    /// it again. Only where no sync can start meanwhile — with the lifecycle
    /// lock held for writing, or where nothing else starts one: done without
    /// it, a Forget's first stop let go of whatever was attached by then,
    /// the sync a reconnect had just started included.
    async fn let_go_of_activity(&self) {
        self.report.space.stop();
        let report = self.report.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || report.activity.detach()).await {
            tracing::warn!("the task letting go of the activity failed: {e}");
        }
    }

    /// Removes the tree store: a forgotten folder's, or one left from a
    /// folder forgotten earlier when a new one is registered.
    async fn remove_tree_store(&self) {
        *self.store.lock().unwrap() = None;
        // Its pause went with it (the outbox on the bus).
        self.forget_pause();
        let Some(paths) = self.sync_paths.lock().unwrap().clone() else { return };
        if let Err(e) = tokio::task::spawn_blocking(move || remove_tree_files(&paths.tree_db)).await {
            tracing::warn!("the task removing the tree store failed: {e}");
        }
    }

    /// The root is "persisted, so it survives a restart" — with its
    /// mode, and with the id the helper holds it by — as the account's
    /// `[accounts.root]`, through the one `ConfigStore`: every write re-reads
    /// the file, so nothing else in it is lost. `Err` when the file could not
    /// be written — or could not be read: what could not be read is never
    /// overwritten. The account's drive stays: it is the account's, not the
    /// folder's (design §8.1).
    fn save_root(&self, root: Option<&Persisted>) -> Result<(), String> {
        let Some(persist) = &self.persist else {
            return Ok(());
        };
        let root = root.map(|root| RootConfig {
            path: root.path.clone(),
            id: root.root_id.clone(),
            intercepted: root.intercepted,
            source: root.source.as_str().into(),
            baloo_excluded: root.baloo_excluded,
            upgrade_when_helper: Some(root.upgrade_when_helper),
        });
        persist.store.set_root(&persist.account, root).map_err(|e| {
            format!("cannot record the sync folder in {}: {e}", persist.store.file().display())
        })
    }

    /// [`save_root`](Self::save_root) where a failure cannot be undone
    /// anyway. The registration itself stands — the root is bound and
    /// usable right now, or forgotten — but the next start will not know,
    /// and §4.4's recovery walk is what the next start owes this folder.
    fn persist_or_log(&self, root: Option<&Persisted>) {
        if let Err(e) = self.save_root(root) {
            tracing::error!("{e}");
        }
    }

    /// [`persist_or_log`](Self::persist_or_log), only when `config.toml`
    /// does not already say exactly this.
    fn remember(&self, root: &Persisted) {
        if self.persist.is_some() && self.persisted_root().as_ref() != Some(root) {
            self.persist_or_log(Some(root));
        }
    }

    fn persisted_root(&self) -> Option<Persisted> {
        let persist = self.persist.as_ref()?;
        let root = persist.store.account(&persist.account)?.root?;
        let upgrade_when_helper = root.upgrades_when_helper();
        Some(Persisted {
            path: root.path,
            root_id: root.id,
            intercepted: root.intercepted,
            source: RootSource::parse(&root.source),
            baloo_excluded: root.baloo_excluded,
            upgrade_when_helper,
        })
    }

    /// Brings the sync folder up, or back up: re-registers the root with the
    /// helper — which re-marks the whole tree a restarted helper has
    /// forgotten — and re-runs §4.4's recovery walk, or, when no root is
    /// registered yet, restores the one persisted at the last start.
    ///
    /// Called once at startup and again after every helper reconnect.
    /// Without the startup call, this walk
    /// never ran in the shipped
    /// daemon at all: it only ever ran inside a `RegisterRoot` D-Bus call,
    /// so after a crash, files left `hydrating`/`dehydrating` stayed that
    /// way until a human registered the folder again.
    ///
    /// The sign-in gate `RegisterRoot` applies is deliberately not applied
    /// here. This is not a new registration but the return of one made
    /// earlier, and the folder is full of placeholders either way: refusing
    /// to re-register it because a token has not been restored yet would
    /// leave those placeholders unmarked and uninterceptable, reading as
    /// zeros, which is worse than anything a signed-out daemon can be.
    ///
    /// # An intercepted root is held before the helper is back
    ///
    /// A restored intercepted root becomes this daemon's registration at
    /// once, link or no link ([`hold`](Self::hold)), before any call that
    /// changes the registration is decided — this one, or a registration or
    /// Forget that reaches a freshly started daemon first
    /// ([`restore_locked`](Self::restore_locked)) — and it stays so if
    /// bringing it up then fails. It used to exist nowhere until the helper
    /// came back and the bind succeeded: in between — the start of every
    /// session, and a D-Bus-activated first call in particular — the daemon
    /// held no root, `RootState` said `none`, and
    /// `RegisterWithoutInterception` of the very folder the helper still
    /// held, marks, ignore marks and all, was accepted. Measured in the VM
    /// suite: the next dehydration there punched a file that was still
    /// ignored, and it read 65536 zero bytes. Held, it answers what an
    /// intercepted root with its helper gone answers — "already registered"
    /// to a second registration, `NoHelper` to a Forget or a dehydration.
    pub async fn resume(&self) {
        let _lifecycle = self.lifecycle.write().await;
        if self.held.lock().unwrap().is_some() {
            return;
        }
        self.restore_locked().await;
        match self.registration() {
            // Registered without interception: there is no helper
            // registration to renew. A folder registered that way because no
            // helper was connected switches to interception now that one is
            //, and its switch recovers it with this link; one
            // registered that way on purpose stays as it is — unless it shows
            // OneDrive, which is kept in step only with interception (HS2):
            // for such a folder no choice stands against the helper. A
            // recovery that had to leave files alone because a helper was
            // running with no link to it runs again now that
            // there is one.
            Some(reg) if !reg.intercepted => {
                let Some(link) = self.link() else { return };
                if reg.upgrade_when_helper || reg.source == RootSource::OneDrive {
                    self.upgrade(reg, link).await;
                } else if reg.recovery_deferred {
                    self.bring_up(&reg.root.path, false).await;
                }
            }
            // Nothing to register with yet; `supervise_helper` calls back
            // the moment there is.
            Some(_) if self.link().is_none() => {}
            Some(reg) => self.bring_up(&reg.root.path, true).await,
            None => {
                if let Some(persisted) = self.persisted_root().filter(|p| !p.intercepted) {
                    self.bring_up(&persisted.path, false).await;
                    let reg = self
                        .registration()
                        .filter(|r| !r.intercepted && (r.upgrade_when_helper || r.source == RootSource::OneDrive));
                    if let (Some(reg), Some(link)) = (reg, self.link()) {
                        self.upgrade(reg, link).await;
                    }
                }
            }
        }
        // What left the folder is marked again first (`docs/design/writes.md` §10), then the helper marked
        // nothing new while it was away (§3.3).
        self.outbox_helper_back();
        self.watcher_helper_back();
    }

    /// Holds the intercepted root `config.toml` records, if the daemon does
    /// not hold one yet — see [`resume`](Self::resume). Called by `main`
    /// before the bus name is claimed, so that the first thing a client
    /// reads is the folder rather than `none`; quick, since nothing is asked
    /// of the helper and at most one xattr is read.
    pub async fn restore(&self) {
        let _lifecycle = self.lifecycle.write().await;
        self.restore_locked().await;
    }

    /// [`restore`](Self::restore), under a `lifecycle` lock the caller
    /// already holds. Every call that changes the registration runs this
    /// first, so none of them can be decided — "no root yet", say — before
    /// the root `config.toml` records has been looked at, whichever of them
    /// reaches a freshly started daemon first.
    async fn restore_locked(&self) {
        if self.registration().is_some() || self.held.lock().unwrap().is_some() {
            return;
        }
        if let Some(persisted) = self.persisted_root().filter(|p| p.intercepted) {
            self.hold(persisted).await;
        }
    }

    /// Binds a root that is already this daemon's, publishing why when that
    /// fails. The root is left exactly as it was either way.
    async fn bring_up(&self, path: &Path, intercepted: bool) {
        if let Err(e) = self.bind(path, intercepted, false).await {
            let message = format!("cannot bring up the sync folder {}: {e}", path.display());
            tracing::error!("{message}");
            self.state.update(|s| {
                s.root_path = path.display().to_string();
                s.root_state = RootState::Error;
                s.last_error = message;
            });
        }
    }

    /// Switches a folder registered without interception because no helper
    /// was connected to interception, now that one is. Called by
    /// [`resume`](Self::resume), with `lifecycle` held for writing, so no
    /// registration, Forget, populate or free-up runs meanwhile.
    ///
    /// Found in real use: a folder registered before the helper
    /// was installed stayed without interception once it was, and every file
    /// in it read as zeros until a Forget and a new registration.
    ///
    /// # In this order
    ///
    /// 1. The folder's sync is stopped. A running sync decided at its start
    ///    that nothing is to be marked, and would go on placing directories
    ///    unmarked (invariant M1).
    /// 2. The switch is written down in `config.toml` before the helper hears
    ///    of the folder, as a fresh intercepted registration is: a crash
    ///    after that leaves a folder the next start holds as
    ///    intercepted and brings up at the helper's connect.
    /// 3. The helper registers the root. Its walk marks every directory in
    ///    it — the very walk that brings an intercepted folder back at every
    ///    restart, where content, too, was placed before the marks were. The
    ///    folder is then recovered with this link.
    /// 4. [`commit`](Self::commit) publishes it as intercepted — `ready`, and
    ///    the no-interception warning gone — and starts its sync again,
    ///    intercepted this time, so that everything it places from then on
    ///    is marked first.
    ///
    /// # When it fails (Ruling 2 of)
    ///
    /// The helper is asked to let go of anything it may have saved, and
    /// `config.toml` is put back: the folder stays exactly as it was, without
    /// interception, its sync running again, and `LastError` says why. The
    /// next connect tries again. The one exception is a helper that cannot
    /// confirm it let go: the folder is then kept intercepted, waiting for
    /// the next connect to bring it up ([`NotSwitched::Held`]).
    async fn upgrade(&self, reg: Registration, link: HelperLink) {
        let shown = reg.root.path.display().to_string();
        tracing::info!("the konedrive helper is connected: switching {shown} to interception");
        let was_syncing = self.stop_sync().await;
        match self.switch_to_interception(&reg, &link).await {
            Ok(()) => tracing::info!("{shown} is intercepted now"),
            Err(NotSwitched::Held) => {}
            Err(NotSwitched::Kept(why)) => {
                tracing::error!("switching {shown} to interception failed, so it stays without: {why}");
                if reg.recovery_deferred {
                    // What `resume` runs for such a folder instead; it starts
                    // the sync again as every bring-up does.
                    self.bring_up(&reg.root.path, false).await;
                } else if was_syncing {
                    self.start_sync().await;
                }
                self.note_switch_failed(&why);
            }
        }
    }

    /// Steps 2 to 4 of [`upgrade`](Self::upgrade).
    async fn switch_to_interception(&self, reg: &Registration, link: &HelperLink) -> Result<(), NotSwitched> {
        let (dir, root) =
            root::prepare(&reg.root.path).await.map_err(|e| NotSwitched::Kept(SyncError::from(e).to_string()))?;
        let previous = self.persisted_root();
        self.save_root(Some(&Persisted::of(&root, true, reg.source, reg.baloo_excluded, false)))
            .map_err(NotSwitched::Kept)?;
        let registered = link.register_root(&dir, &root.root_id).await.map_err(|e| e.to_string());
        drop(dir);
        let recovered = match registered {
            Ok(()) => {
                let clearance = Clearance::Link(link.clone());
                root::recover(&clearance, &root, &self.locks).await.map_err(|e| e.to_string())
            }
            Err(why) => Err(why),
        };
        match recovered {
            Ok(report) => {
                self.commit(root, true, reg.source, false, false, report).await;
                Ok(())
            }
            Err(why) => Err(self.undo_switch(link, root, reg, previous, why).await),
        }
    }

    /// Undoes a switch that failed after `config.toml` recorded it — the way
    /// [`abandon`](Self::abandon) undoes a fresh registration: the helper is
    /// told to let go, and `config.toml` is put back. If the helper cannot
    /// confirm it let go, the folder is kept intercepted instead: a folder
    /// the helper may still hold must never be one the daemon
    /// holds without interception. Its sync stays stopped until it is
    /// brought up, at the next connect.
    async fn undo_switch(
        &self,
        link: &HelperLink,
        root: SyncRoot,
        reg: &Registration,
        previous: Option<Persisted>,
        why: String,
    ) -> NotSwitched {
        match link.unregister_root(&root.root_id).await {
            Ok(()) | Err(HelperError::Refused(libc::EPERM)) => {
                self.persist_or_log(previous.as_ref());
                NotSwitched::Kept(why)
            }
            Err(e) => {
                let message = format!(
                    "switching {} to interception failed ({why}), and the helper could not be told \
                     to let go of it ({e}); it is kept with interception, and brought up the next \
                     time the helper connects",
                    root.path.display()
                );
                tracing::error!("{message}");
                *self.root.lock().unwrap() = Some(Registration {
                    dev: hub::device_of(&root.path),
                    root,
                    intercepted: true,
                    recovery_deferred: false,
                    source: reg.source,
                    brought_up: false,
                    baloo_excluded: reg.baloo_excluded,
                    upgrade_when_helper: false,
                });
                self.state.update(|s| {
                    s.root_state = RootState::Error;
                    s.last_error = message;
                });
                NotSwitched::Held
            }
        }
    }

    /// Adds why a switch failed to `LastError`, in place of what an earlier
    /// failed switch said there.
    fn note_switch_failed(&self, why: &str) {
        let note = format!("{SWITCH_FAILED}: {why}; it is tried again the next time the helper connects");
        self.state.update(|s| {
            let before = s.last_error.split(SWITCH_FAILED).next().unwrap_or_default();
            let before = before.trim_end_matches(". ");
            s.last_error = if before.is_empty() { note } else { format!("{before}. {note}") };
        });
    }

    /// Takes an intercepted root restored from `config.toml` as this
    /// daemon's registration, before anything is asked of the helper, and
    /// publishes it as waiting for the helper.
    ///
    /// Its id comes from `config.toml` — it is the name the helper holds the
    /// root by, and all a Forget needs even when the folder is gone — or,
    /// from a config written before the id was recorded, from the folder
    /// itself. With neither, the root is not held, and the failure is
    /// published as a startup failure always was; that leaves the one case
    /// lists.
    async fn hold(&self, persisted: Persisted) {
        let root_id = if root::looks_like_a_root_id(&persisted.root_id) {
            persisted.root_id
        } else if let Some(root_id) = root::recorded_root_id(&persisted.path).await {
            root_id
        } else {
            let message = format!(
                "cannot bring up the sync folder {}: config.toml does not record its root id, \
                 and the folder carries none that can be read",
                persisted.path.display()
            );
            tracing::error!("{message}");
            self.state.update(|s| {
                s.root_path = persisted.path.display().to_string();
                s.root_state = RootState::Error;
                s.last_error = message;
            });
            return;
        };
        let shown = persisted.path.display().to_string();
        *self.root.lock().unwrap() = Some(Registration {
            dev: hub::device_of(&persisted.path),
            root: SyncRoot { path: persisted.path, root_id },
            intercepted: true,
            recovery_deferred: false,
            source: persisted.source,
            brought_up: false,
            // Held, not yet brought up: a Forget of a held root always
            // fails before it reaches Baloo (`forget_locked` needs a link,
            // which is exactly what held means there is none of), so what
            // this says here is never acted on either way.
            baloo_excluded: false,
            upgrade_when_helper: false,
        });
        self.state.update(|s| {
            s.root_path = shown;
            s.root_state = RootState::Error;
            s.last_error.clear();
            s.waits_for_helper = true;
        });
    }

    /// Publishes the helper's disappearance: `RootState` used
    /// to stay `ready` with an empty `LastError` while the sync folder was,
    /// in the only sense that matters, dead — nothing intercepting, nothing
    /// reconnecting, and every un-hydrated file reading as zeros. Now it
    /// reads `error`, and `LastError` says what `HelperState` says (HS3):
    /// how to start the helper. The rest of what `LastError` said stays.
    pub fn report_helper_lost(&self) {
        let Some(reg) = self.registration() else {
            return;
        };
        if reg.intercepted || reg.source == RootSource::OneDrive {
            self.state.update(|s| s.waits_for_helper = true);
        }
    }

    /// Mirrors `source_dir` into the root as placeholders: the
    /// offline stand-in for the real fill. Also remembers `source_dir` as
    /// this service's `ContentSource`, so `Hydrate()` (and any real
    /// interception-driven fill routed through `serve_hydrations`) has
    /// somewhere to fetch bytes from afterwards.
    pub async fn populate_from_directory(&self, source_dir: &Path) -> Result<u64, SyncError> {
        // The mode decides what is asked of the helper below, so it must not
        // change until this is done (see `lifecycle`).
        let _lifecycle = self.lifecycle.read().await;
        let reg = self.require_registration()?;
        if reg.source == RootSource::OneDrive {
            return Err(SyncError::Unsupported(
                "this folder shows your OneDrive; filling it from a directory is for a folder \
                 registered while signed out"
                    .into(),
            ));
        }
        // a source that overlaps the root is refused
        // — one inside it is a directory of placeholders, which a fill would
        // copy as zeros into a file it then stamps `hydrated`, and one around
        // it would be mirrored into itself. Compared by resolved path, so a
        // symlink to either is seen through; a bind mount is not — but a
        // file reached through one carries konedrive's attributes, and every
        // source file is refused on those, or on leading into the folder,
        // both when it is mirrored and when it is read.
        let named = source_dir.to_path_buf();
        let root = reg.root.path.clone();
        let (source, root) = on_blocking_thread(move || {
            Ok::<_, io::Error>((std::fs::canonicalize(&named)?, std::fs::canonicalize(&root)?))
        })
        .await
        .and_then(|resolved| resolved)
        .map_err(|e| SyncError::Io(format!("{}: {e}", source_dir.display())))?;
        if source.starts_with(&root) || root.starts_with(&source) {
            return Err(SyncError::Unsupported(format!(
                "{} {} the sync folder {}, and a folder cannot be filled from itself: its files \
                 there are placeholders, which would be copied as zeros",
                source.display(),
                if source.starts_with(&root) { "is inside" } else { "contains" },
                root.display()
            )));
        }
        // A root registered without interception is never
        // announced to the helper, and a `MarkDir` is an announcement. Sent
        // whenever a link merely existed, it failed the whole populate on a
        // filesystem where the uid owns no helper root (`EPERM`) and, where
        // it owns one, it landed: the directory was intercepted, and a folder
        // the user asked to leave alone was half intercepted. Both measured
        // in the VM suite.
        let link = if reg.intercepted { self.link() } else { None };
        let walk = Walk { link: link.as_ref(), root: &root };
        let created = populate_walk(walk, &source, &reg.root.path, Path::new(""))
            .await
            .map_err(|e| match e.get_ref().and_then(|inner| inner.downcast_ref::<RefusedSource>()) {
                Some(RefusedSource(why)) => SyncError::Unsupported(why.clone()),
                None => SyncError::Io(format!("{}: {e}", source_dir.display())),
            })?;
        // The source refuses, when the bytes are read, any file
        // that leads into the folder by then — a symlink swapped since.
        *self.source.lock().unwrap() =
            Some(Arc::new(LocalDir::new(source).refusing_files_of(root)) as Arc<dyn ContentSource>);
        Ok(created)
    }

    /// `Refresh()`: a cycle now, for a folder that shows OneDrive.
    ///
    /// A folder whose sync is not running — it could not start: its tree
    /// store could not be opened (F18) — has it started again here, the way
    /// every start is made, with `lifecycle` held for writing. `Ok` means a
    /// sync runs; when it still cannot, the refusal says why. A folder not
    /// brought up yet — held until its helper is back, or kept after a
    /// registration that failed — is refused: its sync starts when it is.
    ///
    /// Refused `NoHelper` whenever the folder has no helper to keep it in
    /// step with (HS2): no link, or no interception yet.
    pub async fn refresh(&self) -> Result<(), SyncError> {
        self.require_helper_for(&self.require_onedrive()?)?;
        // The outbox too (`docs/design/writes.md` §11): rows in backoff go now,
        // and, while a sync runs, the quota is read again, which may end a
        // full OneDrive (issue #2).
        self.retry_outbox();
        if self.nudge() {
            self.refresh_quota().await;
            return Ok(());
        }
        let _lifecycle = self.lifecycle.write().await;
        // Looked at again under the lock: a Forget may have come first.
        let reg = self.require_onedrive()?;
        self.require_helper_for(&reg)?;
        if !reg.brought_up {
            return Err(SyncError::Io(format!("the folder is not up: {}", self.last_error())));
        }
        self.start_sync().await;
        if self.syncing.lock().unwrap().is_some() {
            drop(_lifecycle);
            self.refresh_quota().await;
            return Ok(());
        }
        let why = self.state.get().sync_trouble.map(|t| t.text);
        Err(SyncError::Io(why.unwrap_or_else(|| "the sync could not be started".into())))
    }

    /// HS2: a folder that shows OneDrive is kept in step only when it is
    /// intercepted and the helper is connected.
    fn require_helper_for(&self, reg: &Registration) -> Result<(), SyncError> {
        if reg.intercepted && self.link().is_some() {
            Ok(())
        } else {
            Err(SyncError::NoHelper)
        }
    }

    fn require_onedrive(&self) -> Result<Registration, SyncError> {
        let reg = self.require_registration()?;
        if reg.source != RootSource::OneDrive {
            return Err(SyncError::Unsupported("this folder is not connected to OneDrive".into()));
        }
        Ok(reg)
    }

    /// A cycle now, if a OneDrive folder is syncing (the network came back). A read-write
    /// folder's outbox waits for that cycle (`docs/design/writes.md` §9).
    pub fn refresh_now(&self) {
        self.outbox_after_network();
        self.nudge();
    }

    /// [`refresh_now`](Self::refresh_now); whether a sync was running to
    /// nudge.
    fn nudge(&self) -> bool {
        match self.syncing.lock().unwrap().as_ref() {
            Some(syncing) => {
                syncing.poller.refresh();
                // The notification socket too: closed while stopped, opened again when not,
                // tried again at once when the network came back (`live`).
                syncing.poller.wake_live();
                true
            }
            None => false,
        }
    }

    /// A cycle now with a Full reconcile, which places again what is missing
    /// here (`RestoreDeletes`, a forgotten local object). Whether a sync runs.
    fn nudge_full(&self) -> bool {
        match self.syncing.lock().unwrap().as_ref() {
            Some(syncing) => {
                syncing.poller.refresh_full();
                true
            }
            None => false,
        }
    }

    /// `Skipped()`: every item not in the folder whose own folder is, as a
    /// full path and a reason.
    ///
    /// The store is read with `lifecycle` held for reading (see `store`), so
    /// a Forget waits for a read under way rather than remove the files
    /// under it. The lock goes into the blocking task with the store's clone,
    /// so it is held as long as the clone is, even when this call is dropped
    /// part-way.
    pub async fn skipped(&self) -> Result<Vec<(String, String)>, SyncError> {
        let lifecycle = Arc::clone(&self.lifecycle).read_owned().await;
        let Some(reg) = self.registration() else { return Ok(Vec::new()) };
        let Some(store) = self.store.lock().unwrap().clone() else { return Ok(Vec::new()) };
        let skipped = tokio::task::spawn_blocking(move || {
            let _lifecycle = lifecycle;
            // One query, on the read-only connection (issue #39): the cycle's
            // work is not held up behind it.
            store.read_blocking(|s| s.skipped())
        })
        .await
        .map_err(|e| SyncError::Io(format!("the store task failed: {e}")))?
        .map_err(|e| SyncError::Io(e.to_string()))?;
        Ok(skipped
            .into_iter()
            .map(|(rel, reason)| (reg.root.path.join(rel).display().to_string(), reason.as_str().to_owned()))
            .collect())
    }

    /// `ItemsListed`, `ItemsPlaced`, `SkippedCount`.
    pub fn items(&self) -> (u64, u64, u64) {
        let s = self.state.get();
        (s.items_listed, s.items_placed, s.skipped_count)
    }

    /// Fills one placeholder now — see the type's own doc comment for why
    /// this fills directly rather than only through kernel interception.
    ///
    /// # Open first, then lock, then look again
    ///
    /// The order matters three times over.
    ///
    /// *Open through the gate.* The descriptor comes from
    /// `SyncRoot::open_inside` — `openat2` with `RESOLVE_BENEATH`,
    /// `RESOLVE_NO_SYMLINKS` and `O_NOFOLLOW`, from the root's own
    /// descriptor, only while the folder still carries this root's id. The
    /// version this replaces checked a canonicalized *string*, awaited a
    /// lock with no time limit, and only then opened that string by name;
    /// measured, an ordinary directory rename inside the root in that window
    /// was enough to have `source::hydrate` `pwrite` a file **outside** the
    /// root and `Hydrate` report success.
    ///
    /// *Lock after the open, not before it.* Taking the lock first
    /// deadlocked the very path it exists to coordinate with: with a real
    /// helper and a marked root, opening an `online-only` file is
    /// intercepted, and the interception travels helper →
    /// `serve_hydrations` → the same lock, which this call is holding while
    /// it waits for that open to return. Nothing in `cargo test` could reach
    /// it, because nothing unprivileged can install a fanotify group.
    ///
    /// *Read the state again under the lock.* Whoever held the lock first
    /// may well have been a fill of this same inode, and it may have
    /// finished the job.
    pub async fn hydrate_now(&self, path: &Path) -> Result<(), SyncError> {
        // "Download now" is an open, for the pool: it goes first.
        match self.fill_now(path, Some(crate::pool::Class::Open)).await? {
            Answered::Failed(FillError::NotCleared(NotCleared::Unlinked)) => Err(SyncError::NoHelper),
            Answered::Failed(FillError::NotCleared(e)) => Err(SyncError::Io(format!("nothing was filled: {e}"))),
            Answered::Failed(FillError::Errno(errno)) => Err(SyncError::Io(format!(
                "hydration failed: {}",
                std::io::Error::from_raw_os_error(errno)
            ))),
            _ => Ok(()),
        }
    }

    /// [`hydrate_now`](Self::hydrate_now)'s fill, and what came of it —
    /// recorded as any fill is. A pinned download goes through here too
    /// ([`pin::PinFill`]).
    ///
    /// `class` is the slot of the account's transfer pool it takes, before the per-inode
    /// lock (never waiting for a slot with the lock held); `None` when the caller holds one
    /// already (a pinned download). A pinned download of a large file goes in parallel parts
    /// (`source::parts`, issue #28), the slot held for it being its first stream's; a file
    /// being opened, and `Hydrate`, keep one stream.
    async fn fill_now(&self, path: &Path, class: Option<crate::pool::Class>) -> Result<Answered, SyncError> {
        let reg = self.require_registration()?;
        let Some(source) = self.source.lock().unwrap().clone() else {
            return Err(SyncError::NoSource);
        };

        let root = reg.root.clone();
        let target = path.to_path_buf();
        let (file, shown) = tokio::task::spawn_blocking(move || open_shown(&root, &target))
            .await
            .map_err(|e| SyncError::Io(format!("the hydration task failed: {e}")))??;
        let key = InodeKey::of(&file).map_err(|e| SyncError::Io(e.to_string()))?;

        // A placeholder has its full size: whether this is a large transfer.
        let size = crate::pool::Size::of(file.metadata().map_or(0, |meta| meta.len()));
        let mut slot = match class {
            Some(class) => Some(self.pool.acquire_sized(class, size).await),
            None => None,
        };
        let split = (class.is_none() && size == crate::pool::Size::Large)
            .then(|| source::Split::new(Arc::clone(&self.pool), Arc::clone(&self.parts)));
        // Serializes against `dehydrate()` and against `serve_hydrations`'s
        // own fills of the same inode (both share this table).
        let guard = self.locks.lock(key).await;

        let (file, decision) = tokio::task::spawn_blocking(move || {
            let decision = classify_for_hydration(&file);
            (file, decision)
        })
        .await
        .map_err(|e| SyncError::Io(format!("the hydration task failed: {e}")))?;
        let may_be_marked = match decision? {
            Fill::AlreadyThere => return Ok(Answered::AlreadyThere),
            Fill::Needed { may_be_marked } => may_be_marked,
        };
        // A file that could be carrying an ignore mark has the way cleared
        // before the fill can fail and punch it (`source::hydrate_with`), by
        // local rule. An intercepted root needs its link for
        // that — without one, refuse rather than fill a file a failure would
        // then empty under its mark. A root registered without interception
        // clears it the same way when there is a link, and otherwise goes by
        // whether a helper is running at all (`Clearance`).
        let clearance = match (may_be_marked, reg.intercepted) {
            (false, _) => None,
            (true, true) => Some(Clearance::Link(self.require_link()?)),
            (true, false) => Some(self.clearance()),
        };

        let fd: std::os::fd::OwnedFd = file.into();
        // Shown in `Transfers.Downloads` while it downloads; `Hydrate` (an open, for the pool)
        // as a file being opened.
        let tracked = if class == Some(crate::pool::Class::Open) {
            Tracked::opening(source, self.report.transfers.clone(), shown.clone())
        } else {
            Tracked::new(source, self.report.transfers.clone(), shown.clone())
        };
        // Stopped where it is when the file is taken off the disk because
        // OneDrive removed its item (issue #104).
        let fill = async {
            match &split {
                Some(split) => source::hydrate_in_parts(fd, &tracked, clearance.as_ref(), split).await,
                None => source::hydrate_with(fd, &tracked, clearance.as_ref()).await,
            }
        };
        let filled = unless_removed(Some(&guard), fill).await.unwrap_or(Err(FillError::Errno(libc::ENOENT)));
        let size = tracked.fetched();
        drop(tracked);
        drop(guard);
        let answered = match filled {
            Ok(()) => Answered::Filled,
            Err(e) => Answered::Failed(e),
        };
        if let (Some(slot), Answered::Filled) = (slot.as_mut(), &answered) {
            slot.succeeded();
        }
        drop(slot);
        // A fill that never started for want of the helper is a refusal
        // the caller is told of, not a download that failed.
        let refused = matches!(answered, Answered::Failed(FillError::NotCleared(_)));
        if let Some(event) = fill_event(&answered, &shown, size).filter(|_| !refused) {
            self.report.activity.record(vec![event]).await;
            self.report.space.kick();
        }
        Ok(answered)
    }

    /// Frees a hydrated file's space back to a placeholder.
    ///
    /// The file is opened here, through the same `SyncRoot::open_inside`
    /// gate, so that the per-inode lock can be taken on the inode that is
    /// about to be emptied — `(st_dev, st_ino)` from that very descriptor,
    /// never a name — and so that the descriptor the lock was
    /// taken on is the one `root::dehydrate_opened` marks, clears and
    /// punches (one open per dehydration).
    ///
    /// Recorded as a `freed` event with what it freed.
    ///
    /// Refused `NotAllowed` for a file a pin keeps on this device — its own,
    /// or a folder's above it: `FreeUp` takes a pin off.
    pub async fn dehydrate(&self, path: &Path) -> Result<(), SyncError> {
        let reg = self.require_registration()?;
        let (root, target) = (reg.root.clone(), path.to_path_buf());
        let pinned = tokio::task::spawn_blocking(move || {
            let full = root.path.join(root.relative(&target).ok()?);
            pin::pinned_by(&root.path, &full).map(|by| (full, by))
        })
        .await
        .map_err(|e| SyncError::Io(format!("the dehydration task failed: {e}")))?;
        if let Some((full, by)) = pinned {
            return Err(SyncError::NotAllowed(pin::refusal(&full, &by)));
        }
        let (freed, shown) = self.free_one(path, Wait::Yes).await?;
        let event = activity::event(Kind::Freed, shown, activity::human_size(freed));
        self.report.activity.record(vec![event]).await;
        self.report.space.kick();
        Ok(())
    }

    /// `FreeUpSpace()`: every downloaded file under the folder
    /// freed up through the same per-file path `Dehydrate` takes — the
    /// helper's `ClearIgnore`, the write lease, the per-inode lock — so every
    /// rule that holds for one file holds here.
    ///
    /// A file that is open (its lease is refused) or that a fill or another
    /// free-up is busy with (its per-inode lock is taken) is left as it is
    /// and counted as busy, never waited for: a download can take any time.
    /// A file that is not a clean download — changed here, or not ours — is
    /// left alone as `Dehydrate` would refuse it, and counted in neither.
    /// The walk only reads names and attributes (`lstat`, `lgetxattr`); only
    /// a file that reads `hydrated` is opened, to be freed.
    ///
    /// Refused as `Dehydrate` is where no file could be freed (no root; an
    /// intercepted root with no helper), and stopped with that refusal if it
    /// becomes true part-way. What was freed is one `freed` event for the
    /// folder, not one per file.
    ///
    /// A file a pin keeps on this device is left as it is, and counted in
    /// [`FreedUp::pinned`]; the event says how many.
    pub async fn free_up_space(&self) -> Result<FreedUp, SyncError> {
        let reg = self.require_registration()?;
        if reg.intercepted {
            self.require_link()?;
        }
        let root = reg.root.path.clone();
        let (candidates, pinned) = tokio::task::spawn_blocking(move || pin::downloaded_under(&root, false))
            .await
            .map_err(|e| SyncError::Io(format!("the walk of the folder failed: {e}")))?;
        let (mut freed, stopped) = self.free_each(candidates).await;
        freed.pinned = pinned;
        if freed.files > 0 {
            let mut detail = freed_detail(freed.files, freed.bytes);
            if pinned > 0 {
                detail.push_str(&format!("; {pinned} kept on this device"));
            }
            let event = activity::event(Kind::Freed, reg.root.path.display().to_string(), detail);
            self.report.activity.record(vec![event]).await;
        }
        if pinned > 0 {
            tracing::info!("free up space kept {pinned} file(s) that are always kept on this device");
        }
        self.report.space.kick();
        match stopped {
            // Forgotten part-way: what was freed
            // is freed, and is what the caller hears — not a refusal that
            // drops the counts.
            Some(SyncError::NoRoot) => Ok(freed),
            Some(e) => Err(e),
            None => Ok(freed),
        }
    }

    /// Frees each of `files` up without waiting for any ([`free_one`](Self::free_one)
    /// with `Wait::No`): what was freed, and the refusal — no helper, no
    /// root — that stopped it part-way, if one did. A file in use is counted
    /// busy, one changed here [`FreedUp::modified`]; either is left as it is.
    async fn free_each(&self, files: Vec<PathBuf>) -> (FreedUp, Option<SyncError>) {
        let mut freed = FreedUp::default();
        for path in files {
            match self.free_one(&path, Wait::No).await {
                Ok((bytes, _)) => {
                    freed.files += 1;
                    freed.bytes += bytes;
                }
                Err(SyncError::InUse) => freed.busy += 1,
                Err(SyncError::ModifiedLocally) => freed.modified += 1,
                // Counted as busy, as `FreeUpSpace` says: it waits to go up.
                Err(SyncError::NotUploaded(_)) => freed.busy += 1,
                Err(e @ (SyncError::NoHelper | SyncError::NoRoot)) => return (freed, Some(e)),
                Err(e) => tracing::info!("{} is not freed up: {e}", path.display()),
            }
        }
        (freed, None)
    }

    /// Puts a pin on each of `targets`, or takes it off, through `write`
    /// ([`pin::set_pin`]; a test's own to fail on purpose), stopping at the
    /// first failure: the paths done, and that failure. A file's pin is
    /// written under its per-inode lock, since a fill lifts the same write
    /// bit around its own attribute writes; each descriptor is closed once
    /// its write is done.
    async fn set_pins(
        &self,
        targets: Vec<PinTarget>,
        on: bool,
        write: fn(&File, bool) -> io::Result<()>,
    ) -> (Vec<PathBuf>, Option<SyncError>) {
        let mut done = Vec::new();
        for PinTarget { item, shown, is_dir, .. } in targets {
            let guard = if is_dir {
                None
            } else {
                match InodeKey::of(&item) {
                    Ok(key) => Some(self.locks.lock(key).await),
                    Err(e) => return (done, Some(SyncError::Io(format!("{}: {e}", shown.display())))),
                }
            };
            let written = tokio::task::spawn_blocking(move || write(&item, on)).await;
            drop(guard);
            match written {
                Ok(Ok(())) => done.push(shown),
                Ok(Err(e)) => return (done, Some(SyncError::Io(format!("{}: {e}", shown.display())))),
                Err(e) => return (done, Some(SyncError::Io(format!("the pin task failed: {e}")))),
            }
        }
        (done, None)
    }

    /// Every one of `paths` opened and looked at ([`pin_targets`]), on a
    /// blocking thread.
    async fn pin_targets(&self, root: &SyncRoot, paths: &[PathBuf]) -> Result<Vec<PinTarget>, SyncError> {
        let (root, paths) = (root.clone(), paths.to_vec());
        tokio::task::spawn_blocking(move || pin_targets(&root, &paths))
            .await
            .map_err(|e| SyncError::Io(format!("the pin task failed: {e}")))?
    }

    /// `Pin(paths)`, "Always keep on this device": each path — a file, a
    /// folder, or the folder itself — gets a pin, and every online-only file
    /// under it is queued for download ([`pin::Pins`]); how many were queued
    /// by this call.
    ///
    /// The pin is written first and the downloads follow, so a crash in
    /// between loses nothing: the next sweep finds them. A path a folder
    /// above it pins already is left as it is. Every path is checked before
    /// any is pinned: one outside the folder, a `.konedrive-*` name, or a
    /// file that is not ours refuses the call.
    pub async fn pin(&self, paths: &[PathBuf]) -> Result<u32, SyncError> {
        let reg = self.require_registration()?;
        let targets = self.pin_targets(&reg.root, paths).await?;
        let (mut again, mut write) = (Vec::new(), Vec::new());
        for target in targets {
            if !target.above.is_empty() {
                continue;
            }
            if target.own {
                again.push(target.shown);
            } else {
                write.push(target);
            }
        }
        let (pinned, failed) = self.set_pins(write, true, pin::set_pin).await;
        for shown in &pinned {
            self.pins.pinned(shown.clone());
        }
        // What is pinned is queued, even when a later path could not be; a
        // path pinned already is looked through again.
        again.extend(pinned);
        let queued = self.pins.queue_under(again).await;
        match failed {
            Some(e) => Err(e),
            None => Ok(queued),
        }
    }

    /// What `Unpin` and `FreeUp` check of every path before they change
    /// anything, on its own: each path is in the folder and one of ours, and
    /// no folder above it pins it and stays pinned (`NotAllowed`). `Files`
    /// asks every account whose folder a call's paths are in first, so that
    /// a call that spans accounts is refused as a whole or not at all.
    pub async fn check_unpinnable(&self, paths: &[PathBuf]) -> Result<(), SyncError> {
        let reg = self.require_registration()?;
        let targets = self.pin_targets(&reg.root, paths).await?;
        match kept_by_folder(&targets) {
            Some(refusal) => Err(refusal),
            None => Ok(()),
        }
    }

    /// What `Pin` checks of every path before it pins any, on its own: each
    /// path is in the folder, and one of ours. `Files` asks every account
    /// first, as for [`check_unpinnable`](Self::check_unpinnable).
    pub async fn check_pinnable(&self, paths: &[PathBuf]) -> Result<(), SyncError> {
        let reg = self.require_registration()?;
        self.pin_targets(&reg.root, paths).await.map(drop)
    }

    /// What `FreeUp` checks before it frees anything, on its own:
    /// [`check_unpinnable`](Self::check_unpinnable)'s rules, and a folder
    /// with interception has its helper (`NoHelper`).
    pub async fn check_free_up(&self, paths: &[PathBuf]) -> Result<(), SyncError> {
        let reg = self.require_registration()?;
        if reg.intercepted {
            self.require_link()?;
        }
        self.check_unpinnable(paths).await?;
        // A file named itself with a change waiting to go up refuses the whole
        // call, before any account frees anything (the outbox on the bus).
        let targets = self.pin_targets(&reg.root, paths).await?;
        for target in targets.iter().filter(|t| !t.is_dir) {
            self.refuse_unuploaded(&target.item, &target.shown.display().to_string()).await?;
        }
        Ok(())
    }

    /// `Unpin(paths)`, unchecking "Always keep on this device": each path's
    /// own pin comes off, and nothing else changes — its files stay
    /// downloaded. How many pins came off. A path a folder above it pins is
    /// refused `NotAllowed`, naming the folder, as `FreeUp` refuses it
    /// ([`kept_by_folder`]); every path is checked before any pin comes off.
    pub async fn unpin(&self, paths: &[PathBuf]) -> Result<u32, SyncError> {
        let reg = self.require_registration()?;
        let targets = self.pin_targets(&reg.root, paths).await?;
        if let Some(refusal) = kept_by_folder(&targets) {
            return Err(refusal);
        }
        let own: Vec<PinTarget> = targets.into_iter().filter(|target| target.own).collect();
        let (unpinned, failed) = self.set_pins(own, false, pin::set_pin).await;
        for shown in &unpinned {
            self.pins.unpinned(shown);
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(unpinned.len() as u32),
        }
    }

    /// `FreeUp(paths)`, the menu's "Free up space":
    ///
    /// - a path with a pin of its own loses it, and then everything under it
    ///   is freed up;
    /// - a path a folder above it pins is refused `NotAllowed`, naming that
    ///   folder — unless that folder is one of `paths`, whose pin this call
    ///   takes off ([`kept_by_folder`]); checked for every path before
    ///   anything changes;
    /// - any other path is freed up.
    ///
    /// The pins come off first. One that cannot stops the call with that
    /// failure, before anything is freed; the pins already off stay off, and
    /// `PinnedCount` says so.
    ///
    /// Each file goes through `FreeUpSpace`'s per-file path: one in use or
    /// changed here is left and counted (`busy` and [`FreedUp::modified`]),
    /// and one a pin of its own — or of a folder between it and the path, or
    /// above the path by the time it is walked — keeps is left and counted in
    /// [`FreedUp::pinned`]. One `freed` event per path that freed anything.
    pub async fn free_up(&self, paths: &[PathBuf]) -> Result<FreedUp, SyncError> {
        self.free_up_with(paths, pin::set_pin).await
    }

    /// [`free_up`](Self::free_up), taking pins off through `write`.
    async fn free_up_with(&self, paths: &[PathBuf], write: fn(&File, bool) -> io::Result<()>) -> Result<FreedUp, SyncError> {
        let reg = self.require_registration()?;
        if reg.intercepted {
            self.require_link()?;
        }
        let targets = self.pin_targets(&reg.root, paths).await?;
        if let Some(refusal) = kept_by_folder(&targets) {
            return Err(refusal);
        }
        // A file named itself, whose change waits to be uploaded, is refused as
        // a whole (`docs/design/writes.md` §11); inside a folder it is left and counted.
        for target in targets.iter().filter(|t| !t.is_dir) {
            self.refuse_unuploaded(&target.item, &target.shown.display().to_string()).await?;
        }
        let walks: Vec<(PathBuf, bool)> = targets.iter().map(|t| (t.shown.clone(), t.is_dir)).collect();
        // The other descriptors close here: one of our own left open on a
        // file would refuse the write lease its free-up takes.
        let own: Vec<PinTarget> = targets.into_iter().filter(|target| target.own).collect();
        let (unpinned, failed) = self.set_pins(own, false, write).await;
        for shown in &unpinned {
            self.pins.unpinned(shown);
        }
        if let Some(e) = failed {
            return Err(e);
        }
        let mut total = FreedUp::default();
        let mut stopped = None;
        for (shown, is_dir) in walks {
            let (root, start) = (reg.root.path.clone(), shown.clone());
            let (candidates, pinned) = tokio::task::spawn_blocking(move || {
                // Looked at again now: a folder above pinned since the check
                // keeps everything under it.
                let inherited = pin::pinned_above(&root, &start).is_some();
                pin::downloaded_under(&start, inherited)
            })
            .await
            .map_err(|e| SyncError::Io(format!("the walk of the folder failed: {e}")))?;
            let (freed, stop) = self.free_each(candidates).await;
            if freed.files > 0 {
                let detail = if is_dir { freed_detail(freed.files, freed.bytes) } else { activity::human_size(freed.bytes) };
                let event = activity::event(Kind::Freed, shown.display().to_string(), detail);
                self.report.activity.record(vec![event]).await;
            }
            total.files += freed.files;
            total.bytes += freed.bytes;
            total.busy += freed.busy;
            total.modified += freed.modified;
            total.pinned += pinned;
            if stop.is_some() {
                stopped = stop;
                break;
            }
        }
        self.report.space.kick();
        match stopped {
            Some(SyncError::NoRoot) | None => Ok(total),
            Some(e) => Err(e),
        }
    }

    /// `PinnedCount`.
    pub fn pinned_count(&self) -> u32 {
        self.state.get().pinned_count
    }

    /// One file freed up, and how many bytes of blocks that gave back —
    /// `Dehydrate`'s whole sequence. `Wait::No` answers `InUse` rather than
    /// wait for a fill or a free-up of the same file (`FreeUpSpace`).
    ///
    /// # Open, wait for the file, and only then decide
    ///
    /// The mode decides what the punch may go by, and it must not change
    /// between that decision and the punch (see `lifecycle`). The version
    /// this replaces took the lifecycle lock first and then waited for a
    /// fill of the same inode — a download of any length — holding up every
    /// registration and Forget meanwhile. Now the file is opened and its
    /// inode lock taken first, and the lifecycle lock after: the
    /// registration is looked at again under it, and a folder forgotten or
    /// registered anew meanwhile is refused, with nothing punched.
    async fn free_one(&self, path: &Path, wait: Wait) -> Result<(u64, String), SyncError> {
        let reg = self.require_registration()?;
        if reg.intercepted {
            // Refused before a wait that could only end in the same refusal.
            self.require_link()?;
        }
        let root = reg.root.clone();
        let target = path.to_path_buf();
        let (file, shown) = tokio::task::spawn_blocking(move || open_shown(&root, &target))
            .await
            .map_err(|e| SyncError::Io(format!("the dehydration task failed: {e}")))??;
        let key = InodeKey::of(&file).map_err(|e| SyncError::Io(e.to_string()))?;

        let _guard = match wait {
            Wait::Yes => self.locks.lock(key).await,
            Wait::No => self.locks.try_lock(key).ok_or(SyncError::InUse)?,
        };
        // A change waiting to be uploaded is only here (`docs/design/writes.md` §11):
        // looked at under the inode lock, and refused when it cannot be told.
        self.refuse_unuploaded(&file, &shown).await?;
        let _lifecycle = self.lifecycle.read().await;
        let reg = match self.registration() {
            Some(now) if now.root.path == reg.root.path && now.root.root_id == reg.root.root_id => now,
            _ => return Err(SyncError::NoRoot),
        };
        // local rule decides at the punch (`Clearance`,
        // `root::dehydrate_opened`). An intercepted root is refused outright
        // without its link: freed up while nothing intercepts, the file
        // would read zeros until the helper is back. A root registered
        // without interception reads zeros by design, and goes by the rule:
        // its link if it has one — the helper then clears the mark, which it
        // grants on ownership of the file alone — or, with none, whether a
        // helper is running at all.
        let clearance = if reg.intercepted {
            Clearance::Link(self.require_link()?)
        } else {
            self.clearance()
        };
        // Measured under the lock, so no fill of the same file changes it in
        // between, and on a second descriptor for the same inode, since the
        // first is handed over whole.
        let probe = file.try_clone().map_err(|e| SyncError::Io(e.to_string()))?;
        let blocks = |file: &File| file.metadata().map(|m| m.blocks()).unwrap_or(0);
        let before = blocks(&probe);
        root::dehydrate_opened(&clearance, file).await.map_err(SyncError::from)?;
        Ok((before.saturating_sub(blocks(&probe)) * 512, shown))
    }

    /// `ActivityLog.Recent(limit)`: the newest `limit` events, newest first.
    pub async fn recent_activity(&self, limit: u32) -> Result<Vec<activity::Event>, SyncError> {
        let report = self.report.clone();
        tokio::task::spawn_blocking(move || report.activity.recent(limit as usize))
            .await
            .map_err(|e| SyncError::Io(format!("the activity task failed: {e}")))?
            .map_err(|e| SyncError::Io(e.to_string()))
    }

    /// `Conflicts.List()`: (time, original, rescued), newest first; one whose
    /// rescued file is gone is dropped on the way.
    pub async fn conflicts(&self) -> Result<Vec<crate::tree::ConflictRow>, SyncError> {
        let report = self.report.clone();
        tokio::task::spawn_blocking(move || report.activity.conflicts())
            .await
            .map_err(|e| SyncError::Io(format!("the conflicts task failed: {e}")))?
            .map_err(|e| SyncError::Io(e.to_string()))
    }

    /// `Conflicts.Dismiss(rescued_path)`: the conflict comes off the list, and
    /// the file stays where it is. A path that names no conflict is refused
    /// with that path in the refusal.
    pub async fn dismiss_conflict(&self, rescued: &str) -> Result<(), SyncError> {
        let (report, path) = (self.report.clone(), rescued.to_owned());
        let removed = tokio::task::spawn_blocking(move || report.activity.dismiss(&path))
            .await
            .map_err(|e| SyncError::Io(format!("the conflicts task failed: {e}")))?
            .map_err(|e| SyncError::Io(e.to_string()))?;
        if removed {
            Ok(())
        } else {
            Err(SyncError::NoConflict(rescued.to_owned()))
        }
    }

    /// `LastChecked`, `LocalBytes`, `Conflicts.Count`.
    pub fn status(&self) -> (i64, u64, u32) {
        let s = self.state.get();
        (s.last_checked, s.local_bytes, s.conflict_count)
    }

    /// `Transfers`: every download under way, as (path, bytes done, total).
    pub fn transfers(&self) -> Vec<(String, u64, u64)> {
        self.report.transfers.list().into_iter().map(|t| (t.path, t.done, t.total)).collect()
    }

    /// `Transfers.LargeFiles` (issue #50): the large files the sync moves now, each once, the
    /// files being opened left out ([`activity::large_files`]).
    pub fn large_files(&self) -> u32 {
        let downloads = self.report.transfers.subscribe().borrow().clone();
        activity::large_files(&downloads, &self.state.get().uploads)
    }

    /// `Files.WebUrl`: the address of the page OneDrive's web interface has for
    /// the file or folder at `path`. The path is opened as a pin's is
    /// (`SyncRoot::open_item`: beneath the root, no link followed, nothing
    /// downloaded) for its item id, and OneDrive is asked for that item — one
    /// GET, with the drive client's own retries. Nothing is changed, here or
    /// in OneDrive, and nothing is remembered.
    ///
    /// Refused `NotInOneDrive` for an item with no id (not uploaded yet),
    /// `NotSignedIn` with no drive or no token, `Unreachable` when OneDrive
    /// does not answer.
    pub async fn web_url(&self, path: &Path) -> Result<String, SyncError> {
        let reg = self.require_registration()?;
        let (root, target) = (reg.root.clone(), path.to_path_buf());
        let (id, shown) = tokio::task::spawn_blocking(move || -> Result<_, SyncError> {
            let (item, shown) = root.open_item(&target)?;
            let id = konedrive_fs::placeholder::read_item_id(&item)
                .map_err(|e| SyncError::Io(format!("{}: {e}", shown.display())))?;
            Ok((id, shown.display().to_string()))
        })
        .await
        .map_err(|e| SyncError::Io(format!("reading the item failed: {e}")))??;
        let Some(id) = id else { return Err(SyncError::NotInOneDrive(shown)) };
        let drive = self.drive.lock().unwrap().clone().ok_or(SyncError::NotSignedIn)?;
        page_of(drive.item(&id).await, &shown)
    }

    /// `Files.WebUrl` of the account's folder itself: the address of the page
    /// of the drive's root. One GET, as [`web_url`](Self::web_url).
    pub async fn root_web_url(&self) -> Result<String, SyncError> {
        let reg = self.require_registration()?;
        let drive = self.drive.lock().unwrap().clone().ok_or(SyncError::NotSignedIn)?;
        page_of(drive.root_item().await, &reg.root.path.display().to_string())
    }

    /// The file's own state, or `not-managed` for anything that is not a
    /// plain file this daemon actually manages inside the current root —
    /// including a file outside the root altogether, per.
    ///
    /// # A query never opens the file
    ///
    /// The state is read with `lgetxattr` on the path. Opening the file
    /// instead made `ItemState` a *download*: under a marked directory, the
    /// open of an `online-only` file is intercepted and the whole file is
    /// fetched as a side effect of asking what state it is in — and where
    /// nothing can serve it, the denial makes the open fail and this answers
    /// `not-managed` for a genuinely managed placeholder, which is simply a
    /// wrong answer. Opening it also blocks: `ItemState` on a FIFO inside
    /// the folder parked the D-Bus dispatch task forever.
    pub async fn item_state(&self, path: &Path) -> String {
        const NOT_MANAGED: &str = "not-managed";
        let Some(reg) = self.registration() else {
            return NOT_MANAGED.into();
        };
        let path = path.to_path_buf();
        // `canonicalize` and `getxattr` are blocking syscalls
        // and do not belong on the zbus dispatch task.
        tokio::task::spawn_blocking(move || {
            let Ok(canonical) = std::fs::canonicalize(&path) else {
                return NOT_MANAGED.to_owned();
            };
            if !canonical.starts_with(&reg.root.path) {
                return NOT_MANAGED.to_owned();
            }
            match state_of_path(&canonical) {
                Some(state) => state.as_str().to_owned(),
                None => NOT_MANAGED.to_owned(),
            }
        })
        .await
        .unwrap_or_else(|_| NOT_MANAGED.to_owned())
    }
}

/// The page's address out of OneDrive's answer about the item shown as `shown`.
fn page_of(answer: Result<crate::drive::DriveItem, crate::drive::DriveError>, shown: &str) -> Result<String, SyncError> {
    use crate::drive::DriveError;
    match answer {
        Ok(item) => item
            .web_url
            .filter(|url| !url.is_empty())
            .ok_or_else(|| SyncError::Io(format!("OneDrive gave no address for the page of {shown}"))),
        Err(DriveError::SignedOut) => Err(SyncError::NotSignedIn),
        Err(DriveError::Transient(why)) => Err(SyncError::Unreachable(why)),
        Err(DriveError::NotFound) => Err(SyncError::Io(format!("{shown} is not in OneDrive any more"))),
        Err(other) => Err(SyncError::Io(format!("asking OneDrive for the page of {shown}: {other}"))),
    }
}

/// A file opened through the gate (`SyncRoot::open_inside`), and its full
/// path as the activity log names it: inside the root as it was registered,
/// however `path` spelled it (a link on the way, `..`) — events are kept
/// only for paths inside the registered folder.
fn open_shown(root: &SyncRoot, path: &Path) -> Result<(File, String), DehydrateError> {
    let file = root.open_inside(path)?;
    let shown = root.relative(path).map(|rel| root.path.join(rel)).unwrap_or_else(|_| path.to_path_buf());
    Ok((file, shown.display().to_string()))
}

/// Whether a free-up waits for the per-inode lock ([`SyncService::free_one`]).
#[derive(Clone, Copy)]
enum Wait {
    Yes,
    No,
}

/// What `FreeUpSpace()` and `FreeUp()` did: files freed up, the bytes of
/// blocks that gave back, files left as they were because they were in use
/// or changed here, and downloaded files left because a pin keeps them on
/// this device. `FreeUp` answers `busy + modified` as its `busy`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FreedUp {
    pub files: u32,
    pub bytes: u64,
    pub busy: u32,
    pub modified: u32,
    pub pinned: u32,
}

/// A path `Pin`, `Unpin` or `FreeUp` was given, opened beneath the root
/// (`SyncRoot::open_item`) and looked at, before anything changes.
struct PinTarget {
    item: File,
    /// Its full path as the activity log names it.
    shown: PathBuf,
    is_dir: bool,
    /// It carries a pin of its own.
    own: bool,
    /// The folders above it, up to the root, that carry a pin: nearest first.
    above: Vec<PathBuf>,
}

/// Opens and looks at each of `paths`; one that cannot be — outside the
/// root, a `.konedrive-*` name, a file that is not ours — refuses them all.
/// Blocking.
fn pin_targets(root: &SyncRoot, paths: &[PathBuf]) -> Result<Vec<PinTarget>, SyncError> {
    paths
        .iter()
        .map(|path| {
            let (item, shown) = root.open_item(path)?;
            let io = |e: io::Error| SyncError::Io(format!("{}: {e}", shown.display()));
            let is_dir = item.metadata().map_err(io)?.is_dir();
            let own = konedrive_fs::placeholder::read_pin(&item).map_err(io)?;
            let above = pin::pinned_ancestors(&root.path, &shown);
            Ok(PinTarget { item, shown, is_dir, own, above })
        })
        .collect()
}

/// Why pins cannot come off `targets`: a folder above one of them pins it
/// and stays pinned — it is not itself one of them with its own pin, which
/// the same call takes off. A path with a pin of its own under a pinned
/// folder is refused too: taking its pin off would leave it pinned.
fn kept_by_folder(targets: &[PinTarget]) -> Option<SyncError> {
    let coming_off: std::collections::HashSet<&Path> =
        targets.iter().filter(|target| target.own).map(|target| target.shown.as_path()).collect();
    targets
        .iter()
        .flat_map(|target| target.above.iter().map(move |folder| (target, folder)))
        .find(|(_, folder)| !coming_off.contains(folder.as_path()))
        .map(|(target, folder)| SyncError::NotAllowed(pin::refusal(&target.shown, folder)))
}

/// A `freed` event's detail for more than one file: "2 files, 1.5 MiB".
fn freed_detail(files: u32, bytes: u64) -> String {
    format!("{files} file{}, {}", if files == 1 { "" } else { "s" }, activity::human_size(bytes))
}

/// Whether [`SyncService::hydrate_now`] still has work to do.
enum Fill {
    /// It does. `may_be_marked` is false only for an `online-only` file:
    /// every other state it can be found in is one an ignore mark can be
    /// on (see `source::hydrate_with`).
    Needed { may_be_marked: bool },
    AlreadyThere,
}

/// What a file's own state says about whether it needs filling.
///
/// # The `hydrated` label is not believed on its own
///
/// `check_dehydratable` verifies the stamp before it punches; this did not,
/// so a file whose `user.konedrive.state` says `hydrated` over a hole —
/// measured: 4096 bytes, 0 blocks, hand-labelled — reported success from
/// `Hydrate` and stayed empty. §9 names that state exactly, and repairing it
/// is what a manual "download it now" is for.
///
/// The three cases are told apart deliberately:
///
/// - **no stamp at all** → fill. Nothing this daemon wrote can be in that
///   state: `fill` writes the stamp *before* it writes `hydrated`, so a
///   `hydrated` file with no stamp was labelled by something else, and its
///   content is exactly as unproven as an `online-only` file's.
/// - **a stamp that matches** → nothing to do.
/// - **a stamp that does not match** → refuse with "modified locally",
///   never fill. There is no upload in this sub-project, so a local edit is
///   the only copy of that data (§8), and overwriting it with remote content
///   would be the same class of permanent loss this whole component exists
///   to avoid — just pointing the other way. Refusing is loud, and it leaves
///   the user a file they can still copy out.
///
/// A zero-byte file is `hydrated` from birth and carries no stamp by design
/// (`create_placeholder`), so it is answered before any of that.
fn classify_for_hydration(file: &File) -> Result<Fill, SyncError> {
    match read_state(file) {
        Ok(None) => Err(SyncError::NotManaged),
        Ok(Some(State::Hydrated)) => {
            let empty = file.metadata().map_err(|e| SyncError::Io(e.to_string()))?.len() == 0;
            if empty {
                return Ok(Fill::AlreadyThere);
            }
            match read_stamp(file).map_err(|e| SyncError::Io(e.to_string()))? {
                None => Ok(Fill::Needed { may_be_marked: true }),
                Some(_) => {
                    if stamp_matches(file).map_err(|e| SyncError::Io(e.to_string()))? {
                        Ok(Fill::AlreadyThere)
                    } else {
                        Err(SyncError::ModifiedLocally)
                    }
                }
            }
        }
        // `dehydrating` is not "somebody is busy with it": under the
        // per-inode lock this call holds, no dehydration of this inode can
        // be running. It is what a crash — or a cancelled `Dehydrate`
        // (`root::dehydrate`'s) — left behind, and §5.2 says
        // exactly what to do with it: treat it as "hydrate it again".
        // Reporting success over whatever the punch got to is the one thing
        // that must not happen.
        Ok(Some(State::OnlineOnly)) => Ok(Fill::Needed { may_be_marked: false }),
        Ok(Some(State::Hydrating | State::Dehydrating)) => Ok(Fill::Needed { may_be_marked: true }),
        Err(StateError::Io(e)) => Err(SyncError::Io(e.to_string())),
        Err(StateError::Corrupt(value)) => {
            Err(SyncError::Io(format!("unrecognised state {value:?}")))
        }
    }
}

/// One file's state read by name, with no open at all.
/// `xattr::get` is `lgetxattr`: it does not follow a final symlink, and the
/// path it is given has already been canonicalized.
fn state_of_path(path: &Path) -> Option<State> {
    let raw = xattr::get(path, XATTR_STATE).ok()??;
    String::from_utf8_lossy(&raw).parse().ok()
}

/// `SyncService` is itself a valid, if initially empty, `ContentSource`:
/// `serve_hydrations` is started once, at daemon startup,
/// before any root — let alone any source directory — necessarily exists
/// yet. Delegating to whatever `populate_from_directory` most recently
/// registered means `serve_hydrations` does not need to be restarted (or
/// handed a source through some other side channel) once a root and a
/// source do exist.
#[async_trait]
impl ContentSource for SyncService {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let source = self.source.lock().unwrap().clone();
        match source {
            Some(source) => source.fetch(item_id, from, end).await,
            None => Err(SourceError::NotFound(format!(
                "{item_id}: no content source is registered"
            ))),
        }
    }
}

/// A pinned download is an ordinary fill ([`SyncService::fill_now`]):
/// verified, checkpointed, shown in `Transfers` and recorded as
/// `downloaded` or `failed`. A file whose pin was taken off since it was
/// queued, or whose folder was forgotten, is passed over.
#[async_trait]
impl pin::PinFill for SyncService {
    async fn fill_pinned(&self, path: &Path) -> pin::Filled {
        let Some(reg) = self.registration() else { return pin::Filled::Done };
        let (root, target) = (reg.root.path.clone(), path.to_path_buf());
        let still = tokio::task::spawn_blocking(move || pin::pinned_by(&root, &target).is_some())
            .await
            .unwrap_or(false);
        if !still {
            return pin::Filled::Done;
        }
        // The pins' worker holds a slot of the pool for it.
        match self.fill_now(path, None).await {
            Ok(Answered::Failed(FillError::Errno(errno))) if errno == libc::ENOSPC || errno == libc::EDQUOT => {
                pin::Filled::NoSpace
            }
            Ok(Answered::Failed(_)) => pin::Filled::Failed,
            Ok(_) => pin::Filled::Done,
            Err(e) => {
                tracing::info!("{} is kept on this device but was not downloaded: {e}", path.display());
                pin::Filled::Failed
            }
        }
    }
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The recursive half of `populate_from_directory`: mirrors `source` into
/// `dest` as placeholders, marking every newly-created directory before
/// anything is created inside it (invariant M1) and skipping any name that
/// already exists. `relative` accumulates the `item_id` — the entry's path
/// relative to the original `source_dir` — as the walk descends.
///
/// This is explicitly the offline test path (own description),
/// not production-hardened infrastructure: unlike `root::recover`, it walks
/// by path rather than by directory descriptor, because its threat model is
/// "a local directory the same user built for a test", not an adversarial
/// or racing filesystem. It still walks a directory the user names, though,
/// and a symlink is never treated as one to recurse into — `entry.file_type`
/// is `lstat`-based and already reports a symlink as a symlink rather than
/// as whatever it points at, but that is std's own default, not a decision
/// this function makes, so it is spelled out below rather than leaned on
/// implicitly: a symlink back at one of its own ancestors is exactly the
/// shape that turns "walk the tree" into recursion with no base case, and
/// nothing here may ever decide to recurse on the strength of what a
/// symlink's target happens to be. A symlink to a regular file is still
/// picked up as one, through the same follow `std::fs::metadata` performs
/// for a plain file's own size and mtime — that follows the link exactly
/// once (the kernel bounds the rest of any chain on its own), and is a
/// read, never a walk decision — unless it leads into the sync folder, or
/// to one of konedrive's own files anywhere (a hardlink to a placeholder):
/// then the whole populate is refused, since a file filled
/// from it would be filled with a placeholder's zeros.
/// What every level of [`populate_walk`] needs besides where it is: the
/// link to mark new directories through, if any, and the resolved sync
/// folder that no source file may lead into.
#[derive(Clone, Copy)]
struct Walk<'a> {
    link: Option<&'a HelperLink>,
    root: &'a Path,
}

/// A source file refused because it leads into the sync folder:
/// `PopulateFromDirectory` answers `Unsupported` with this text.
#[derive(Debug)]
struct RefusedSource(String);

impl std::fmt::Display for RefusedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RefusedSource {}

fn populate_walk<'a>(
    walk: Walk<'a>,
    source: &'a Path,
    dest: &'a Path,
    relative: &'a Path,
) -> BoxFuture<'a, io::Result<u64>> {
    Box::pin(async move {
        let mut created = 0u64;
        let listed = source.to_path_buf();
        let entries = on_blocking_thread(move || list_source(&listed)).await??;
        for (name, file_type) in entries {
            let dest_path = dest.join(&name);
            let relative_path = relative.join(&name);
            if file_type.is_dir() {
                // Invariant M1, both halves. The directory is marked
                // **before** anything is created inside it, which is why the
                // mark happens here and the recursion below it. And it is
                // marked whether or not this run is the one that created it:
                // the version this replaces marked only inside
                // `if !dest_path.exists()`, so a crash between `create_dir`
                // and `mark_dir` left a directory that every later run
                // skipped — permanently unmarked, and everything under it
                // permanently uninterceptable, until the helper's next
                // startup walk happened to cover it.
                let target = dest_path.clone();
                let handle = on_blocking_thread(move || ensure_dir(&target)).await??;
                if let Some(link) = walk.link {
                    link.mark_dir(&handle).await.map_err(|e| {
                        io::Error::other(format!("cannot mark {}: {e}", dest_path.display()))
                    })?;
                }
                drop(handle);
                created +=
                    populate_walk(walk, &source.join(&name), &dest_path, &relative_path).await?;
            } else if file_type.is_file() || file_type.is_symlink() {
                let source_path = source.join(&name);
                let dest_dir = dest.to_path_buf();
                // The item id is the entry's path *relative to the source
                // root* — `sub/b.bin`, not `b.bin` — which is exactly the
                // name `LocalDir` resolves a fetch by. A bare file name
                // gives every nested placeholder an id that fetches nothing.
                let item_id = relative_path.to_string_lossy().into_owned();
                created +=
                    on_blocking_thread({
                        let root = walk.root.to_path_buf();
                        move || make_placeholder(&source_path, &root, &dest_dir, &name, &item_id)
                    })
                        .await??;
            }
        }
        Ok(created)
    })
}

/// Runs one blocking step of the populate walk off the reactor: `read_dir`,
/// `stat`, `create_dir`, `openat` and `linkat` are all
/// blocking syscalls, and this runs from a zbus dispatch task.
async fn on_blocking_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| io::Error::other(format!("the populate task failed: {e}")))
}

/// One source directory's entries, in a stable order. `file_type` is
/// `lstat`-based, so a symlink is reported as a symlink rather than as
/// whatever it points at.
fn list_source(source: &Path) -> io::Result<Vec<(OsString, std::fs::FileType)>> {
    let mut entries: Vec<_> = std::fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    entries
        .into_iter()
        .map(|e| Ok((e.file_name(), e.file_type()?)))
        .collect()
}

/// The destination directory, created if it is not there yet, opened as a
/// directory. `O_NOFOLLOW` so a symlink planted under that name is never
/// what gets marked and populated.
fn ensure_dir(path: &Path) -> io::Result<File> {
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let flags = nix::fcntl::OFlag::O_RDONLY
        | nix::fcntl::OFlag::O_DIRECTORY
        | nix::fcntl::OFlag::O_NOFOLLOW
        | nix::fcntl::OFlag::O_CLOEXEC;
    let fd = nix::fcntl::open(path, flags, nix::sys::stat::Mode::empty())
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    Ok(File::from(fd))
}

/// Mirrors one source file as a placeholder; returns how many were created
/// (0 when the name is already taken, or the source is not a regular file).
fn make_placeholder(
    source_path: &Path,
    root: &Path,
    dest_dir: &Path,
    name: &OsString,
    item_id: &str,
) -> io::Result<u64> {
    if dest_dir.join(name).exists() {
        return Ok(0);
    }
    // A real regular file's own metadata, or — for a symlink — its
    // target's: `fs::metadata` follows, which is exactly what is wanted
    // here and never what decided whether to recurse. A symlink whose
    // target is a directory, that is dangling, or that points at anything
    // else, is left alone: `is_file()` on the target is the only thing that
    // turns a symlink into a placeholder.
    let Ok(meta) = std::fs::metadata(source_path) else {
        return Ok(0);
    };
    if !meta.is_file() {
        return Ok(0);
    }
    // A file that leads into the folder — a symlink to a
    // placeholder there, a hardlink to one — would be filled from that
    // placeholder's zeros. Refused before anything is created for it.
    if let Some(why) = source::refused_source_path(source_path, root)? {
        return Err(io::Error::other(RefusedSource(format!(
            "{} cannot be a source file: {why}, and a file filled from it would be filled with a \
             placeholder's zeros",
            source_path.display()
        ))));
    }
    let dir_handle = File::open(dest_dir)?;
    konedrive_fs::placeholder::create_placeholder(
        &dir_handle,
        &name.to_string_lossy(),
        item_id,
        meta.len(),
        meta.modified()?,
    )?;
    Ok(1)
}

/// Asks a OneDrive folder's sync for a cycle each time the account becomes
/// signed in from any other state. A cycle that finds the account signed out
/// fails as blocking trouble and is retried only on the poller's schedule, so
/// without this a folder reading "signed out" would keep saying so for up to
/// a poll interval after the sign-in. Runs as long as the sync does.
async fn nudge_on_sign_in(
    mut account: watch::Receiver<crate::state::AccountSnapshot>,
    syncing: Arc<Mutex<Option<Syncing>>>,
) {
    let mut last = account.borrow_and_update().state;
    while account.changed().await.is_ok() {
        let now = account.borrow_and_update().state;
        if now == SignInState::SignedIn && last != SignInState::SignedIn {
            if let Some(syncing) = syncing.lock().unwrap().as_ref() {
                syncing.poller.refresh();
            }
        }
        last = now;
    }
}

/// The tree store's files: the database and SQLite's journal beside it.
fn remove_tree_files(tree_db: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = tree_db.as_os_str().to_owned();
        name.push(suffix);
        let file = PathBuf::from(name);
        if let Err(e) = std::fs::remove_file(&file) {
            if e.kind() != io::ErrorKind::NotFound {
                tracing::warn!("cannot remove {}: {e}", file.display());
            }
        }
    }
}

/// Keeps `service`'s helper link alive for the life of the daemon: its hub's
/// [`hub::supervise`], which brings up every account on the hub.
pub async fn supervise_helper(service: Arc<SyncService>, socket_path: PathBuf, backoff: Duration) {
    let hub = Arc::clone(service.hub());
    drop(service);
    hub::supervise(hub, socket_path, backoff).await
}

/// The longest [`hub::supervise`] ever waits between attempts.
pub const MAX_HELPER_BACKOFF: Duration = Duration::from_secs(30);

/// Keeps the `HelperState` of `service`'s hub current ([`hub::watch`]).
pub async fn watch_helper(service: Arc<SyncService>) {
    watch_helper_every(service, helper_status::RECHECK).await
}

/// [`watch_helper`], asking systemd again every `every` while there is no
/// link (tests: well under a second).
pub async fn watch_helper_every(service: Arc<SyncService>, every: Duration) {
    let hub = Arc::clone(service.hub());
    drop(service);
    hub::watch_every(hub, every).await
}

#[cfg(test)]
mod tests;
