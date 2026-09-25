//! Everything this sub-project adds to the daemon: the helper link, the
//! content source, the hydration loop, and `SyncService` — the `org.konedrive.Sync1`
//! D-Bus surface's own half of the work (`dbus.rs` is the thin zbus wrapper
//! around it, the same split `crate::account`/`crate::dbus` uses for
//! `Account1`). There is one `SyncService` per account; the helper link, its
//! supervisor and the per-inode locks are the daemon's, in `hub.rs`.

pub mod activity;
pub mod baloo;
pub mod dbus;
pub mod disk;
pub mod graph_source;
pub mod helper;
pub mod helper_status;
pub mod hub;
pub mod listing;
pub mod materialize;
pub mod network;
pub mod pin;
pub mod root;
pub mod source;
pub mod thumbs;

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

use crate::config::{ConfigStore, RootConfig};
use crate::state::{SignInState, StateHandle};

/// Fills served on open at once ([`serve_hydrations`]); pinned downloads
/// have as many slots again of their own ([`pin::PIN_SLOTS`]).
pub const FILL_SLOTS: usize = 4;

/// Answers hydration requests until the helper goes away. At most four run at
/// once; everything else waits, and no request is ever dropped silently.
///
/// The permit is acquired *before* spawning, not inside the
/// spawned task. Acquiring it inside the task would drain the bounded mpsc
/// of hydration requests into an unbounded pile of tasks — each holding a
/// suspended open's event descriptor — as fast as the helper could send
/// them, destroying the backpressure the channel exists to provide.
/// Blocking here instead, before `recv()` is called again, propagates that
/// backpressure all the way back to the helper — but only as far as the
/// request queue. It must never reach the socket: the reader thread that
/// fills the queue is also the one that reads the `Ack` each fill below
/// waits for before it lets go of its permit, so a reader stopped by a full
/// queue with requests still ahead of an `Ack` in the socket wedges all four
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
    serve(link, requests, locks, Fillers::One(source, report)).await;
}

/// Who fills a hydration request, and where it is reported.
#[derive(Clone)]
enum Fillers {
    /// One source, one report, whatever the file (tests, the VM suite).
    One(Arc<dyn ContentSource>, Report),
    /// The account the file belongs to ([`hub::HelperHub::route`]): the
    /// daemon's.
    Routed(Arc<hub::HelperHub>),
}

impl Fillers {
    async fn route(&self, fd: &std::os::fd::OwnedFd) -> Option<(Arc<dyn ContentSource>, Report)> {
        match self {
            Fillers::One(source, report) => Some((Arc::clone(source), report.clone())),
            Fillers::Routed(hub) => hub.route(fd).await.map(hub::filler),
        }
    }
}

/// The loop behind [`serve_hydrations_reporting`] and the hub's: at most
/// [`FILL_SLOTS`] fills at once, the four slots shared by every account.
async fn serve(
    link: HelperLink,
    mut requests: tokio::sync::mpsc::Receiver<HydrateRequest>,
    locks: InodeLocks,
    fillers: Fillers,
) {
    let permits = Arc::new(tokio::sync::Semaphore::new(FILL_SLOTS));
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
            let Some((source, report)) = fillers.route(&fd).await else {
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
            let tracked = Tracked::new(Arc::clone(&source), report.transfers.clone(), shown.clone());
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
            let filled = AssertUnwindSafe(source::answer_request(fd, &tracked, Some(&link)))
                .catch_unwind()
                .await;
            let size = tracked.fetched();
            // Whatever came of it, the download is over.
            drop(tracked);
            let (errno, event) = match filled {
                Ok(answered) => (answered.errno(), fill_event(&answered, &shown, size)),
                Err(_) => {
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
                .or_insert_with(|| Slot { mutex: Arc::new(tokio::sync::Mutex::new(())), users: 0 });
            slot.users += 1;
            Arc::clone(&slot.mutex)
        };
        // Armed *before* the await, so a caller whose future is dropped
        // while it is parked below still takes itself out of the count. A
        // D-Bus method's future is dropped whenever its caller goes away,
        // and `hydrate_now` can park here for as long as another fill of the
        // same file takes, which has no time limit.
        let row = Row { table: Arc::clone(&self.inner), key };
        let guard = mutex.lock_owned().await;
        InodeGuard { _guard: guard, _row: row }
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
                .or_insert_with(|| Slot { mutex: Arc::new(tokio::sync::Mutex::new(())), users: 0 });
            slot.users += 1;
            Arc::clone(&slot.mutex)
        };
        // Counted like any other user until it gives up: dropped with the
        // refusal, it takes itself out of the count and, if it was the only
        // one, the row out of the table.
        let row = Row { table: Arc::clone(&self.inner), key };
        let guard = mutex.try_lock_owned().ok()?;
        Some(InodeGuard { _guard: guard, _row: row })
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
    _row: Row,
}

// --- `org.konedrive.Sync1`'s own half of the work ------------------------
//
// `dbus.rs` is the thin zbus wrapper (the same split `crate::account` /
// `crate::dbus` uses for `Account1`); everything that actually does
// something lives here, so it can be exercised without a bus at all.

/// What `RootState` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootState {
    /// No root is registered.
    None,
    /// A root is registered and, as far as this daemon knows, healthy.
    Ready,
    /// A root is registered, but **nothing intercepts opens inside it**
    ///: it was registered through
    /// `RegisterRootWithoutInterception`, so a placeholder nobody fills
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
/// `Account1`.
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
    /// `ConflictCount`: conflicts whose rescued file is still there.
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
/// (`Conflicts()`, `ConflictCount`), not a problem, and is not said here
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
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(". ")
}

/// Shared, observable sync state (see `state::StateHandle`, the same shape
/// for `Account1`).
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

    pub fn subscribe(&self) -> watch::Receiver<SyncSnapshot> {
        self.tx.subscribe()
    }
}

/// Everything `Sync1` can refuse, flattened from `RegisterError` and
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
    /// `Sync1` answers it `NotEmpty`.
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
    #[error("{0}")]
    Io(String),
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
/// `org.konedrive.Sync1` exposes, and what `org.konedrive.Files1` routes to it.
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
}

/// A registered root and how — or whether — opens inside it are intercepted.
#[derive(Clone)]
struct Registration {
    root: SyncRoot,
    /// False only for a root registered through
    /// `RegisterRootWithoutInterception`.
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
            // The pins' downloads go through this very service, which they must
            // not keep alive: a weak reference.
            Arc::new_cyclic(|me: &std::sync::Weak<Self>| Self {
                pins: pin::Pins::new(state.clone(), me.clone()),
                hub: Arc::clone(hub),
                link: hub.link_cell(),
                account,
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
            })
        })
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
        *self.drive.lock().unwrap() = Some(drive);
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
    /// (`Accounts1.Remove`). Called with `lifecycle` held for writing.
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
    /// shared by a `RegisterRoot` call, a `RegisterRootWithoutInterception`
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
    /// daemon will accept for `RegisterRootWithoutInterception` — and then
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

    /// `Accounts1.Remove`'s first step: the folder forgotten exactly as
    /// [`unregister_root`](Self::unregister_root) forgets it — refused under
    /// the same rule — and, under the same `lifecycle` lock so that nothing
    /// comes in between, the account retired: no registration, bring-up or
    /// switch is made for it from then on. An account with no folder is
    /// retired all the same.
    pub async fn retire(&self) -> Result<(), SyncError> {
        self.forget(true).await
    }

    /// [`unregister_root`](Self::unregister_root) and [`retire`](Self::retire).
    async fn forget(&self, retire: bool) -> Result<(), SyncError> {
        // The tasks only, outside the lock; the activity is let go of under
        // it (B-M1), where no reconnect can have started a sync meanwhile.
        let was_syncing = self.stop_tasks().await;
        let _lifecycle = self.lifecycle.write().await;
        let was_syncing = self.stop_tasks().await || was_syncing;
        if was_syncing {
            self.let_go_of_activity().await;
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
            self.sync_cannot_start(format!(
                "{} shows OneDrive, but no drive is configured; it is not kept in step",
                reg.root.path.display()
            ));
            return;
        };
        // Files are downloaded from the drive whether or not the folder can
        // be kept in step.
        let source: Arc<dyn ContentSource> = Arc::new(graph_source::GraphSource::new(drive.clone()));
        *self.source.lock().unwrap() = Some(Arc::clone(&source));
        let tree_db = paths.tree_db.clone();
        let store = match tokio::task::spawn_blocking(move || crate::tree::TreeStore::open(&tree_db)).await {
            Ok(Ok(store)) => crate::tree::Store::new(store),
            Ok(Err(e)) => return self.sync_cannot_start(format!("the tree store cannot be opened: {e}")),
            Err(e) => return self.sync_cannot_start(format!("the tree store cannot be opened: {e}")),
        };
        // The activity log and the conflicts are kept in this
        // store from now on, and `LastChecked` is where the last run left it.
        // Every caller holds `lifecycle` for writing, so no other start can
        // attach a store of its own meanwhile.
        let (report, attached, folder) = (self.report.clone(), store.clone(), reg.root.path.clone());
        let last_checked = tokio::task::spawn_blocking(move || {
            report.activity.attach(attached.clone(), &folder);
            attached.with(|s| s.meta("last_checked")).ok().flatten().and_then(|v| v.parse::<i64>().ok())
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(0);
        self.state.update(|s| s.last_checked = last_checked);
        // The account's drive, as `config.toml` keeps it (A-M5, design §8.1):
        // the same-account check then survives a tree store rebuilt empty.
        let drive_record = self.persist.clone().map(|persist| {
            let recorded = persist.store.account(&persist.account).map(|a| a.drive_id).filter(|d| !d.is_empty());
            listing::DriveRecord { store: persist.store, account: persist.account, recorded }
        });
        // Nudges the thumbnail filler right after a cycle, rather than making
        // it wait out its own idle timer.
        let kick = Arc::new(Notify::new());
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
        });
        let schedule = self.schedule.lock().unwrap().clone();
        // Checked again and kept in one critical section: a second start that
        // passed the check at the top while the store opened must leave the
        // first sync alone. Replacing it would drop a `Poller` that runs on
        // with nothing left to stop it; the lifecycle lock every caller holds
        // is what keeps two starts apart, not this.
        let mut syncing = self.syncing.lock().unwrap();
        if syncing.is_some() {
            return;
        }
        *self.store.lock().unwrap() = Some(store.clone());
        let poller = listing::Poller::start(listing, schedule);
        let sign_in_watch = self
            .account
            .as_ref()
            .map(|account| tokio::spawn(nudge_on_sign_in(account.subscribe(), Arc::clone(&self.syncing))));
        // Its own task, stopped with the poller: a slow thumbnail request
        // never holds up the reconcile. None at all without a cache to fill.
        let thumbnails = paths.thumbnails.clone().map(|cache| {
            let cancel = CancellationToken::new();
            let task = thumbs::ThumbnailFiller::new(drive, store, reg.root.clone(), cache).spawn(kick, cancel.clone());
            (task, cancel)
        });
        *syncing = Some(Syncing { poller, sign_in_watch, thumbnails });
    }

    /// Why a OneDrive folder is not kept in step, said as blocking trouble:
    /// nothing retries it on its own; a `Refresh()` does, as
    /// does bringing the folder up again.
    fn sync_cannot_start(&self, text: String) {
        tracing::error!("{text}");
        self.state.update(|s| s.sync_trouble = Some(SyncTrouble { text, blocking: true }));
    }

    /// Stops the sync and waits for it: a Forget's, and tests'. (The daemon
    /// has no orderly shutdown; nothing stops the sync when it exits.)
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
        if let Some(watch) = syncing.sign_in_watch {
            watch.abort();
        }
        // Told before the poller is waited for, so that both wind down at
        // once.
        if let Some((_, cancel)) = &syncing.thumbnails {
            cancel.cancel();
        }
        syncing.poller.stop().await;
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
    /// `RegisterRootWithoutInterception` of the very folder the helper still
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
        if self.nudge() {
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

    /// A cycle now, if a OneDrive folder is syncing (the network came back).
    pub fn refresh_now(&self) {
        self.nudge();
    }

    /// [`refresh_now`](Self::refresh_now); whether a sync was running to
    /// nudge.
    fn nudge(&self) -> bool {
        match self.syncing.lock().unwrap().as_ref() {
            Some(syncing) => {
                syncing.poller.refresh();
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
            store.with(|s| s.skipped(crate::tree::Table::Items))
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
        match self.fill_now(path).await? {
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
    async fn fill_now(&self, path: &Path) -> Result<Answered, SyncError> {
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
        // Shown in `Transfers` while it downloads.
        let tracked = Tracked::new(source, self.report.transfers.clone(), shown.clone());
        let filled = source::hydrate_with(fd, &tracked, clearance.as_ref()).await;
        let size = tracked.fetched();
        drop(tracked);
        drop(guard);
        let answered = match filled {
            Ok(()) => Answered::Filled,
            Err(e) => Answered::Failed(e),
        };
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
    /// no folder above it pins it and stays pinned (`NotAllowed`). `Files1`
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
    /// path is in the folder, and one of ours. `Files1` asks every account
    /// first, as for [`check_unpinnable`](Self::check_unpinnable).
    pub async fn check_pinnable(&self, paths: &[PathBuf]) -> Result<(), SyncError> {
        let reg = self.require_registration()?;
        self.pin_targets(&reg.root, paths).await.map(drop)
    }

    /// What `FreeUp` checks before it frees anything, on its own:
    /// [`check_unpinnable`](Self::check_unpinnable)'s rules, and a folder
    /// with interception has its helper (`NoHelper`).
    pub async fn check_free_up(&self, paths: &[PathBuf]) -> Result<(), SyncError> {
        if self.require_registration()?.intercepted {
            self.require_link()?;
        }
        self.check_unpinnable(paths).await
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

    /// `RecentActivity(limit)`: the newest `limit` events, newest first.
    pub async fn recent_activity(&self, limit: u32) -> Result<Vec<activity::Event>, SyncError> {
        let report = self.report.clone();
        tokio::task::spawn_blocking(move || report.activity.recent(limit as usize))
            .await
            .map_err(|e| SyncError::Io(format!("the activity task failed: {e}")))?
            .map_err(|e| SyncError::Io(e.to_string()))
    }

    /// `Conflicts()`: (time, original, rescued), newest first; one whose
    /// rescued file is gone is dropped on the way.
    pub async fn conflicts(&self) -> Result<Vec<crate::tree::ConflictRow>, SyncError> {
        let report = self.report.clone();
        tokio::task::spawn_blocking(move || report.activity.conflicts())
            .await
            .map_err(|e| SyncError::Io(format!("the conflicts task failed: {e}")))?
            .map_err(|e| SyncError::Io(e.to_string()))
    }

    /// `DismissConflict(rescued_path)`: the conflict comes off the list, and
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

    /// `LastChecked`, `LocalBytes`, `ConflictCount`.
    pub fn status(&self) -> (i64, u64, u32) {
        let s = self.state.get();
        (s.last_checked, s.local_bytes, s.conflict_count)
    }

    /// `Transfers`: every download under way, as (path, bytes done, total).
    pub fn transfers(&self) -> Vec<(String, u64, u64)> {
        self.report.transfers.list().into_iter().map(|t| (t.path, t.done, t.total)).collect()
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
    async fn fetch(&self, item_id: &str, from: u64) -> Result<Fetched, SourceError> {
        let source = self.source.lock().unwrap().clone();
        match source {
            Some(source) => source.fetch(item_id, from).await,
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
        match self.fill_now(path).await {
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
mod tests {
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use async_trait::async_trait;
    use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
    use nix::sys::socket::{
        accept, bind, listen as sock_listen, socket, AddressFamily, Backlog, SockFlag, SockType,
        UnixAddr,
    };
    use tokio::sync::mpsc;

    use super::source::{ContentSource, Fetched, LocalDir, SourceError};
    use super::*;

    /// Where a service persists its folder: the one account of the
    /// `config.toml` at `file` (added when there is none), in a store opened
    /// from the file — as each start opens it. A file that cannot be read
    /// makes a store that refuses every write, as the daemon's does.
    pub(super) fn persist(file: &Path) -> Persist {
        assert_eq!(file.file_name().and_then(|n| n.to_str()), Some("config.toml"));
        let paths = crate::config::Paths::in_dir(file.parent().unwrap());
        // `open` awaits nothing but the wallet check, which here is ready.
        let opening = std::pin::pin!(ConfigStore::open(&paths, async { false }));
        let std::task::Poll::Ready(store) =
            opening.poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        else {
            unreachable!("ConfigStore::open waited")
        };
        let account = match store.snapshot().accounts.first() {
            Some(account) => account.id.clone(),
            None => store.add_account("Personal").map(|a| a.id).unwrap_or_else(|_| "0123456789ab".into()),
        };
        Persist { store: Arc::new(store), account }
    }

    /// What `config.toml` records of the account's folder, in the words of
    /// version 1's file that these tests were first written in. No folder
    /// reads as version 1's defaults.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct Config {
        pub sync_root: String,
        pub sync_root_id: String,
        pub sync_root_intercepted: bool,
        pub sync_root_source: String,
        pub sync_root_baloo_excluded: bool,
        pub sync_root_upgrade_when_helper: Option<bool>,
    }

    impl Config {
        pub(super) fn load(file: &Path) -> Result<Self, String> {
            let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
            let config: crate::config::Config = toml::from_str(&text).map_err(|e| e.to_string())?;
            Ok(match config.accounts.first().and_then(|a| a.root.clone()) {
                Some(root) => Config {
                    sync_root: root.path.display().to_string(),
                    sync_root_id: root.id,
                    sync_root_intercepted: root.intercepted,
                    sync_root_source: root.source,
                    sync_root_baloo_excluded: root.baloo_excluded,
                    sync_root_upgrade_when_helper: root.upgrade_when_helper,
                },
                None => Config {
                    sync_root: String::new(),
                    sync_root_id: String::new(),
                    sync_root_intercepted: true,
                    sync_root_source: "local".into(),
                    sync_root_baloo_excluded: false,
                    sync_root_upgrade_when_helper: None,
                },
            })
        }
    }

    /// Writes a `config.toml` whose one account's folder is `root`, as a
    /// daemon that knew less wrote it.
    fn write_config(file: &Path, root: &str) {
        let text = format!("config_version = 2\n\n[[accounts]]\nid = \"0123456789ab\"\nlabel = \"Personal\"\n\n[accounts.root]\n{root}");
        std::fs::write(file, text).unwrap();
    }

    /// A stand-in helper: accepts one connection, greets, acknowledges the
    /// handshake `Hello`, then acknowledges everything and reports every
    /// `HydrateDone` it sees. The listener is bound on the caller's thread,
    /// before this returns, so `connect` cannot race `bind`.
    ///
    /// It reports through a `tokio` channel rather than a `std` one because
    /// the tests below wait for it *inside* the runtime: a blocking
    /// `recv_timeout` on a current-thread runtime would park the one thread
    /// that has to run `serve_hydrations`.
    fn fake_helper(path: std::path::PathBuf) -> mpsc::UnboundedReceiver<(u64, i32)> {
        let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
            .unwrap();
        let addr = UnixAddr::new(&path).unwrap();
        bind(fd.as_raw_fd(), &addr).unwrap();
        sock_listen(&fd, Backlog::new(16).unwrap()).unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let listener: OwnedFd = fd;
            let accepted = accept(listener.as_raw_fd()).unwrap();
            // SAFETY: `accept` just returned a freshly opened descriptor that
            // this process now solely owns.
            let stream = unsafe { UnixStream::from_raw_fd(accepted) };
            let mut channel = Channel::new(stream).unwrap();
            channel
                .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
                .unwrap();
            let (hello, _) = channel.recv::<ToHelper>().unwrap();
            assert!(
                matches!(hello, ToHelper::Hello { version } if version == PROTOCOL_VERSION),
                "{hello:?}"
            );
            channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
            while let Ok((message, _fd)) = channel.recv::<ToHelper>() {
                if let ToHelper::HydrateDone { req_id, errno } = message {
                    let _ = tx.send((req_id, errno));
                }
                if channel.send(&ToDaemon::Ack { errno: 0 }, None).is_err() {
                    break;
                }
            }
        });
        rx
    }

    fn placeholder(dir: &std::path::Path, name: &str, item_id: &str, size: u64) -> OwnedFd {
        let handle = std::fs::File::open(dir).unwrap();
        konedrive_fs::placeholder::create_placeholder(
            &handle,
            name,
            item_id,
            size,
            std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
        )
        .unwrap();
        std::fs::File::options()
            .read(true)
            .write(true)
            .open(dir.join(name))
            .unwrap()
            .as_fd()
            .try_clone_to_owned()
            .unwrap()
    }

    struct Panics;

    #[async_trait]
    impl ContentSource for Panics {
        async fn fetch(&self, _item_id: &str, _from: u64) -> Result<Fetched, SourceError> {
            panic!("this content source explodes on contact");
        }
    }

    /// A panicking fill answers no one on its own: it closes the
    /// event fd by unwinding and produces no errno, so `hydrate_done` is
    /// never called and the `open()` the kernel suspended is never responded
    /// to at all — it hangs for the life of the helper. A panic in our code
    /// must degrade to a denial, never to silence.
    #[tokio::test]
    async fn a_panicking_fill_still_answers_the_suspended_open() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("helper.sock");
        let mut seen = fake_helper(socket_path.clone());
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

        let local = tempfile::tempdir().unwrap();
        let fd = placeholder(local.path(), "file.bin", "ITEM", 4096);

        let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
        tokio::spawn(serve_hydrations(link, rx, Arc::new(Panics), InodeLocks::new()));
        tx.send(HydrateRequest { req_id: 77, fd }).await.unwrap();

        let answer = tokio::time::timeout(Duration::from_secs(5), seen.recv())
            .await
            .expect("a panicking hydration must still answer the suspended open")
            .expect("the helper connection must stay up");
        assert_eq!(answer, (77, libc::EIO));
    }

    /// Pinned the only way it can be: the discriminator is
    /// not how many fills run at once — that is 4 either way — but whether
    /// the request loop stops *taking* work while they run. Acquiring the
    /// permit inside the spawned task instead drains the bounded channel as
    /// fast as the helper can fill it, into a pile of tasks each holding a
    /// suspended open's event descriptor, and the channel never refuses
    /// anything. Here it must refuse.
    #[tokio::test]
    async fn the_request_loop_stops_taking_work_while_four_fills_are_running() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("helper.sock");
        let _seen = fake_helper(socket_path.clone());
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM"), vec![1u8; 4096]).unwrap();
        let local = tempfile::tempdir().unwrap();
        let fds: Vec<OwnedFd> = (0..40)
            .map(|i| placeholder(local.path(), &format!("f{i}.bin"), "ITEM", 4096))
            .collect();

        // Every fill parks in `fetch` and holds its permit there.
        let source = Arc::new(LocalDir::new(remote.path()).delay(Duration::from_secs(3600)));
        let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
        tokio::spawn(serve_hydrations(link, rx, source, InodeLocks::new()));

        let mut accepted = 0;
        let mut refused = false;
        for (i, fd) in fds.into_iter().enumerate() {
            match tx.try_send(HydrateRequest { req_id: i as u64, fd }) {
                Ok(()) => accepted += 1,
                Err(_) => {
                    refused = true;
                    break;
                }
            }
            // Let the request loop take everything it is willing to take
            // before offering it the next one.
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
        }

        assert!(
            refused,
            "the channel accepted all {accepted} requests: the request loop is draining it \
             into unbounded in-flight work instead of stopping at four"
        );
        assert!(
            accepted <= 4 + 4 + 1,
            "at most four in flight, four buffered and one blocked on the permit, but \
             {accepted} were accepted"
        );
    }

    /// The daemon's half of `MAX_OUTSTANDING_HYDRATIONS`, in its easier form:
    /// every fill slot taken by a fill that will not finish during the test,
    /// the rest of the helper's credit queued — the reader thread must still
    /// get as far as the `Ack` the helper queued behind them. (Its fills
    /// still hold credit, so this is not the worst case; the test below is.) If it stops short (a request queue shallower than
    /// the contract), that `Ack` is never read: here a call hangs, and in
    /// the real burst every fill waiting for its own `HydrateDone`'s `Ack`
    /// hangs with it until the daemon's call timeout ends the connection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_call_is_answered_while_every_request_the_helper_may_send_is_in_flight() {
        const MAX: usize = konedrive_proto::MAX_OUTSTANDING_HYDRATIONS;
        let files = tempfile::tempdir().unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        let requests: Vec<OwnedFd> = (0..MAX)
            .map(|i| {
                std::fs::write(source_dir.path().join(format!("f{i}")), [7u8; 16]).unwrap();
                placeholder(files.path(), &format!("f{i}"), &format!("f{i}"), 16)
            })
            .collect();

        let sockets = tempfile::tempdir().unwrap();
        let path = sockets.path().join("helper.sock");
        let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
            .unwrap();
        bind(fd.as_raw_fd(), &UnixAddr::new(&path).unwrap()).unwrap();
        sock_listen(&fd, Backlog::new(4).unwrap()).unwrap();
        std::thread::spawn(move || {
            let listener: OwnedFd = fd;
            let accepted = accept(listener.as_raw_fd()).unwrap();
            // SAFETY: `accept` just returned a freshly opened descriptor that
            // this thread now solely owns.
            let mut channel = Channel::new(unsafe { UnixStream::from_raw_fd(accepted) }).unwrap();
            channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
            let _hello = channel.recv::<ToHelper>().unwrap();
            channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
            // Everything the helper may have outstanding, all at once, ahead
            // of whatever the daemon asks next — the order a burst produces.
            for (req_id, request) in requests.iter().enumerate() {
                channel
                    .send(&ToDaemon::HydrateRequest { req_id: req_id as u64 }, Some(request.as_fd()))
                    .unwrap();
            }
            while channel.recv::<ToHelper>().is_ok() {
                if channel.send(&ToDaemon::Ack { errno: 0 }, None).is_err() {
                    break;
                }
            }
        });

        let (link, incoming) = HelperLink::connect(&path).await.unwrap();
        // A minute per fetch: every fill slot stays taken for the whole test.
        let (source, _peak) = CountingSource::new(source_dir.path(), Duration::from_secs(60));
        tokio::spawn(serve_hydrations(link.clone(), incoming, source, InodeLocks::new()));

        let dir = std::fs::File::open(files.path()).unwrap();
        let answered = tokio::time::timeout(Duration::from_secs(5), link.mark_dir(&dir)).await;
        assert!(
            matches!(answered, Ok(Ok(()))),
            "with {MAX} hydrations in flight the daemon stopped reading before the Ack queued \
             behind them: {answered:?}"
        );
    }

    // --- C1: a request looks again -----

    /// What `serve_hydrations` answered for one request, through the plain
    /// fake helper, with a source that can be watched.
    async fn serve_one(fd: OwnedFd, source: Arc<dyn ContentSource>) -> (u64, i32) {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("helper.sock");
        let mut seen = fake_helper(socket_path.clone());
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
        tokio::spawn(serve_hydrations(link, rx, source, InodeLocks::new()));
        tx.send(HydrateRequest { req_id: 5, fd }).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), seen.recv())
            .await
            .expect("the request must be answered")
            .expect("the helper connection must stay up")
    }

    /// C1, on the host. A request the helper
    /// sent while the file was `online-only` waits — for a fill slot, or for
    /// credit — and the file is filled directly meanwhile. The request must
    /// find it filled and answer at once, untouched. Before the fix it filled
    /// the file again without looking; with a source that can no longer serve
    /// it, the failed re-fetch rolled back — demoting a hydrated file and
    /// punching it — which in the VM, where an opener had meanwhile had the
    /// file ignore-marked, gave the next reader 65 536 zero bytes.
    #[tokio::test]
    async fn a_request_for_a_file_filled_meanwhile_is_answered_without_touching_it() {
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM"), vec![4u8; 4096]).unwrap();
        let local = tempfile::tempdir().unwrap();
        let fd = placeholder(local.path(), "file.bin", "ITEM", 4096);
        let waiting = fd.try_clone().unwrap();
        let source = Arc::new(LocalDir::new(remote.path()));
        assert_eq!(source::hydrate(fd, source.as_ref()).await, 0, "filled directly");
        std::fs::remove_file(remote.path().join("ITEM")).unwrap();
        let fetched = source.fetches();

        let answer = serve_one(waiting, Arc::clone(&source) as Arc<dyn ContentSource>).await;

        let path = local.path().join("file.bin");
        assert_eq!(answer, (5, 0), "a file that is already there is answered success");
        assert_eq!(source.fetches(), fetched, "and it is not fetched again");
        assert_eq!(std::fs::read(&path).unwrap(), vec![4u8; 4096], "nor emptied");
        assert_eq!(
            read_state(&std::fs::File::open(&path).unwrap()).unwrap(),
            Some(State::Hydrated),
            "nor demoted"
        );
    }

    /// The same stale request when the re-fetch would *succeed*: the file was
    /// edited in place after it was filled, and there is no upload in this
    /// sub-project, so that edit is the only copy (§8). Before the fix the
    /// request overwrote it with the remote content and reported success.
    #[tokio::test]
    async fn a_request_never_overwrites_a_hydrated_file_edited_in_place() {
        use std::os::unix::fs::FileExt;
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM"), vec![4u8; 4096]).unwrap();
        let local = tempfile::tempdir().unwrap();
        let fd = placeholder(local.path(), "file.bin", "ITEM", 4096);
        let waiting = fd.try_clone().unwrap();
        let source = Arc::new(LocalDir::new(remote.path()));
        assert_eq!(source::hydrate(fd, source.as_ref()).await, 0);
        let path = local.path().join("file.bin");
        std::fs::File::options().write(true).open(&path).unwrap().write_all_at(b"EDITED", 0).unwrap();
        let fetched = source.fetches();

        let answer = serve_one(waiting, Arc::clone(&source) as Arc<dyn ContentSource>).await;

        assert_eq!(answer, (5, 0), "the file is there, so the opener is let through to it");
        assert_eq!(source.fetches(), fetched, "without fetching anything");
        assert_eq!(&std::fs::read(&path).unwrap()[..6], b"EDITED", "and the edit is kept");
    }

    /// A content source that records, on its first fetch, whether the fake
    /// helper had already been asked to `ClearIgnore`.
    struct ClearedFirst {
        dir: std::path::PathBuf,
        seen: Arc<std::sync::Mutex<Vec<Seen>>>,
        fetches: Arc<std::sync::Mutex<Vec<bool>>>,
    }

    #[async_trait]
    impl ContentSource for ClearedFirst {
        async fn fetch(&self, item_id: &str, from: u64) -> Result<Fetched, SourceError> {
            let cleared = self.seen.lock().unwrap().contains(&Seen::ClearIgnore);
            self.fetches.lock().unwrap().push(cleared);
            LocalDir::new(self.dir.clone()).fetch(item_id, from).await
        }
    }

    /// A file a cancelled `Dehydrate` left `dehydrating` may still carry its
    /// ignore mark (the call stopped between the state write and its
    /// `ClearIgnore`), and a fill that fails punches the file. So a fill of a
    /// file in any state but `online-only` makes `hydrating` durable and then
    /// has the mark cleared, **before the first byte is fetched** — the
    /// question small round 3's punch enumeration should have asked: can this
    /// file carry a mark placed after the last clear?
    #[tokio::test]
    async fn a_file_left_dehydrating_is_refilled_only_after_its_ignore_mark_is_cleared() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM"), vec![8u8; 4096]).unwrap();
        let local = tempfile::tempdir().unwrap();
        let fd = placeholder(local.path(), "file.bin", "ITEM", 4096);
        let path = local.path().join("file.bin");
        konedrive_fs::placeholder::write_state(&std::fs::File::open(&path).unwrap(), State::Dehydrating)
            .unwrap();
        let fetches = Arc::new(std::sync::Mutex::new(Vec::new()));
        let source = Arc::new(ClearedFirst {
            dir: remote.path().to_path_buf(),
            seen: Arc::clone(&helper.seen),
            fetches: Arc::clone(&fetches),
        });

        let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
        tokio::spawn(serve_hydrations(link, rx, source, InodeLocks::new()));
        tx.send(HydrateRequest { req_id: 9, fd }).await.unwrap();
        wait_until("the request is answered", || helper.seen().contains(&Seen::HydrateDone)).await;

        assert_eq!(
            *fetches.lock().unwrap(),
            vec![true],
            "the one fetch must come after the ignore mark was cleared"
        );
        assert_eq!(std::fs::read(&path).unwrap(), vec![8u8; 4096]);
    }

    /// And when the mark cannot be cleared, nothing is fetched and nothing is
    /// touched: the file keeps its content and the state it was found in.
    #[tokio::test]
    async fn a_refill_whose_ignore_mark_cannot_be_cleared_touches_nothing() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        helper.refuse(Seen::ClearIgnore, libc::EIO);
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM"), vec![8u8; 4096]).unwrap();
        let local = tempfile::tempdir().unwrap();
        let fd = placeholder(local.path(), "file.bin", "ITEM", 4096);
        let path = local.path().join("file.bin");
        std::fs::write(&path, vec![3u8; 4096]).unwrap();
        konedrive_fs::placeholder::write_state(&std::fs::File::open(&path).unwrap(), State::Dehydrating)
            .unwrap();
        let source = Arc::new(LocalDir::new(remote.path()));

        let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
        tokio::spawn(serve_hydrations(link, rx, Arc::clone(&source) as Arc<dyn ContentSource>, InodeLocks::new()));
        tx.send(HydrateRequest { req_id: 9, fd }).await.unwrap();
        wait_until("the request is answered", || helper.seen().contains(&Seen::HydrateDone)).await;

        assert_eq!(source.fetches(), 0, "nothing may be fetched into a file that may be ignored");
        assert_eq!(std::fs::read(&path).unwrap(), vec![3u8; 4096], "nor may it be emptied");
        assert_eq!(
            read_state(&std::fs::File::open(&path).unwrap()).unwrap(),
            Some(State::Dehydrating),
            "and it keeps the state it was found in"
        );
    }

    /// `Hydrate()` asks the same question: in an intercepted root it clears the
    /// mark before refilling a file that may carry one, and with the helper
    /// gone it refuses rather than fill a file it could then have to punch.
    #[tokio::test]
    async fn hydrate_now_clears_the_ignore_mark_before_refilling_a_file_that_may_carry_one() {
        let (service, root_dir, _source_dir, _sockets, helper) =
            populated_service(&vec![5u8; 4096]).await;
        let file = root_dir.path().join("f.bin");
        let dehydrating = || {
            let handle = std::fs::File::options().read(true).write(true).open(&file).unwrap();
            konedrive_fs::placeholder::write_state(&handle, State::Dehydrating).unwrap();
        };

        dehydrating();
        let link = service.link();
        service.set_link(None);
        let refused = service.hydrate_now(&file).await;
        assert!(matches!(refused, Err(SyncError::NoHelper)), "{refused:?}");
        assert_eq!(service.item_state(&file).await, "dehydrating", "and nothing changed");

        service.set_link(link);
        helper.forget();
        service.hydrate_now(&file).await.unwrap();
        assert_eq!(helper.seen(), vec![Seen::ClearIgnore], "the mark is cleared first");
        assert_eq!(std::fs::read(&file).unwrap(), vec![5u8; 4096]);
        assert_eq!(service.item_state(&file).await, "hydrated");
    }

    /// The credit contract's true worst case. Four fills have finished and sent `HydrateDone`, and
    /// hold their fill slots until the `Ack`s come back; the helper has
    /// counted those four requests answered and sent its whole credit of new
    /// requests — and the four `Ack`s are queued on the socket *behind* them.
    /// The reader thread must take every one of those requests to reach the
    /// `Ack`s: the request loop holds one while it waits for a slot and the
    /// request queue the rest, so the queue must be at least
    /// `MAX_OUTSTANDING_HYDRATIONS - 1` deep. One shallower and nothing moves
    /// again: the fills wait for `Ack`s the reader never reaches, and the
    /// reader waits for a queue the fills never drain.
    ///
    /// The test above keeps its fills running, so their requests still hold
    /// credit and fewer new ones can be in flight; it passed with the queue
    /// cut to 59. This one fails at 62 and passes at 63 (measured by
    /// mutating the queue depth in `helper.rs`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_reader_reaches_acks_queued_behind_every_request_the_helper_may_send() {
        const MAX: usize = konedrive_proto::MAX_OUTSTANDING_HYDRATIONS;
        const SLOTS: usize = 4;
        let files = tempfile::tempdir().unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        let requests: Vec<OwnedFd> = (0..SLOTS + MAX)
            .map(|i| {
                std::fs::write(source_dir.path().join(format!("f{i}")), [7u8; 16]).unwrap();
                placeholder(files.path(), &format!("f{i}"), &format!("f{i}"), 16)
            })
            .collect();

        let sockets = tempfile::tempdir().unwrap();
        let path = sockets.path().join("helper.sock");
        let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
            .unwrap();
        bind(fd.as_raw_fd(), &UnixAddr::new(&path).unwrap()).unwrap();
        sock_listen(&fd, Backlog::new(4).unwrap()).unwrap();
        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<u64>();
        std::thread::spawn(move || {
            let listener: OwnedFd = fd;
            let accepted = accept(listener.as_raw_fd()).unwrap();
            // SAFETY: `accept` just returned a freshly opened descriptor that
            // this thread now solely owns.
            let mut channel = Channel::new(unsafe { UnixStream::from_raw_fd(accepted) }).unwrap();
            channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
            let _hello = channel.recv::<ToHelper>().unwrap();
            channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
            let send = |channel: &mut Channel, req_id: usize| {
                channel
                    .send(&ToDaemon::HydrateRequest { req_id: req_id as u64 }, Some(requests[req_id].as_fd()))
                    .unwrap();
            };
            // One request per fill slot, and their `HydrateDone`s read but
            // not yet acknowledged: four fills now hold their slots.
            for req_id in 0..SLOTS {
                send(&mut channel, req_id);
            }
            for _ in 0..SLOTS {
                let (done, _) = channel.recv::<ToHelper>().unwrap();
                let ToHelper::HydrateDone { req_id, .. } = done else { panic!("{done:?}") };
                let _ = done_tx.send(req_id);
            }
            // Those four answered, the whole credit is free again: a
            // helper sends that many more before the four `Ack`s.
            for req_id in SLOTS..SLOTS + MAX {
                send(&mut channel, req_id);
            }
            for _ in 0..SLOTS {
                channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
            }
            while let Ok((message, _)) = channel.recv::<ToHelper>() {
                if let ToHelper::HydrateDone { req_id, .. } = message {
                    let _ = done_tx.send(req_id);
                }
                if channel.send(&ToDaemon::Ack { errno: 0 }, None).is_err() {
                    break;
                }
            }
        });

        let (link, incoming) = HelperLink::connect(&path).await.unwrap();
        let source = Arc::new(LocalDir::new(source_dir.path()));
        tokio::spawn(serve_hydrations(link, incoming, source, InodeLocks::new()));

        let mut answered = 0;
        while answered < SLOTS + MAX {
            let next = tokio::time::timeout(Duration::from_secs(5), done_rx.recv()).await;
            assert!(
                matches!(next, Ok(Some(_))),
                "after {answered} HydrateDone(s) nothing more came: the daemon stopped reading \
                 before the Acks queued behind {MAX} requests, and its fills wait for them"
            );
            answered += 1;
        }
    }

    // --- InodeLocks (Ruling: serialization) -----------

    /// A file's `(dev, ino)`, the way every caller of `InodeLocks` gets one.
    fn key_of(path: &std::path::Path) -> InodeKey {
        InodeKey::of(&std::fs::File::open(path).unwrap()).unwrap()
    }

    /// The property a path key cannot have: two names for one
    /// inode are one key, and two different files are two keys.
    #[test]
    fn a_key_names_the_inode_and_not_the_name_it_was_reached_by() {
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one.bin");
        let another = dir.path().join("another.bin");
        std::fs::write(&one, b"x").unwrap();
        std::fs::write(&another, b"x").unwrap();
        let link = dir.path().join("link.bin");
        std::fs::hard_link(&one, &link).unwrap();
        let renamed = dir.path().join("renamed.bin");

        assert_eq!(key_of(&one), key_of(&link), "a hard link is the same inode");
        assert_ne!(key_of(&one), key_of(&another), "two files are two inodes");
        let before = key_of(&one);
        std::fs::rename(&one, &renamed).unwrap();
        assert_eq!(before, key_of(&renamed), "a rename changes no inode");
    }

    /// The lock has two constructors for one key — `of` on the
    /// `SyncService` side (`hydrate_now`, `dehydrate`) and `of_fd` on the
    /// interception side (`serve_hydrations`, which must not consume the
    /// event fd) — and serialization across the two sides holds only while
    /// they compute the same key. Drop `st_dev` from one, or offset the inode
    /// in the other, and an intercepted fill races a `Dehydrate` of the same
    /// file (C1's cross-side form) with every other test green.
    #[test]
    fn both_constructors_give_one_file_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        let other = dir.path().join("g.bin");
        std::fs::write(&path, b"x").unwrap();
        std::fs::write(&other, b"y").unwrap();

        // What `serve_hydrations` is handed: a bare descriptor.
        let event_fd: OwnedFd = std::fs::File::open(&path).unwrap().into();
        let by_fd = InodeKey::of_fd(&event_fd).unwrap();
        // What `hydrate_now` and `dehydrate` open for themselves.
        let by_file = InodeKey::of(&std::fs::File::open(&path).unwrap()).unwrap();

        assert_eq!(
            by_fd, by_file,
            "the interception side and the service side must lock the same key for one file"
        );
        assert_ne!(
            InodeKey::of_fd(std::fs::File::open(&other).unwrap()).unwrap(),
            by_file,
            "and a different file must still be a different key"
        );
    }

    /// The core property: a second waiter on the *same* key does not run
    /// until the first holder's guard drops. Measured by ordering, not by
    /// timing alone — `order` only ever gets `"b"` pushed onto it after
    /// `"a-still-holding"`, which can only happen if `locks.lock` really
    /// blocked task B for the whole time A held its guard.
    #[tokio::test]
    async fn the_second_waiter_on_the_same_key_does_not_run_until_the_first_releases() {
        let locks = InodeLocks::new();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"x").unwrap();
        let key = key_of(&dir.path().join("f"));
        let order: Arc<std::sync::Mutex<Vec<&'static str>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

        let guard_a = locks.lock(key).await;

        let order_b = Arc::clone(&order);
        let locks_b = locks.clone();
        let key_b = key;
        let waiter = tokio::spawn(async move {
            let _guard_b = locks_b.lock(key_b).await;
            order_b.lock().unwrap().push("b");
        });

        // Give the waiter every chance to (wrongly) run before A releases.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        order.lock().unwrap().push("a-still-holding");

        drop(guard_a);
        waiter.await.unwrap();

        assert_eq!(
            *order.lock().unwrap(),
            vec!["a-still-holding", "b"],
            "the second waiter must not enter until the first guard is dropped"
        );
    }

    /// The other half: this is a per-key lock, not a single global one — an
    /// unrelated file must never wait on this one's holder.
    #[tokio::test]
    async fn a_different_key_is_not_blocked_by_an_unrelated_one() {
        let locks = InodeLocks::new();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"x").unwrap();
        std::fs::write(dir.path().join("b"), b"x").unwrap();
        let _held = locks.lock(key_of(&dir.path().join("a"))).await;

        let other = tokio::time::timeout(
            Duration::from_millis(200),
            locks.lock(key_of(&dir.path().join("b"))),
        )
        .await;
        assert!(other.is_ok(), "an unrelated key must not block on this one's holder");
    }

    /// The table must not grow without bound: once the only guard for a key
    /// is dropped, that key's row is gone, not merely unlocked.
    #[tokio::test]
    async fn a_releasing_key_is_removed_from_the_table() {
        let locks = InodeLocks::new();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x"), b"x").unwrap();
        let guard = locks.lock(key_of(&dir.path().join("x"))).await;
        assert_eq!(locks.tracked(), 1, "the key must be tracked while held");
        drop(guard);
        assert_eq!(
            locks.tracked(),
            0,
            "a key with no more holders or waiters must not stay in the table forever"
        );
    }

    /// `try_lock` is refused while the key is held, leaves no
    /// row behind when refused, and once granted excludes `lock` like any
    /// other holder.
    #[tokio::test]
    async fn try_lock_is_refused_while_held_and_leaves_nothing_behind() {
        let locks = InodeLocks::new();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x"), b"x").unwrap();
        let key = key_of(&dir.path().join("x"));

        let held = locks.lock(key).await;
        assert!(locks.try_lock(key).is_none(), "granted while another holder has it");
        assert_eq!(locks.users(key), 1, "a refused try_lock left itself in the count");
        drop(held);
        assert_eq!(locks.tracked(), 0);

        let taken = locks.try_lock(key).expect("free, so granted");
        let waiter = tokio::time::timeout(Duration::from_millis(100), locks.lock(key)).await;
        assert!(waiter.is_err(), "lock() got in while try_lock held the key");
        drop(taken);
        assert_eq!(locks.tracked(), 0, "rows left behind");
    }

    /// The other direction, and the one the previous bookkeeping got wrong:
    /// a row must **survive** while somebody else still needs it. Dropping
    /// it there would hand the next caller a brand-new mutex for an inode
    /// another task is already working on — two fills of one file, which is
    /// exactly what this table exists to prevent, arrived at through the
    /// cleanup rather than through the lock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_row_stays_while_another_caller_is_still_using_it() {
        let locks = InodeLocks::new();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x"), b"x").unwrap();
        let key = key_of(&dir.path().join("x"));

        let held_by_b = Arc::new(tokio::sync::Notify::new());
        let guard_a = locks.lock(key).await;
        let locks_b = locks.clone();
        let told = Arc::clone(&held_by_b);
        let b = tokio::spawn(async move {
            let _guard = locks_b.lock(key).await;
            told.notify_one();
            tokio::time::sleep(Duration::from_secs(3)).await;
        });
        // Wait until B is genuinely counted as waiting. Spinning on
        // `yield_now` and hoping does not do it: on a multi-threaded runtime
        // B may not have reached `lock` at all when A releases, and then the
        // cleanup this test is about never has two callers to choose
        // between — the over-eager version passes exactly as the correct one
        // does. (Measured: with the yield-only version, the "drop the row
        // while another caller holds it" mutant survived.)
        wait_until("the second caller is waiting", || locks.users(key) == 2).await;
        drop(guard_a);
        held_by_b.notified().await;

        // B holds it now. A third caller must wait for B — which it can only
        // do if A's release left B's row in the table.
        let third = tokio::time::timeout(Duration::from_millis(300), locks.lock(key)).await;
        assert!(
            third.is_err(),
            "a third caller entered while another still held the same inode: the row was \
             dropped from the table while it was in use"
        );
        b.abort();
    }

    /// A waiter whose future is dropped — a D-Bus method whose caller went
    /// away, a `select!` that lost — must not leave its row behind. Measured
    /// before the fix: holder releases, parked waiter is cancelled, one row
    /// stays in the table forever.
    #[tokio::test]
    async fn a_cancelled_waiter_leaves_no_row_behind() {
        let locks = InodeLocks::new();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x"), b"x").unwrap();
        let key = key_of(&dir.path().join("x"));

        let guard = locks.lock(key).await;
        let locks_b = locks.clone();
        let waiter = tokio::spawn(async move {
            let _guard = locks_b.lock(key).await;
        });
        wait_until("the waiter is parked", || locks.users(key) == 2).await;
        waiter.abort();
        // Awaiting the handle after `abort` is what guarantees the task's
        // future has actually been dropped, not merely told to stop.
        assert!(waiter.await.unwrap_err().is_cancelled());
        drop(guard);

        assert_eq!(
            locks.tracked(),
            0,
            "a cancelled waiter left its row in the table: the table grows by one row per \
             cancelled call, forever"
        );
    }

    // --- SyncService -------------------------------------------------------

    /// What a fake helper was asked to do, in the order it was asked.
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    enum Seen {
        RegisterRoot,
        UnregisterRoot,
        /// A `MarkDir`, and how many entries the directory held **at the
        /// moment the mark arrived** — read through the very descriptor the
        /// daemon attached. Invariant M1 says a new directory is marked
        /// before anything is created inside it, and this is the only way to
        /// measure that from outside: a mark that arrives after the
        /// directory has been filled reports a non-zero count.
        MarkDir { entries: usize },
        MarkFile,
        ClearIgnore,
        HydrateDone,
    }

    impl Seen {
        /// The request's kind alone: `MarkDir` with its count left out.
        fn kind(&self) -> Seen {
            match self {
                Seen::MarkDir { .. } => Seen::MarkDir { entries: 0 },
                other => other.clone(),
            }
        }
    }

    /// A fake helper that records what it was asked to do and can be cut off
    /// on demand: greets, acknowledges `Hello`, and acknowledges everything
    /// after that. It accepts connection after connection, so a daemon that
    /// reconnects finds it still there.
    struct FakeHelper {
        seen: Arc<std::sync::Mutex<Vec<Seen>>>,
        /// Requests answered with an errno instead of 0, by kind — the
        /// `Seen` a request is recorded as. Anything absent is acknowledged.
        refusals: Arc<std::sync::Mutex<HashMap<Seen, i32>>>,
        /// A duplicate of the live connection's socket, so a test can cut it
        /// the way a helper that died would.
        live: Arc<std::sync::Mutex<Option<UnixStream>>>,
    }

    impl FakeHelper {
        /// Starts one on `path`. `register_root_delay` holds the ack for
        /// `RegisterRoot` open, which is the only window a test has to
        /// interfere between a registration and the recovery that follows.
        fn start(path: std::path::PathBuf, register_root_delay: Duration) -> Self {
            let fd = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
                .unwrap();
            let addr = UnixAddr::new(&path).unwrap();
            bind(fd.as_raw_fd(), &addr).unwrap();
            sock_listen(&fd, Backlog::new(16).unwrap()).unwrap();
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let refusals: Arc<std::sync::Mutex<HashMap<Seen, i32>>> =
                Arc::new(std::sync::Mutex::new(HashMap::new()));
            let live: Arc<std::sync::Mutex<Option<UnixStream>>> =
                Arc::new(std::sync::Mutex::new(None));
            let recorded = Arc::clone(&seen);
            let refusing = Arc::clone(&refusals);
            let current = Arc::clone(&live);
            std::thread::spawn(move || {
                let listener: OwnedFd = fd;
                while let Ok(accepted) = accept(listener.as_raw_fd()) {
                    // SAFETY: `accept` just returned a freshly opened
                    // descriptor that this process now solely owns.
                    let stream = unsafe { UnixStream::from_raw_fd(accepted) };
                    *current.lock().unwrap() = stream.try_clone().ok();
                    let Ok(mut channel) = Channel::new(stream) else { continue };
                    if channel
                        .send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None)
                        .is_err()
                    {
                        continue;
                    }
                    while let Ok((message, fd)) = channel.recv::<ToHelper>() {
                        let note = match &message {
                            ToHelper::Hello { .. } | ToHelper::UnmarkDir => None,
                            ToHelper::RegisterRoot { .. } => Some(Seen::RegisterRoot),
                            ToHelper::UnregisterRoot { .. } => Some(Seen::UnregisterRoot),
                            ToHelper::MarkDir => {
                                Some(Seen::MarkDir { entries: entries_of(fd.as_ref()) })
                            }
                            ToHelper::MarkFile => Some(Seen::MarkFile),
                            ToHelper::ClearIgnore => Some(Seen::ClearIgnore),
                            ToHelper::HydrateDone { .. } => Some(Seen::HydrateDone),
                        };
                        // Kinds are compared without `MarkDir`'s entry count.
                        let errno = note
                            .as_ref()
                            .map(Seen::kind)
                            .and_then(|kind| refusing.lock().unwrap().get(&kind).copied())
                            .unwrap_or(0);
                        if let Some(note) = note {
                            recorded.lock().unwrap().push(note);
                        }
                        if matches!(message, ToHelper::RegisterRoot { .. }) {
                            std::thread::sleep(register_root_delay);
                        }
                        if channel.send(&ToDaemon::Ack { errno }, None).is_err() {
                            break;
                        }
                    }
                }
            });
            Self { seen, refusals, live }
        }

        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }

        /// From now on, answers every request of `kind` with `errno`.
        fn refuse(&self, kind: Seen, errno: i32) {
            self.refusals.lock().unwrap().insert(kind.kind(), errno);
        }

        fn forget(&self) {
            self.seen.lock().unwrap().clear();
        }

        /// Sends the daemon a hydration request for `fd` on the live
        /// connection, as the helper does for an intercepted open. A second
        /// `Channel` on the same socket is safe: every send is one datagram.
        fn send_request(&self, req_id: u64, fd: &OwnedFd) {
            let live = self.live.lock().unwrap();
            let stream = live.as_ref().expect("a live connection").try_clone().unwrap();
            let mut channel = Channel::new(stream).unwrap();
            channel.send(&ToDaemon::HydrateRequest { req_id }, Some(fd.as_fd())).unwrap();
        }

        /// Cuts the live connection, the way a helper that crashed would.
        fn hang_up(&self) {
            if let Some(stream) = self.live.lock().unwrap().take() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    /// How many entries a directory holds, through a descriptor rather than
    /// a name.
    fn entries_of(fd: Option<&OwnedFd>) -> usize {
        let Some(fd) = fd else { return usize::MAX };
        std::fs::read_dir(format!("/proc/self/fd/{}", fd.as_raw_fd()))
            .map(|entries| entries.count())
            .unwrap_or(usize::MAX)
    }

    async fn service_with_helper() -> (Arc<SyncService>, tempfile::TempDir, FakeHelper) {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        (SyncService::new(Some(link), None, None), sockets, helper)
    }

    /// `hydrate_now` must never report success on a file it did not fill —
    /// the "never serve zeros" property, exercised directly rather than only
    /// through the round trip in `sync_dbus.rs`.
    #[tokio::test]
    async fn hydrate_now_actually_fills_the_placeholder_with_the_sources_bytes() {
        let (service, _sockets, _helper) = service_with_helper().await;
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("f.bin"), vec![9u8; 2048]).unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root(root_dir.path()).await.unwrap();
        service.populate_from_directory(source_dir.path()).await.unwrap();
        let target = root_dir.path().join("f.bin");

        service.hydrate_now(&target).await.unwrap();

        assert_eq!(service.item_state(&target).await, "hydrated");
        assert_eq!(std::fs::read(&target).unwrap(), vec![9u8; 2048]);
    }

    // --- What a download or a free-up reports -----------------

    /// The newest events first, as (kind, path, detail), oldest first.
    async fn activity_of(service: &SyncService) -> Vec<(String, String, String)> {
        let mut events = service.recent_activity(200).await.unwrap();
        events.reverse();
        events.into_iter().map(|e| (e.kind, e.path, e.detail)).collect()
    }

    /// `Hydrate` finishing is a `downloaded` event with the
    /// file's size; one that fails is a `failed` event saying why.
    #[tokio::test]
    async fn hydrate_is_recorded_as_downloaded_and_a_failed_one_as_failed() {
        let (service, root_dir, _source_dir, _sockets, _helper) = populated_service(&vec![9u8; 2048]).await;
        let target = root_dir.path().join("f.bin");
        service.hydrate_now(&target).await.unwrap();
        // A placeholder whose item the source does not have.
        drop(placeholder(root_dir.path(), "gone.bin", "gone.bin", 100));
        let gone = root_dir.path().join("gone.bin");
        assert!(service.hydrate_now(&gone).await.is_err());

        let shown = |path: &Path| path.display().to_string();
        assert_eq!(
            activity_of(&service).await,
            vec![
                ("downloaded".to_owned(), shown(&target), "2.0 KiB".to_owned()),
                ("failed".to_owned(), shown(&gone), "it could not be downloaded".to_owned()),
            ]
        );
    }

    /// For a fill on open: the helper's request, filled through
    /// `serve_hydrations_reporting`, is a `downloaded` event under the name
    /// the file has — sent after the opener is answered.
    #[tokio::test]
    async fn a_fill_on_open_is_recorded_as_downloaded() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().canonicalize().unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("ITEM"), vec![3u8; 4096]).unwrap();
        let fd = placeholder(&folder, "opened.bin", "ITEM", 4096);
        // Events are kept only for the folder registered now.
        let report = Report::new(SyncStateHandle::new(SyncSnapshot {
            root_path: folder.display().to_string(),
            ..SyncSnapshot::default()
        }));
        let mut added = report.activity.subscribe();

        let socket_path = folder.join("helper.sock");
        let mut seen = fake_helper(socket_path.clone());
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
        let source: Arc<dyn ContentSource> = Arc::new(LocalDir::new(source_dir.path()));
        tokio::spawn(serve_hydrations_reporting(link, rx, source, InodeLocks::new(), report.clone()));
        tx.send(HydrateRequest { req_id: 9, fd }).await.unwrap();
        let answered = tokio::time::timeout(Duration::from_secs(10), seen.recv()).await.unwrap().unwrap();
        assert_eq!(answered, (9, 0));

        let event = tokio::time::timeout(Duration::from_secs(10), added.recv()).await.unwrap().unwrap();
        let opened = folder.join("opened.bin").display().to_string();
        assert_eq!((event.kind.as_str(), event.path.as_str(), event.detail.as_str()), ("downloaded", opened.as_str(), "4.0 KiB"));
    }

    /// A source whose one stream is the reading end of a pipe the test
    /// writes into: how a test holds a download half-way.
    struct Piped {
        reader: std::sync::Mutex<Option<tokio::io::DuplexStream>>,
        size: u64,
    }

    #[async_trait]
    impl ContentSource for Piped {
        async fn fetch(&self, _item_id: &str, from: u64) -> Result<Fetched, SourceError> {
            let stream = self.reader.lock().unwrap().take().ok_or_else(|| SourceError::NotFound("fetched twice".into()))?;
            Ok(Fetched {
                served_from: from,
                size: self.size,
                mtime: std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
                version: None,
                stream: Box::new(stream),
            })
        }
    }

    /// A source that answers "not found" once it is let go.
    struct Gated(std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>);

    #[async_trait]
    impl ContentSource for Gated {
        async fn fetch(&self, _item_id: &str, _from: u64) -> Result<Fetched, SourceError> {
            let gate = self.0.lock().unwrap().take();
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            Err(SourceError::NotFound("not there".into()))
        }
    }

    /// `Transfers`: a download is listed, with how far it has
    /// got, for as long as it runs — and not a moment after, whether it
    /// finished or failed.
    #[tokio::test]
    async fn a_download_shows_in_transfers_until_it_ends_however_it_ends() {
        use tokio::io::AsyncWriteExt;
        let (service, root_dir, _source_dir, _sockets, _helper) = populated_service(&vec![9u8; 64 * 1024]).await;
        let mut transfers = service.report().transfers.subscribe();
        let target = root_dir.path().join("f.bin");
        let shown = target.display().to_string();
        let (mut writer, reader) = tokio::io::duplex(128 * 1024);
        install_source(&service, Arc::new(Piped { reader: std::sync::Mutex::new(Some(reader)), size: 64 * 1024 }));
        let filling = {
            let (service, target) = (Arc::clone(&service), target.clone());
            tokio::spawn(async move { service.hydrate_now(&target).await })
        };
        writer.write_all(&[9u8; 16 * 1024]).await.unwrap();
        let halfway = |all: &std::collections::BTreeMap<u64, activity::Transfer>| {
            all.values().any(|t| t.path == shown && (t.done, t.total) == (16 * 1024, 64 * 1024))
        };
        tokio::time::timeout(Duration::from_secs(10), transfers.wait_for(halfway)).await.unwrap().unwrap();
        writer.write_all(&[9u8; 48 * 1024]).await.unwrap();
        drop(writer);
        filling.await.unwrap().unwrap();
        assert_eq!(service.transfers(), Vec::new(), "a finished download is not listed");

        drop(placeholder(root_dir.path(), "g.bin", "g.bin", 100));
        let failing_target = root_dir.path().join("g.bin");
        let failing_shown = failing_target.display().to_string();
        let (open, gate) = tokio::sync::oneshot::channel();
        install_source(&service, Arc::new(Gated(std::sync::Mutex::new(Some(gate)))));
        let failing = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.hydrate_now(&failing_target).await })
        };
        let listed = |all: &std::collections::BTreeMap<u64, activity::Transfer>| all.values().any(|t| t.path == failing_shown);
        tokio::time::timeout(Duration::from_secs(10), transfers.wait_for(listed)).await.unwrap().unwrap();
        open.send(()).unwrap();
        assert!(failing.await.unwrap().is_err());
        assert_eq!(service.transfers(), Vec::new(), "a failed download is not listed either");
    }

    /// A service with a folder registered without interception and no
    /// helper anywhere, filled from `files` (name, size), each downloaded
    /// when `hydrated` says so.
    async fn local_folder(files: &[(&str, usize, bool)]) -> (Arc<SyncService>, tempfile::TempDir, tempfile::TempDir) {
        let service = SyncService::new(None, None, None);
        let dir = tempfile::tempdir().unwrap();
        service.set_helper_socket(dir.path().join("no-helper.sock"));
        let source_dir = dir.path().join("source");
        std::fs::create_dir(&source_dir).unwrap();
        for (name, size, _) in files {
            std::fs::write(source_dir.join(name), vec![5u8; *size]).unwrap();
        }
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root_without_interception(root_dir.path()).await.unwrap();
        service.populate_from_directory(&source_dir).await.unwrap();
        for (name, _, hydrated) in files {
            if *hydrated {
                service.hydrate_now(&root_dir.path().join(name)).await.unwrap();
            }
        }
        (service, root_dir, dir)
    }

    /// `FreeUpSpace`: every downloaded file freed up through the
    /// per-file path, except one that is open — counted as busy, left as it
    /// is, and no error. The bytes are the blocks given back. And a Forget of
    /// the folder takes its activity with it.
    #[tokio::test]
    async fn free_up_space_frees_what_is_not_in_use_and_counts_what_is() {
        let (service, root_dir, _dir) = local_folder(&[("a.bin", 64 * 1024, true), ("b.bin", 64 * 1024, true)]).await;
        let (a, b) = (root_dir.path().join("a.bin"), root_dir.path().join("b.bin"));
        let before = data_blocks(&a);
        let _in_use = std::fs::File::open(&b).unwrap();

        let freed = service.free_up_space().await.unwrap();

        assert_eq!(freed, FreedUp { files: 1, bytes: (before - data_blocks(&a)) * 512, busy: 1, modified: 0, pinned: 0 });
        assert!(freed.bytes >= 64 * 1024, "{freed:?}");
        assert_eq!(service.item_state(&a).await, "online-only");
        assert_eq!(service.item_state(&b).await, "hydrated", "an open file is left as it is");
        let folder = root_dir.path().display().to_string();
        assert_eq!(activity_of(&service).await.pop().unwrap(), ("freed".to_owned(), folder, "1 file, 64.0 KiB".to_owned()));

        service.unregister_root().await.unwrap();
        assert!(activity_of(&service).await.is_empty(), "a Forget drops the activity");
    }

    /// `LocalBytes`, for a folder with a placeholder and a
    /// downloaded file: what the downloaded file takes (and the placeholder
    /// its next to nothing), measured on its own after the download. On the
    /// paused clock, so the five seconds between two walks cost nothing.
    #[tokio::test(start_paused = true)]
    async fn local_bytes_are_what_the_downloaded_file_takes() {
        let (service, root_dir, _dir) = local_folder(&[("a.bin", 64 * 1024, true), ("b.bin", 64 * 1024, false)]).await;
        let (a, b) = (root_dir.path().join("a.bin"), root_dir.path().join("b.bin"));
        assert!(data_blocks(&b) < 8, "b.bin is a placeholder");
        let expected = (data_blocks(&a) + data_blocks(&b)) * 512;
        assert!(expected >= 64 * 1024);
        let mut state = service.state().subscribe();
        tokio::time::timeout(Duration::from_secs(60), state.wait_for(|s| s.local_bytes == expected))
            .await
            .unwrap_or_else(|_| panic!("LocalBytes stayed {}, not {expected}", service.status().1))
            .unwrap();
    }

    // --- Activity and space accounting corner cases ---------------------------

    /// Item 4: a download that ends after its folder was forgotten records
    /// nothing in the folder registered next — `Hydrate` takes no lifecycle
    /// lock, so a Forget does not wait for it.
    #[tokio::test]
    async fn a_download_that_ends_after_its_folder_is_forgotten_is_not_in_the_next_ones_activity() {
        let (service, root_a, _dir) = local_folder(&[("a.bin", 4096, false)]).await;
        let (open, gate) = tokio::sync::oneshot::channel();
        install_source(&service, Arc::new(Gated(std::sync::Mutex::new(Some(gate)))));
        let mut transfers = service.report().transfers.subscribe();
        let filling = {
            let (service, a) = (Arc::clone(&service), root_a.path().join("a.bin"));
            tokio::spawn(async move { service.hydrate_now(&a).await })
        };
        tokio::time::timeout(Duration::from_secs(10), transfers.wait_for(|all| !all.is_empty())).await.unwrap().unwrap();

        service.unregister_root().await.unwrap();
        let root_b = tempfile::tempdir().unwrap();
        service.register_root_without_interception(root_b.path()).await.unwrap();
        open.send(()).unwrap();
        assert!(filling.await.unwrap().is_err());

        assert_eq!(activity_of(&service).await, Vec::new(), "folder A's download is not folder B's activity");
    }

    /// Item 5: the walker measuring `LocalBytes` ends with the service —
    /// it held the state, so nothing waiting on it ever saw the end.
    #[tokio::test(start_paused = true)]
    async fn dropping_the_service_ends_its_walker() {
        let (service, _root, _dir) = local_folder(&[("a.bin", 4096, true)]).await;
        assert!(service.report().space.running(), "the registration started it");
        let mut state = service.state().subscribe();
        drop(service);
        let ended = tokio::time::timeout(Duration::from_secs(60), async { while state.changed().await.is_ok() {} }).await;
        assert!(ended.is_ok(), "something still holds the state: the walker");
    }

    /// Item 6: a fill gives its slot back before it records what it did. The
    /// log is held still here, so every recording waits: with four slots
    /// held by fills that are only recording, a fifth request was never
    /// filled at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_fill_lets_go_of_its_slot_before_it_records() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().canonicalize().unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        let report = Report::new(SyncStateHandle::new(SyncSnapshot {
            root_path: folder.display().to_string(),
            ..SyncSnapshot::default()
        }));
        let socket_path = folder.join("helper.sock");
        let mut seen = fake_helper(socket_path.clone());
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let (tx, rx) = mpsc::channel::<HydrateRequest>(8);
        let source: Arc<dyn ContentSource> = Arc::new(LocalDir::new(source_dir.path()));
        tokio::spawn(serve_hydrations_reporting(link, rx, source, InodeLocks::new(), report.clone()));

        let held = report.activity.hold();
        for n in 0..5u64 {
            std::fs::write(source_dir.path().join(format!("ITEM{n}")), vec![1u8; 1024]).unwrap();
            let fd = placeholder(&folder, &format!("f{n}.bin"), &format!("ITEM{n}"), 1024);
            tx.send(HydrateRequest { req_id: n, fd }).await.unwrap();
        }
        for _ in 0..5 {
            let answered = tokio::time::timeout(Duration::from_secs(10), seen.recv())
                .await
                .expect("a request waited for a slot held by a fill that was only recording");
            assert_eq!(answered.unwrap().1, 0);
        }
        drop(held);
    }

    /// Item 8: a file a download or another free-up holds the per-inode lock
    /// of is busy, not waited for.
    #[tokio::test]
    async fn free_up_space_counts_a_file_whose_lock_is_taken_as_busy() {
        let (service, root_dir, _dir) = local_folder(&[("a.bin", 64 * 1024, true)]).await;
        let a = root_dir.path().join("a.bin");
        let key = InodeKey::of(&std::fs::File::open(&a).unwrap()).unwrap();
        // The descriptor is closed again: only the lock stands in the way.
        let _held = service.locks().lock(key).await;
        let freed = tokio::time::timeout(Duration::from_secs(10), service.free_up_space())
            .await
            .expect("it waited for the lock")
            .unwrap();
        assert_eq!(freed, FreedUp { files: 0, bytes: 0, busy: 1, modified: 0, pinned: 0 });
        assert_eq!(service.item_state(&a).await, "hydrated");
    }

    /// Item 8: a downloaded file changed here is neither freed nor busy:
    /// it is left, as `Dehydrate` would leave it.
    #[tokio::test]
    async fn free_up_space_counts_a_file_changed_here_in_neither() {
        let (service, root_dir, _dir) = local_folder(&[("a.bin", 64 * 1024, true)]).await;
        let a = root_dir.path().join("a.bin");
        std::io::Write::write_all(&mut std::fs::OpenOptions::new().append(true).open(&a).unwrap(), b"mine").unwrap();
        let freed = service.free_up_space().await.unwrap();
        assert_eq!(freed, FreedUp { files: 0, bytes: 0, busy: 0, modified: 1, pinned: 0 });
        assert!(std::fs::read(&a).unwrap().ends_with(b"mine"), "the change is kept");
    }

    // --- "Always keep on this device" -----------------------------------------

    /// A folder registered without interception, with no helper anywhere,
    /// filled with `docs/a.bin`, `docs/b.bin` and `c.bin`, 64 KiB each, none
    /// of them downloaded. Returns the folder's path as registered.
    async fn folder_to_pin() -> (Arc<SyncService>, PathBuf, tempfile::TempDir, tempfile::TempDir) {
        let service = SyncService::new(None, None, None);
        let dir = tempfile::tempdir().unwrap();
        service.set_helper_socket(dir.path().join("no-helper.sock"));
        let source = dir.path().join("source");
        std::fs::create_dir_all(source.join("docs")).unwrap();
        for name in ["docs/a.bin", "docs/b.bin", "c.bin"] {
            std::fs::write(source.join(name), vec![3u8; 64 * 1024]).unwrap();
        }
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root_without_interception(root_dir.path()).await.unwrap();
        service.populate_from_directory(&source).await.unwrap();
        (service, root_dir.path().canonicalize().unwrap(), root_dir, dir)
    }

    /// Waits until nothing a pin asked for is pending or downloading: each
    /// download recorded, since a file leaves the queue only after that.
    async fn pinned_downloads_done(service: &SyncService) {
        for _ in 0..1000 {
            if service.pins.queued().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("still queued: {:?}", service.pins.queued());
    }

    fn pin_of(path: &Path) -> Option<Vec<u8>> {
        xattr::get(path, konedrive_fs::placeholder::XATTR_PIN).unwrap()
    }

    /// Pinning a folder queues every online-only file in it, and each is
    /// downloaded through the ordinary fill, `downloaded` event and all.
    /// What is outside the folder is left alone, and a file inside it is not
    /// pinned again on its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pinning_a_folder_downloads_what_is_in_it() {
        let (service, root, _root_dir, _dir) = folder_to_pin().await;
        let (a, b, c) = (root.join("docs/a.bin"), root.join("docs/b.bin"), root.join("c.bin"));

        assert_eq!(service.pin(&[root.join("docs")]).await.unwrap(), 2);
        pinned_downloads_done(&service).await;

        assert_eq!(std::fs::read(&a).unwrap(), vec![3u8; 64 * 1024]);
        assert_eq!(service.item_state(&b).await, "hydrated");
        assert_eq!(service.item_state(&c).await, "online-only");
        assert_eq!(pin_of(&root.join("docs")), Some(b"1".to_vec()));
        assert_eq!(service.pinned_count(), 1);
        let downloaded: Vec<String> =
            activity_of(&service).await.into_iter().filter(|(kind, ..)| kind == "downloaded").map(|(_, path, _)| path).collect();
        assert_eq!(downloaded.len(), 2, "{downloaded:?}");

        assert_eq!(service.pin(&[a.clone()]).await.unwrap(), 0);
        assert_eq!(pin_of(&a), None, "the folder pins it already");
        assert_eq!(service.pinned_count(), 1);
    }

    /// Free up space on a file a pinned folder keeps is refused, naming the
    /// folder — through `FreeUp` and through `Dehydrate` alike.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn freeing_up_what_a_pinned_folder_keeps_is_refused_naming_the_folder() {
        let (service, root, _root_dir, _dir) = folder_to_pin().await;
        let (docs, a) = (root.join("docs"), root.join("docs/a.bin"));
        service.pin(&[docs.clone()]).await.unwrap();
        pinned_downloads_done(&service).await;

        let refused = service.free_up(&[a.clone()]).await.unwrap_err();
        let expected = format!("{} is pinned by {}: unpin it first", a.display(), docs.display());
        assert!(matches!(&refused, SyncError::NotAllowed(why) if *why == expected), "{refused:?}");
        assert!(matches!(service.dehydrate(&a).await, Err(SyncError::NotAllowed(_))));
        assert!(matches!(service.unpin(&[a.clone()]).await, Err(SyncError::NotAllowed(_))));
        assert_eq!(service.item_state(&a).await, "hydrated");
        assert_eq!(pin_of(&docs), Some(b"1".to_vec()));

        // With the folder in the same call, whose pin that call takes off.
        let freed = service.free_up(&[docs.clone(), a.clone()]).await.unwrap();
        assert_eq!((freed.files, freed.pinned), (2, 0), "{freed:?}");
        assert_eq!((pin_of(&docs), service.pinned_count()), (None, 0));
    }

    /// Pins that came off before a later one could not stay off, and
    /// `PinnedCount` says so; nothing is freed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_free_up_whose_later_pin_cannot_come_off_keeps_the_count_right() {
        let (service, root, _root_dir, _dir) = folder_to_pin().await;
        let (docs, c) = (root.join("docs"), root.join("c.bin"));
        service.pin(&[docs.clone(), c.clone()]).await.unwrap();
        pinned_downloads_done(&service).await;
        fn fails_on_files(item: &File, on: bool) -> io::Result<()> {
            if item.metadata()?.is_file() {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            pin::set_pin(item, on)
        }

        assert!(matches!(service.free_up_with(&[docs.clone(), c.clone()], fails_on_files).await, Err(SyncError::Io(_))));

        assert_eq!((pin_of(&docs), pin_of(&c)), (None, Some(b"1".to_vec())));
        assert_eq!(service.pinned_count(), 1);
        assert_eq!(service.item_state(&root.join("docs/a.bin")).await, "hydrated", "nothing was freed");
    }

    /// A file queued while pinned whose pin is gone by its turn is not
    /// downloaded.
    #[tokio::test]
    async fn a_queued_file_no_longer_pinned_is_not_downloaded() {
        use pin::PinFill;
        let (service, root, _root_dir, _dir) = folder_to_pin().await;
        let a = root.join("docs/a.bin");

        assert_eq!(service.fill_pinned(&a).await, pin::Filled::Done);

        assert_eq!(service.item_state(&a).await, "online-only");
        assert!(activity_of(&service).await.is_empty(), "nothing was fetched");
    }

    /// Free up space on a pinned folder takes its pin off and frees what is
    /// in it — but a file with a pin of its own stays, counted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn freeing_up_a_pinned_folder_unpins_it_and_frees_all_but_a_pin_below() {
        let (service, root, _root_dir, _dir) = folder_to_pin().await;
        let (docs, a, b) = (root.join("docs"), root.join("docs/a.bin"), root.join("docs/b.bin"));
        service.pin(&[b.clone()]).await.unwrap();
        service.pin(&[docs.clone()]).await.unwrap();
        pinned_downloads_done(&service).await;
        assert_eq!(service.pinned_count(), 2);

        let freed = service.free_up(&[docs.clone()]).await.unwrap();

        assert_eq!((freed.files, freed.busy, freed.pinned), (1, 0, 1), "{freed:?}");
        assert!(freed.bytes >= 64 * 1024, "{freed:?}");
        assert_eq!(service.item_state(&a).await, "online-only");
        assert_eq!(service.item_state(&b).await, "hydrated", "its own pin keeps it");
        assert_eq!((pin_of(&docs), pin_of(&b)), (None, Some(b"1".to_vec())));
        assert_eq!(service.pinned_count(), 1);
    }

    /// `FreeUpSpace` leaves every file a pin keeps, and counts them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn free_up_space_leaves_pinned_files_and_counts_them() {
        let (service, root, _root_dir, _dir) = folder_to_pin().await;
        let c = root.join("c.bin");
        service.pin(&[root.join("docs")]).await.unwrap();
        service.hydrate_now(&c).await.unwrap();
        pinned_downloads_done(&service).await;

        let freed = service.free_up_space().await.unwrap();

        assert_eq!((freed.files, freed.busy, freed.pinned), (1, 0, 2), "{freed:?}");
        assert_eq!(service.item_state(&c).await, "online-only");
        assert_eq!(service.item_state(&root.join("docs/a.bin")).await, "hydrated");
        assert_eq!(service.item_state(&root.join("docs/b.bin")).await, "hydrated");
    }

    /// Item 8: a fill on open that fails is a `failed` event, and a full
    /// disk reads exactly "not enough disk space" — the words the window's
    /// notifier turns into "disk full".
    #[tokio::test]
    async fn a_failed_fill_on_open_is_recorded_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().canonicalize().unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        let fd = placeholder(&folder, "gone.bin", "GONE", 4096);
        let report = Report::new(SyncStateHandle::new(SyncSnapshot {
            root_path: folder.display().to_string(),
            ..SyncSnapshot::default()
        }));
        let mut added = report.activity.subscribe();
        let socket_path = folder.join("helper.sock");
        let mut seen = fake_helper(socket_path.clone());
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
        let source: Arc<dyn ContentSource> = Arc::new(LocalDir::new(source_dir.path()));
        tokio::spawn(serve_hydrations_reporting(link, rx, source, InodeLocks::new(), report.clone()));
        tx.send(HydrateRequest { req_id: 3, fd }).await.unwrap();
        let answered = tokio::time::timeout(Duration::from_secs(10), seen.recv()).await.unwrap().unwrap();
        assert_eq!(answered, (3, libc::EIO));
        let event = tokio::time::timeout(Duration::from_secs(10), added.recv()).await.unwrap().unwrap();
        let gone = folder.join("gone.bin").display().to_string();
        assert_eq!((event.kind.as_str(), event.path.as_str()), ("failed", gone.as_str()));

        for errno in [libc::ENOSPC, libc::EDQUOT] {
            let event = fill_event(&Answered::Failed(FillError::Errno(errno)), "/r/f.bin", None).unwrap();
            assert_eq!((event.kind.as_str(), event.detail.as_str()), ("failed", activity::NO_DISK_SPACE));
        }
    }

    /// Item 8: a download whose caller goes away — a D-Bus call dropped, a
    /// replacement stopped with its poller — leaves `Transfers` with it.
    #[tokio::test]
    async fn a_cancelled_download_leaves_transfers() {
        use tokio::io::AsyncWriteExt;
        let (service, root_dir, _source_dir, _sockets, _helper) = populated_service(&vec![9u8; 64 * 1024]).await;
        let mut transfers = service.report().transfers.subscribe();
        let (mut writer, reader) = tokio::io::duplex(128 * 1024);
        install_source(&service, Arc::new(Piped { reader: std::sync::Mutex::new(Some(reader)), size: 64 * 1024 }));
        let filling = {
            let (service, target) = (Arc::clone(&service), root_dir.path().join("f.bin"));
            tokio::spawn(async move { service.hydrate_now(&target).await })
        };
        writer.write_all(&[9u8; 16 * 1024]).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), transfers.wait_for(|all| !all.is_empty())).await.unwrap().unwrap();
        filling.abort();
        tokio::time::timeout(Duration::from_secs(10), transfers.wait_for(|all| all.is_empty()))
            .await
            .expect("the cancelled download is still listed")
            .unwrap();
    }

    /// `populate_walk` walks a directory the *user* names — unlike
    /// `root::recover`'s hardened, descriptor-based walk, it is explicitly
    /// the offline test path, and its threat model does not include a
    /// racing or adversarial filesystem. It does include an ordinary
    /// mistake, though: a symlink somewhere in a source tree that points
    /// back at one of its own ancestors. If the walk ever decided to
    /// recurse on the strength of what a symlink points at, this would
    /// never return. `tokio::time::timeout` is the backstop in case the fix
    /// regresses; on a passing run it never comes close to firing.
    #[tokio::test]
    async fn populate_from_directory_does_not_follow_a_symlink_cycle_in_the_source() {
        let (service, _sockets, _helper) = service_with_helper().await;
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("real.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink(source_dir.path(), source_dir.path().join("loop")).unwrap();

        let root_dir = tempfile::tempdir().unwrap();
        service.register_root(root_dir.path()).await.unwrap();

        let created = tokio::time::timeout(
            Duration::from_secs(10),
            service.populate_from_directory(source_dir.path()),
        )
        .await
        .expect(
            "populate_from_directory did not return: a symlink cycle in the source was \
             descended",
        )
        .unwrap();

        assert_eq!(created, 1, "only the real file may produce a placeholder");
        assert!(
            !root_dir.path().join("loop").exists(),
            "a symlink to a directory must not be mirrored as one"
        );
    }

    /// The other half of the same fix: a symlink is never descended to
    /// decide whether it is a directory, but a symlink to a *regular* file
    /// is still worth a placeholder — the walk's read side, not its
    /// recursion decision, follows it.
    #[tokio::test]
    async fn populate_from_directory_creates_a_placeholder_for_a_symlink_to_a_regular_file() {
        let (service, _sockets, _helper) = service_with_helper().await;
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("real.bin"), vec![5u8; 4096]).unwrap();
        std::os::unix::fs::symlink(
            source_dir.path().join("real.bin"),
            source_dir.path().join("link.bin"),
        )
        .unwrap();

        let root_dir = tempfile::tempdir().unwrap();
        service.register_root(root_dir.path()).await.unwrap();

        let created = service.populate_from_directory(source_dir.path()).await.unwrap();

        assert_eq!(
            created, 2,
            "both the real file and the symlink to a regular file must be mirrored"
        );
        let placeholder = root_dir.path().join("link.bin");
        assert_eq!(
            std::fs::metadata(&placeholder).unwrap().len(),
            4096,
            "the placeholder must use the symlink's target's size"
        );
        assert_eq!(service.item_state(&placeholder).await, "online-only");
    }

    /// `item_state` must judge a file by the *currently* registered root,
    /// not by whether the file happens to carry konedrive xattrs — a file
    /// left behind by a root this daemon un-registered still carries them,
    /// but it is not this root's business any more. This is the specific
    /// claim `populate_from_directory_mirrors_the_tree_as_placeholders`
    /// (in `tests/sync_dbus.rs`) does *not* actually pin: there, the
    /// "outside the root" file has no konedrive xattrs at all, so it reads
    /// `not-managed` even with the containment check deleted (`read_state`
    /// returns `None` on its own). This test gives the file real, valid
    /// xattrs, so only the containment check can produce `not-managed`.
    #[tokio::test]
    async fn item_state_of_a_file_outside_the_current_root_is_not_managed_even_with_real_xattrs() {
        let (service, _sockets, _helper) = service_with_helper().await;

        let source_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("f.bin"), vec![3u8; 512]).unwrap();
        let root_a = tempfile::tempdir().unwrap();
        service.register_root(root_a.path()).await.unwrap();
        service.populate_from_directory(source_dir.path()).await.unwrap();
        let left_behind = root_a.path().join("f.bin");
        assert_eq!(service.item_state(&left_behind).await, "online-only", "sanity check");

        service.unregister_root().await.unwrap();
        let root_b = tempfile::tempdir().unwrap();
        service.register_root(root_b.path()).await.unwrap();

        assert_eq!(
            service.item_state(&left_behind).await,
            "not-managed",
            "a real konedrive-managed file left behind by a different, no-longer-registered \
             root must not be reported as this root's own"
        );
    }

    #[tokio::test]
    async fn a_file_recovery_finds_in_use_does_not_make_the_root_an_error() {
        // an interrupted file that something has open
        // — on reconnect, the suspended opener whose request is not served
        // yet, or a fill still running from the connection before
        // — refused recovery's lease and published `RootState = error`
        // with "could not reset", about a file that was then filled normally.
        let (service, _sockets, _helper) = service_with_helper().await;
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root(root_dir.path()).await.unwrap();
        let handle = std::fs::File::open(root_dir.path()).unwrap();
        konedrive_fs::placeholder::create_placeholder(
            &handle,
            "busy.bin",
            "ITEM",
            4096,
            std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
        )
        .unwrap();
        let path = root_dir.path().join("busy.bin");
        let busy = std::fs::File::options().read(true).write(true).open(&path).unwrap();
        konedrive_fs::placeholder::write_state(&busy, State::Hydrating).unwrap();

        service.resume().await;

        assert_eq!(service.root_state(), "ready", "{}", service.last_error());
        // logged, not a `LastError` that outlives it.
        assert_eq!(service.last_error(), "");
        assert_eq!(service.item_state(&path).await, "hydrating", "and it is left as found");
    }

    /// A `RecoveryReport` with `failed > 0` is "silently
    /// unrecoverable" case (a refused `ClearIgnore`) — it must not stay
    /// silent: `RegisterRoot` still succeeds (the root itself is usable),
    /// but `RootState`/`LastError` must say so.
    #[tokio::test]
    async fn a_failed_recovery_surfaces_through_root_state_and_last_error() {
        let (service, _sockets, helper) = service_with_helper().await;
        let root_dir = tempfile::tempdir().unwrap();

        // A folder that already carries a root id is exempt from
        // the "must be empty on first registration" check, which is exactly
        // what this test needs — the interrupted file below has to exist
        // *before* `register_root` runs, since recovery runs as part of it.
        // This mirrors what a restart after a crash actually looks like: the
        // folder was registered before, and this is the second registration.
        xattr::set(
            root_dir.path(),
            "user.konedrive.root",
            b"1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d",
        )
        .unwrap();

        // A file left `dehydrating` by a "crash", whose `ClearIgnore` the
        // helper refuses. (A file merely held open used to be the way to
        // force this; it is `busy` now, not a failure —
        // m11 — and has a test of its own above.)
        let path = root_dir.path().join("stuck.bin");
        std::fs::write(&path, vec![1u8; 4096]).unwrap();
        let file = std::fs::File::options().read(true).write(true).open(&path).unwrap();
        konedrive_fs::placeholder::write_state(&file, State::Dehydrating).unwrap();
        drop(file);
        helper.refuse(Seen::ClearIgnore, libc::EIO);

        service.register_root(root_dir.path()).await.unwrap();

        assert_eq!(service.root_state(), "error", "a recovery failure must not be silent");
        assert!(
            service.last_error().contains('1'),
            "the failure count must be in LastError: {}",
            service.last_error()
        );
    }

    // --- Per-inode serialization, measured through the service -----------

    /// A source that reports the largest number of fetches that were ever in
    /// flight at once, and holds each one open for `delay` so that a second
    /// one has time to arrive.
    struct CountingSource {
        dir: std::path::PathBuf,
        delay: Duration,
        in_flight: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CountingSource {
        fn new(dir: &std::path::Path, delay: Duration) -> (Arc<Self>, Arc<std::sync::atomic::AtomicUsize>) {
            let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let source = Arc::new(Self {
                dir: dir.to_path_buf(),
                delay,
                in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                peak: Arc::clone(&peak),
            });
            (source, peak)
        }
    }

    #[async_trait]
    impl ContentSource for CountingSource {
        async fn fetch(&self, item_id: &str, from: u64) -> Result<Fetched, SourceError> {
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.in_flight.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            tokio::time::sleep(self.delay).await;
            let fetched = LocalDir::new(self.dir.clone()).fetch(item_id, from).await;
            self.in_flight.fetch_sub(1, SeqCst);
            fetched
        }
    }

    fn install_source(service: &SyncService, source: Arc<dyn ContentSource>) {
        *service.source.lock().unwrap() = Some(source);
    }

    async fn wait_for_state(service: &SyncService, path: &std::path::Path, want: &str) {
        for _ in 0..400 {
            if service.item_state(path).await == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("{} never reached state {want}", path.display());
    }

    async fn populated_service(
        bytes: &[u8],
    ) -> (Arc<SyncService>, tempfile::TempDir, tempfile::TempDir, tempfile::TempDir, FakeHelper) {
        let (service, sockets, helper) = service_with_helper().await;
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("f.bin"), bytes).unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root(root_dir.path()).await.unwrap();
        service.populate_from_directory(source_dir.path()).await.unwrap();
        (service, root_dir, source_dir, sockets, helper)
    }

    /// The whole of C1. Two names for one inode must serialize.
    /// Measured the way the review measured it: a hard link, a source slow
    /// enough for both fills to overlap, and a count of how many were ever
    /// in flight at once. A path-keyed table gives two — and a failing
    /// source then has one fill's roll-back (`online-only` + `punch_all`)
    /// land on top of the other's committed `hydrated`, which is a file
    /// labelled `hydrated` over a hole.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_names_for_one_inode_are_never_filled_at_the_same_time() {
        let (service, root_dir, source_dir, _sockets, _helper) =
            populated_service(&vec![9u8; 2048]).await;
        let one = root_dir.path().join("f.bin");
        let another = root_dir.path().join("g.bin");
        std::fs::hard_link(&one, &another).unwrap();
        let (source, peak) = CountingSource::new(source_dir.path(), Duration::from_millis(300));
        install_source(&service, source);

        let first = {
            let service = Arc::clone(&service);
            let path = one.clone();
            tokio::spawn(async move { service.hydrate_now(&path).await })
        };
        let second = {
            let service = Arc::clone(&service);
            let path = another.clone();
            tokio::spawn(async move { service.hydrate_now(&path).await })
        };
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();

        assert_eq!(
            peak.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "two fills were in flight on one inode: the lock is keyed by something other than \
             the inode, so two names for one file do not serialize"
        );
        assert_eq!(std::fs::read(&one).unwrap(), vec![9u8; 2048]);
    }

    /// The other pair names: "a hydration request for a file being
    /// dehydrated runs after the dehydration finishes", and the reverse.
    /// The discriminator is the *outcome*, not the timing: a dehydration
    /// that runs while the fill is still in flight sees `state=hydrating`
    /// and is refused, so a `Dehydrate` that succeeds is one that waited.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_dehydration_waits_for_the_fill_of_the_same_inode() {
        let (service, root_dir, source_dir, _sockets, _helper) =
            populated_service(&vec![7u8; 4096]).await;
        let file = root_dir.path().join("f.bin");
        let (source, _peak) = CountingSource::new(source_dir.path(), Duration::from_millis(400));
        install_source(&service, source);

        let fill = {
            let service = Arc::clone(&service);
            let path = file.clone();
            tokio::spawn(async move { service.hydrate_now(&path).await })
        };
        wait_for_state(&service, &file, "hydrating").await;

        service.dehydrate(&file).await.expect(
            "the dehydration ran while the fill was still in flight: it saw `hydrating` and \
             refused, so nothing serialized the two",
        );
        fill.await.unwrap().unwrap();

        use std::os::unix::fs::MetadataExt;
        assert_eq!(service.item_state(&file).await, "online-only");
        assert!(std::fs::metadata(&file).unwrap().blocks() < 8, "the content is gone");
    }

    /// The same table, from the interception side: two suspended opens of
    /// one inode must not be filled at once either.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_hydrations_fills_one_inode_one_fill_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("helper.sock");
        let mut seen = fake_helper(socket_path.clone());
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM"), vec![4u8; 4096]).unwrap();
        let local = tempfile::tempdir().unwrap();
        let first = placeholder(local.path(), "file.bin", "ITEM", 4096);
        // A second descriptor for the very same inode, exactly as two
        // suspended opens of one file would arrive.
        let second = std::fs::File::options()
            .read(true)
            .write(true)
            .open(local.path().join("file.bin"))
            .unwrap()
            .into();

        let (source, peak) = CountingSource::new(remote.path(), Duration::from_millis(300));
        let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
        tokio::spawn(serve_hydrations(link, rx, source, InodeLocks::new()));
        tx.send(HydrateRequest { req_id: 1, fd: first }).await.unwrap();
        tx.send(HydrateRequest { req_id: 2, fd: second }).await.unwrap();

        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(10), seen.recv())
                .await
                .expect("both fills must answer")
                .unwrap();
        }
        assert_eq!(
            peak.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "two suspended opens of one inode were filled at once"
        );
    }

    /// The other half of that, and the reason the key has to be the inode
    /// rather than anything coarser: two *different* files must still be
    /// filled at the same time. A lock that over-matches turns four
    /// concurrent hydrations into a queue of one, which no other test here
    /// would notice.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_hydrations_fills_two_different_inodes_at_the_same_time() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("helper.sock");
        let mut seen = fake_helper(socket_path.clone());
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();

        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM"), vec![4u8; 4096]).unwrap();
        let local = tempfile::tempdir().unwrap();
        let one = placeholder(local.path(), "one.bin", "ITEM", 4096);
        let another = placeholder(local.path(), "another.bin", "ITEM", 4096);

        let (source, peak) = CountingSource::new(remote.path(), Duration::from_millis(300));
        let (tx, rx) = mpsc::channel::<HydrateRequest>(4);
        tokio::spawn(serve_hydrations(link, rx, source, InodeLocks::new()));
        tx.send(HydrateRequest { req_id: 1, fd: one }).await.unwrap();
        tx.send(HydrateRequest { req_id: 2, fd: another }).await.unwrap();

        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(10), seen.recv())
                .await
                .expect("both fills must answer")
                .unwrap();
        }
        assert_eq!(
            peak.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "two unrelated files were filled one after the other: the lock matches more than \
             the inode it is supposed to"
        );
    }

    // --- The gate `hydrate_now` writes through -------------

    /// A registration that has been removed or replaced no longer authorises
    /// writing inside that folder. `dehydrate` checks this
    /// — through `SyncRoot::open_inside`, which verifies the folder
    /// still carries *this* root's id — and `hydrate_now` reached its target
    /// through a `starts_with` on a canonicalized string, which cannot.
    #[tokio::test]
    async fn hydrate_now_refuses_a_root_that_no_longer_carries_its_registration() {
        let (service, root_dir, _source_dir, _sockets, _helper) =
            populated_service(&vec![1u8; 2048]).await;
        let file = root_dir.path().join("f.bin");
        xattr::remove(root_dir.path(), "user.konedrive.root").unwrap();

        let error = service.hydrate_now(&file).await.unwrap_err();
        assert!(
            matches!(error, SyncError::OutsideRoot),
            "expected a refusal, got {error:?}"
        );
        use std::os::unix::fs::MetadataExt;
        assert!(
            std::fs::metadata(&file).unwrap().blocks() < 8,
            "nothing may be written through a registration that is gone"
        );
    }

    /// C3, as it was measured: the window is not a microsecond race. The
    /// old order canonicalized the caller's path, checked *that string*
    /// against the root, awaited the per-inode lock — which has no time
    /// limit, because the fill it waits for has none — and then opened the
    /// checked string by name. An ordinary directory rename inside the root
    /// in that window sent the write outside the root, and `Hydrate`
    /// reported success.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_directory_swapped_while_a_hydration_waits_cannot_redirect_it() {
        let (service, _sockets, _helper) = service_with_helper().await;
        let base = tempfile::tempdir().unwrap();
        let root_dir = base.path().join("root");
        let outside_dir = base.path().join("outside");
        std::fs::create_dir(&root_dir).unwrap();
        std::fs::create_dir(&outside_dir).unwrap();

        let source_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(source_dir.path().join("sub")).unwrap();
        std::fs::write(source_dir.path().join("sub").join("f.bin"), vec![5u8; 1024]).unwrap();
        service.register_root(&root_dir).await.unwrap();
        service.populate_from_directory(source_dir.path()).await.unwrap();

        // The file the measured probe emptied: outside the root, and
        // carrying exactly the xattrs that made the old code treat whatever
        // it opened as one of ours.
        let victim = outside_dir.join("f.bin");
        std::fs::write(&victim, b"the user's own data").unwrap();
        {
            let file = std::fs::File::options().read(true).write(true).open(&victim).unwrap();
            xattr::FileExt::set_xattr(&file, "user.konedrive.item-id", b"sub/f.bin").unwrap();
            konedrive_fs::placeholder::write_state(&file, State::OnlineOnly).unwrap();
        }

        let target = root_dir.join("sub").join("f.bin");
        let (source, _peak) = CountingSource::new(source_dir.path(), Duration::from_millis(400));
        install_source(&service, source);

        let first = {
            let service = Arc::clone(&service);
            let path = target.clone();
            tokio::spawn(async move { service.hydrate_now(&path).await })
        };
        wait_for_state(&service, &target, "hydrating").await;
        let second = {
            let service = Arc::clone(&service);
            let path = target.clone();
            tokio::spawn(async move { service.hydrate_now(&path).await })
        };
        // Long enough for the second call to be waiting, short enough to be
        // well inside the first fill's 400 ms.
        tokio::time::sleep(Duration::from_millis(60)).await;
        std::fs::rename(root_dir.join("sub"), base.path().join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside_dir, root_dir.join("sub")).unwrap();

        let _ = first.await.unwrap();
        let _ = second.await.unwrap();

        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"the user's own data",
            "a file outside the sync root was overwritten with hydration content"
        );
    }

    // --- `Hydrate` never reports success without the bytes ---------------

    /// The plainest form of the rule: a file this daemon does not manage is
    /// refused, not called done.
    #[tokio::test]
    async fn hydrate_now_refuses_a_file_with_no_konedrive_xattrs() {
        let (service, root_dir, _source_dir, _sockets, _helper) =
            populated_service(&vec![1u8; 512]).await;
        let stray = root_dir.path().join("stray.txt");
        std::fs::write(&stray, b"not ours").unwrap();

        let error = service.hydrate_now(&stray).await.unwrap_err();
        assert!(matches!(error, SyncError::NotManaged), "expected a refusal, got {error:?}");
    }

    /// A source that cannot serve the item must not be reported as success:
    /// the file is still a hole afterwards, and the caller was told so.
    #[tokio::test]
    async fn hydrate_now_reports_a_source_that_could_not_serve_the_file() {
        let (service, root_dir, _source_dir, _sockets, _helper) =
            populated_service(&vec![2u8; 4096]).await;
        let file = root_dir.path().join("f.bin");
        // A source directory with nothing in it: every fetch is NotFound.
        let empty = tempfile::tempdir().unwrap();
        install_source(&service, Arc::new(LocalDir::new(empty.path())));

        let error = service.hydrate_now(&file).await.unwrap_err();
        assert!(matches!(error, SyncError::Io(_)), "expected a failure, got {error:?}");
        assert_eq!(
            service.item_state(&file).await,
            "online-only",
            "a failed fill must leave the file where the next open can retry it"
        );
        use std::os::unix::fs::MetadataExt;
        assert!(std::fs::metadata(&file).unwrap().blocks() < 8, "and holding nothing");
    }

    /// With no content source at all there is nowhere for the bytes to come
    /// from, so there is nothing to report success about.
    #[tokio::test]
    async fn hydrate_now_refuses_when_no_content_source_is_registered() {
        let (service, _sockets, _helper) = service_with_helper().await;
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root(root_dir.path()).await.unwrap();
        std::fs::write(root_dir.path().join("f.bin"), b"x").unwrap();

        let error = service.hydrate_now(&root_dir.path().join("f.bin")).await.unwrap_err();
        assert!(matches!(error, SyncError::NoSource), "expected a refusal, got {error:?}");
    }

    /// A file labelled `hydrated` over a hole is §9's named
    /// failure, and a manual "download it now" is what repairs it — so
    /// `Hydrate` must not believe the label on its own.
    #[tokio::test]
    async fn hydrate_now_fills_a_hydrated_label_that_has_no_stamp_behind_it() {
        let (service, root_dir, _source_dir, _sockets, _helper) =
            populated_service(&vec![6u8; 4096]).await;
        let file = root_dir.path().join("f.bin");
        {
            let handle = std::fs::File::options().read(true).write(true).open(&file).unwrap();
            konedrive_fs::placeholder::write_state(&handle, State::Hydrated).unwrap();
        }
        assert_eq!(service.item_state(&file).await, "hydrated", "the label says it is there");

        service.hydrate_now(&file).await.unwrap();

        assert_eq!(
            std::fs::read(&file).unwrap(),
            vec![6u8; 4096],
            "the file was labelled `hydrated` over a hole and `Hydrate` did nothing about it"
        );
    }

    /// The other side of the same check, and the reason it is a stamp
    /// comparison rather than a size one: a `hydrated` file whose stamp does
    /// **not** match was edited locally, and there is no upload in this
    /// sub-project, so that edit is the only copy. Refuse loudly; never
    /// overwrite it with remote content.
    #[tokio::test]
    async fn hydrate_now_refuses_a_hydrated_file_that_was_edited_locally() {
        let (service, root_dir, _source_dir, _sockets, _helper) =
            populated_service(&vec![6u8; 4096]).await;
        let file = root_dir.path().join("f.bin");
        service.hydrate_now(&file).await.unwrap();
        std::fs::write(&file, b"what the user typed").unwrap();

        let error = service.hydrate_now(&file).await.unwrap_err();
        assert!(
            matches!(error, SyncError::ModifiedLocally),
            "expected a refusal, got {error:?}"
        );
        assert_eq!(
            std::fs::read(&file).unwrap(),
            b"what the user typed",
            "a local edit is the only copy of that data and must not be overwritten"
        );
    }

    /// §5.2 treats `dehydrating` as "hydrate it again". A dehydration that a
    /// crash — or a cancelled call — left half-done must not make `Hydrate`
    /// report success over whatever the punch had got to by then.
    #[tokio::test]
    async fn hydrate_now_fills_a_file_a_dehydration_left_half_done() {
        let (service, root_dir, _source_dir, _sockets, _helper) =
            populated_service(&vec![5u8; 4096]).await;
        let file = root_dir.path().join("f.bin");
        {
            let handle = std::fs::File::options().read(true).write(true).open(&file).unwrap();
            konedrive_fs::placeholder::write_state(&handle, State::Dehydrating).unwrap();
        }

        service.hydrate_now(&file).await.unwrap();

        assert_eq!(std::fs::read(&file).unwrap(), vec![5u8; 4096]);
        assert_eq!(service.item_state(&file).await, "hydrated");
    }

    /// A zero-byte file is created
    /// `hydrated` with no stamp (there is nothing to download), so freeing it
    /// up answered `ModifiedLocally` and the command line told the user their
    /// edits would be lost. It takes no space and there is nothing to free:
    /// it succeeds and changes nothing.
    #[tokio::test]
    async fn freeing_up_a_zero_byte_file_succeeds_and_changes_nothing() {
        let (service, root_dir, _source_dir, _sockets, helper) = populated_service(&[]).await;
        let file = root_dir.path().join("f.bin");
        assert_eq!(service.item_state(&file).await, "hydrated");
        helper.forget();

        let freed = service.dehydrate(&file).await;

        assert!(freed.is_ok(), "{freed:?}");
        assert_eq!(service.item_state(&file).await, "hydrated");
        assert!(helper.seen().is_empty(), "nothing needed the helper: {:?}", helper.seen());
    }

    /// Freeing up a file under the read-only lock works and leaves it 0444.
    /// The root is registered the way the test above has it; the file is
    /// put in it by hand, hydrated and stamped, and then locked.
    #[tokio::test]
    async fn freeing_up_a_locked_file_works_and_leaves_it_locked() {
        use std::os::unix::fs::PermissionsExt;
        let (service, root_dir, _source_dir, _sockets, _helper) = populated_service(&[]).await;
        let root = service.root().unwrap();
        let path = root.path.join("locked.bin");
        std::fs::write(&path, vec![1u8; 8192]).unwrap();
        {
            let file = File::options().read(true).write(true).open(&path).unwrap();
            konedrive_fs::placeholder::write_item_id(&file, "L").unwrap();
            konedrive_fs::placeholder::write_state(&file, State::Hydrated).unwrap();
            konedrive_fs::placeholder::write_stamp(&file).unwrap();
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        service.dehydrate(&path).await.unwrap();
        assert_eq!(state_of_path(&path), Some(State::OnlineOnly));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o444);
        drop(root_dir);
    }

    /// But a file that was *made* empty here is an edit like any other.
    #[tokio::test]
    async fn freeing_up_a_file_emptied_here_is_still_refused_as_modified() {
        let (service, root_dir, _source_dir, _sockets, _helper) =
            populated_service(&vec![3u8; 2048]).await;
        let file = root_dir.path().join("f.bin");
        service.hydrate_now(&file).await.unwrap();
        std::fs::File::options().write(true).open(&file).unwrap().set_len(0).unwrap();

        let freed = service.dehydrate(&file).await;

        assert!(matches!(freed, Err(SyncError::ModifiedLocally)), "{freed:?}");
    }

    /// A populate source inside the
    /// root is a source whose files are placeholders: `LocalDir` reads them
    /// through the daemon's own exemption, or with nothing intercepting at
    /// all, so a fill copies zeros into a file it then stamps `hydrated` —
    /// on the offline route the user will actually run. And a source that
    /// contains the root is mirrored into itself. Both are refused, and
    /// nothing is created.
    #[tokio::test]
    async fn a_populate_source_that_overlaps_the_root_is_refused() {
        let service = SyncService::new(None, None, None);
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("root");
        std::fs::create_dir(&root).unwrap();
        service.register_root_without_interception(&root).await.unwrap();
        let inside = root.join("source");
        std::fs::create_dir(&inside).unwrap();
        std::fs::write(inside.join("a.bin"), [1u8; 64]).unwrap();
        std::fs::write(outer.path().join("b.bin"), [2u8; 64]).unwrap();

        let from_inside = service.populate_from_directory(&inside).await;
        assert!(matches!(from_inside, Err(SyncError::Unsupported(_))), "{from_inside:?}");
        assert!(!root.join("a.bin").exists(), "nothing may be created from inside the root");

        let from_around = service.populate_from_directory(outer.path()).await;
        assert!(matches!(from_around, Err(SyncError::Unsupported(_))), "{from_around:?}");
        assert!(!root.join("b.bin").exists(), "nor from a directory containing it");
    }

    /// A root registered without interception, with one `online-only`
    /// placeholder `b.bin` in it, populated from a source outside it.
    async fn root_with_a_placeholder() -> (Arc<SyncService>, tempfile::TempDir, PathBuf, PathBuf) {
        let service = SyncService::new(None, None, None);
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("root");
        std::fs::create_dir(&root).unwrap();
        service.register_root_without_interception(&root).await.unwrap();
        let first = outer.path().join("first");
        std::fs::create_dir(&first).unwrap();
        std::fs::write(first.join("b.bin"), vec![7u8; 64 * 1024]).unwrap();
        service.populate_from_directory(&first).await.unwrap();
        let placeholder = root.join("b.bin");
        assert_eq!(service.item_state(&placeholder).await, "online-only", "sanity check");
        (service, outer, root, placeholder)
    }

    /// Whether `path` holds `hydrated` over nothing but zeros: the outcome
    /// every guard in this module exists to prevent.
    fn hydrated_over_zeros(path: &Path) -> bool {
        let file = File::open(path).unwrap();
        let hydrated = read_state(&file).unwrap() == Some(State::Hydrated);
        let content = std::fs::read(path).unwrap();
        hydrated && !content.is_empty() && content.iter().all(|&b| b == 0)
    }

    /// A source *directory* that overlaps the root is refused, but a source
    /// *file* can still lead into it: a symlink in the source pointing at a
    /// placeholder in the root, or a hardlink to one. `LocalDir` reads it
    /// with nothing intercepting — or through the daemon's own exemption —
    /// so a fill copies the placeholder's zeros into the file it fills, and
    /// stamps it `hydrated`. Such a source is refused, and nothing is
    /// created from it.
    #[tokio::test]
    async fn a_populate_source_file_that_leads_into_the_root_is_refused() {
        let (service, outer, root, placeholder) = root_with_a_placeholder().await;
        let second = outer.path().join("second");
        std::fs::create_dir(&second).unwrap();
        std::os::unix::fs::symlink(&placeholder, second.join("a.bin")).unwrap();

        let populated = service.populate_from_directory(&second).await;
        let mirrored = root.join("a.bin");
        let filled = if mirrored.exists() { Some(service.hydrate_now(&mirrored).await) } else { None };
        assert!(
            !(mirrored.exists() && hydrated_over_zeros(&mirrored)),
            "a symlink in the source to a placeholder in the root was filled with its zeros and \
             stamped hydrated (populate → {populated:?}, Hydrate → {filled:?})"
        );
        assert!(matches!(populated, Err(SyncError::Unsupported(_))), "{populated:?}");
        assert!(!mirrored.exists(), "nothing may be created from a source that leads into the root");

        let third = outer.path().join("third");
        std::fs::create_dir(&third).unwrap();
        std::fs::hard_link(&placeholder, third.join("h.bin")).unwrap();
        let populated = service.populate_from_directory(&third).await;
        let mirrored = root.join("h.bin");
        let filled = if mirrored.exists() { Some(service.hydrate_now(&mirrored).await) } else { None };
        assert!(
            !(mirrored.exists() && hydrated_over_zeros(&mirrored)),
            "a hardlink in the source to a placeholder was filled with its zeros and stamped \
             hydrated (populate → {populated:?}, Hydrate → {filled:?})"
        );
        assert!(matches!(populated, Err(SyncError::Unsupported(_))), "{populated:?}");
    }

    /// The same, decided again where the bytes are read: a
    /// source file that pointed somewhere harmless when the folder was
    /// populated and into the root by the time it is fetched is refused
    /// there, and the file it would have filled stays `online-only`.
    #[tokio::test]
    async fn a_source_file_that_leads_into_the_root_by_the_time_it_is_fetched_is_refused() {
        let (service, outer, root, placeholder) = root_with_a_placeholder().await;
        let second = outer.path().join("second");
        std::fs::create_dir(&second).unwrap();
        let elsewhere = outer.path().join("elsewhere.bin");
        std::fs::write(&elsewhere, vec![9u8; 64 * 1024]).unwrap();
        std::os::unix::fs::symlink(&elsewhere, second.join("a.bin")).unwrap();
        service.populate_from_directory(&second).await.unwrap();
        let mirrored = root.join("a.bin");
        assert_eq!(service.item_state(&mirrored).await, "online-only", "sanity check");

        std::fs::remove_file(second.join("a.bin")).unwrap();
        std::os::unix::fs::symlink(&placeholder, second.join("a.bin")).unwrap();
        let filled = service.hydrate_now(&mirrored).await;

        assert!(
            !hydrated_over_zeros(&mirrored),
            "a source file that now leads into the root was read, and its zeros stamped \
             hydrated (Hydrate → {filled:?})"
        );
        assert!(filled.is_err(), "{filled:?}");
        assert_eq!(service.item_state(&mirrored).await, "online-only");
    }

    /// And the case that must stay a no-op: a file that really is there.
    #[tokio::test]
    async fn hydrate_now_does_nothing_to_a_file_that_is_already_there() {
        let (service, root_dir, source_dir, _sockets, _helper) =
            populated_service(&vec![3u8; 2048]).await;
        let file = root_dir.path().join("f.bin");
        service.hydrate_now(&file).await.unwrap();

        let counted = Arc::new(LocalDir::new(source_dir.path()));
        install_source(&service, Arc::clone(&counted) as Arc<dyn ContentSource>);
        service.hydrate_now(&file).await.unwrap();
        assert_eq!(counted.fetches(), 0, "a complete file must not be fetched again");
    }

    // --- `ItemState` is a query ----------------------------

    /// `ItemState` must answer without opening the file. In production the
    /// reason is that an open of an `online-only` file under a marked
    /// directory *is* a download — the whole file is fetched as a side
    /// effect of asking what state it is in, and where nothing can serve it
    /// the denial makes the open fail and the answer comes back
    /// `not-managed` for a genuinely managed placeholder.
    ///
    /// Unprivileged, interception cannot be reached at all, so the measured
    /// property here is the open itself: a FIFO inside the folder answers
    /// promptly if nothing opens it, and never answers at all if something
    /// does — `open(O_RDONLY)` on a FIFO blocks until a writer arrives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn item_state_answers_without_opening_the_file() {
        let (service, root_dir, _source_dir, _sockets, _helper) =
            populated_service(&vec![1u8; 512]).await;
        let pipe = root_dir.path().join("pipe");
        nix::unistd::mkfifo(&pipe, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();

        let answer = tokio::time::timeout(Duration::from_secs(3), service.item_state(&pipe)).await;
        // Unblocks anything that did open it, so the runtime can shut down
        // even when this assertion fails. `O_NONBLOCK` on the write side
        // returns `ENXIO` when there is no reader, which is the passing case.
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&pipe);
        assert_eq!(
            answer.expect("ItemState opened the file and blocked on it"),
            "not-managed"
        );
        // ... and the ordinary answer still works.
        assert_eq!(service.item_state(&root_dir.path().join("f.bin")).await, "online-only");
    }

    // --- Invariant M1, through the helper's own eyes ---------------------

    /// M1: "every directory under a root is marked, and a new directory is
    /// marked before anything is created inside it". Both halves are
    /// measured from the helper's side — it counts the entries in the very
    /// descriptor it was handed — because that is the only place the order
    /// is observable. A mark that arrives after the directory has been
    /// filled reports a non-zero count; a directory that is never marked
    /// reports nothing at all.
    #[tokio::test]
    async fn every_new_directory_is_marked_before_anything_is_created_in_it() {
        let (service, _sockets, helper) = service_with_helper().await;
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source_dir.path().join("sub").join("deeper")).unwrap();
        std::fs::write(source_dir.path().join("sub").join("a.bin"), vec![1u8; 64]).unwrap();
        std::fs::write(
            source_dir.path().join("sub").join("deeper").join("b.bin"),
            vec![2u8; 64],
        )
        .unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root(root_dir.path()).await.unwrap();
        helper.forget();

        service.populate_from_directory(source_dir.path()).await.unwrap();

        let marks: Vec<Seen> = helper
            .seen()
            .into_iter()
            .filter(|s| matches!(s, Seen::MarkDir { .. }))
            .collect();
        assert_eq!(marks.len(), 2, "both new directories must be marked: {marks:?}");
        assert!(
            marks.iter().all(|s| matches!(s, Seen::MarkDir { entries: 0 })),
            "a directory was marked after its contents were created: {marks:?}"
        );
    }

    /// The crash window the same code left open: a directory that exists but
    /// was never marked — `create_dir` succeeded, `mark_dir` did not, the
    /// daemon died — is skipped by every later run, so it stays unmarked and
    /// everything under it stays uninterceptable. Marking is idempotent;
    /// skipping is not recoverable.
    #[tokio::test]
    async fn a_directory_that_already_exists_is_marked_again() {
        let (service, _sockets, helper) = service_with_helper().await;
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(source_dir.path().join("sub")).unwrap();
        std::fs::write(source_dir.path().join("sub").join("a.bin"), vec![1u8; 64]).unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root(root_dir.path()).await.unwrap();
        // Exactly what a crash between `create_dir` and `mark_dir` leaves.
        std::fs::create_dir(root_dir.path().join("sub")).unwrap();
        helper.forget();

        service.populate_from_directory(source_dir.path()).await.unwrap();

        assert!(
            helper.seen().iter().any(|s| matches!(s, Seen::MarkDir { .. })),
            "a directory left behind unmarked by a crash was never marked: {:?}",
            helper.seen()
        );
    }

    /// The item id is the path relative to the source root, which is what
    /// the content source resolves a fetch by. A bare file name gives every
    /// nested placeholder an id that fetches nothing — and the failure only
    /// shows up later, at the one moment the user is waiting for their file.
    #[tokio::test]
    async fn a_nested_placeholder_carries_an_item_id_that_can_fetch_it() {
        let (service, _sockets, _helper) = service_with_helper().await;
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(source_dir.path().join("sub")).unwrap();
        std::fs::write(source_dir.path().join("sub").join("b.bin"), vec![8u8; 1024]).unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root(root_dir.path()).await.unwrap();
        service.populate_from_directory(source_dir.path()).await.unwrap();

        let nested = root_dir.path().join("sub").join("b.bin");
        assert_eq!(
            xattr::get(&nested, "user.konedrive.item-id").unwrap().unwrap(),
            b"sub/b.bin",
            "the id must name the file inside the source, not just its last component"
        );
        service.hydrate_now(&nested).await.unwrap();
        assert_eq!(std::fs::read(&nested).unwrap(), vec![8u8; 1024]);
    }

    // --- Who may register, and what a failed registration leaves ---------

    async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
        for _ in 0..600 {
            if ready() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{what} never happened");
    }

    async fn service_with_account(
        account: StateHandle,
    ) -> (Arc<SyncService>, tempfile::TempDir, FakeHelper) {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        (SyncService::new(Some(link), Some(account), None), sockets, helper)
    }

    /// §3.1 refuses a registration "when nobody is signed in", which nothing
    /// checked: both interfaces live on the same object, and this is what
    /// wires the one to the other.
    #[tokio::test]
    async fn register_root_is_refused_while_nobody_is_signed_in() {
        let account = StateHandle::new(crate::state::AccountSnapshot::default());
        let (service, _sockets, _helper) = service_with_account(account.clone()).await;
        let root_dir = tempfile::tempdir().unwrap();

        let error = service.register_root(root_dir.path()).await.unwrap_err();
        assert!(matches!(error, SyncError::NotSignedIn), "{error:?}");
        assert!(
            xattr::get(root_dir.path(), "user.konedrive.root").unwrap().is_none(),
            "a refused registration must not have stamped the folder"
        );

        account.update(|s| s.state = SignInState::SignedIn);
        service.register_root(root_dir.path()).await.unwrap();
    }

    /// §3.1 refuses a registration that "overlaps another root". A second
    /// one used to be accepted and to replace the first silently: the first
    /// stayed registered with the helper — still marked, still walked — and
    /// `ItemState` started calling its files `not-managed`.
    #[tokio::test]
    async fn register_root_refuses_a_second_root() {
        let (service, _sockets, helper) = service_with_helper().await;
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        service.register_root(first.path()).await.unwrap();
        helper.forget();

        let error = service.register_root(second.path()).await.unwrap_err();

        assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");
        assert_eq!(
            service.root().unwrap().path,
            std::fs::canonicalize(first.path()).unwrap(),
            "the first root must still be the root"
        );
        assert!(
            helper.seen().is_empty(),
            "the helper was told about a root the daemon refused: {:?}",
            helper.seen()
        );
        assert!(
            xattr::get(second.path(), "user.konedrive.root").unwrap().is_none(),
            "and the refused folder must not have been stamped"
        );
    }

    /// A `RegisterRoot` that fails must leave nothing behind —
    /// no stored root, no published `RootPath` — or, now that a second root
    /// is refused, one failed call would make every retry answer "already
    /// registered". Measured through the only window a test has: the helper
    /// holds its `RegisterRoot` ack open, and the folder's registration is
    /// taken away while it does, so the recovery that follows fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_registration_whose_recovery_fails_leaves_no_root_behind() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let _helper = FakeHelper::start(socket_path.clone(), Duration::from_millis(400));
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let service = SyncService::new(Some(link), None, None);
        let root_dir = tempfile::tempdir().unwrap();

        let registering = {
            let service = Arc::clone(&service);
            let path = root_dir.path().to_path_buf();
            tokio::spawn(async move { service.register_root(&path).await })
        };
        tokio::time::sleep(Duration::from_millis(150)).await;
        xattr::remove(root_dir.path(), "user.konedrive.root").unwrap();

        let error = registering.await.unwrap().unwrap_err();
        assert!(matches!(error, SyncError::Io(_)), "{error:?}");
        assert!(service.root().is_none(), "a failed registration stored a root anyway");
        assert_eq!(service.state().get().root_path, "", "and published it");
        assert_eq!(service.root_state(), "error");
        // The retry a user would make next must not be refused.
        service.register_root(root_dir.path()).await.unwrap();
        assert_eq!(service.root_state(), "ready");
    }

    // --- Registering without interception ------------------

    /// The default stays fail-closed, and for the reason that outranks
    /// everything else here: no helper means no interception, and a
    /// placeholder nobody intercepts reads as zeros.
    #[tokio::test]
    async fn register_root_is_refused_without_a_helper() {
        let service = SyncService::new(None, None, None);
        let root_dir = tempfile::tempdir().unwrap();

        let error = service.register_root(root_dir.path()).await.unwrap_err();

        assert!(matches!(error, SyncError::NoHelper), "{error:?}");
        assert_eq!(service.root_state(), "none");
    }

    /// ...and the whole surface works in the mode that says so out loud.
    /// Before this, the helper-optionality already written into
    /// `populate_walk`, `unregister_root` and `hydrate_now` was unreachable
    /// dead code, because `RegisterRoot` gated all of it.
    #[tokio::test]
    async fn the_whole_flow_works_without_a_helper_when_it_is_asked_for_explicitly() {
        let service = SyncService::new(None, None, None);
        // No helper anywhere — not only no link — so a free-up goes ahead
        //, whatever this machine has at the real socket path.
        let no_helper = tempfile::tempdir().unwrap();
        service.set_helper_socket(no_helper.path().join("helper.sock"));
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(source_dir.path().join("sub")).unwrap();
        std::fs::write(source_dir.path().join("sub").join("b.bin"), vec![4u8; 4096]).unwrap();
        let root_dir = tempfile::tempdir().unwrap();

        service.register_root_without_interception(root_dir.path()).await.unwrap();

        assert_eq!(service.root_state(), "no-interception");
        assert!(
            service.last_error().contains("read as zeros"),
            "the one thing a user must not have to infer: {}",
            service.last_error()
        );

        assert_eq!(service.populate_from_directory(source_dir.path()).await.unwrap(), 1);
        let file = root_dir.path().join("sub").join("b.bin");
        assert_eq!(service.item_state(&file).await, "online-only");
        service.hydrate_now(&file).await.unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), vec![4u8; 4096]);
        assert_eq!(service.item_state(&file).await, "hydrated");
        service.dehydrate(&file).await.unwrap();
        assert_eq!(service.item_state(&file).await, "online-only");

        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!(meta.len(), 4096, "the size survives");
        assert!(meta.blocks() < 8, "the content is gone");
    }

    // --- Startup and the helper supervisor ----------

    /// §3.1's "persisted, so it survives a restart" — and §4.4's walk, which
    /// without it never ran at a startup at all: recovery only ever ran
    /// inside a `RegisterRoot` call, so after a crash a file left
    /// `hydrating` stayed that way until a human registered the folder
    /// again.
    ///
    /// order is pinned here too, for the first time: the helper
    /// records what it was asked, and the registration must come before the
    /// `ClearIgnore` recovery sends for the interrupted file. Both fake
    /// helpers used to discard everything they received, so nothing could
    /// tell the two orders apart.
    #[tokio::test]
    async fn a_persisted_root_comes_back_at_the_next_start_and_is_recovered_after_it() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        let config_dir = tempfile::tempdir().unwrap();
        let config_file = config_dir.path().join("config.toml");
        let root_dir = tempfile::tempdir().unwrap();
        let resolved = std::fs::canonicalize(root_dir.path()).unwrap();

        {
            let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
            let service = SyncService::new(Some(link), None, Some(persist(&config_file)));
            service.register_root(root_dir.path()).await.unwrap();
        }
        assert_eq!(
            Config::load(&config_file).unwrap().sync_root,
            resolved.display().to_string(),
            "the root must be written down where the next start can find it"
        );

        // What a crash during a hydration leaves behind.
        let stuck = root_dir.path().join("stuck.bin");
        std::fs::write(&stuck, vec![1u8; 4096]).unwrap();
        {
            let file = std::fs::File::options().read(true).write(true).open(&stuck).unwrap();
            konedrive_fs::placeholder::write_state(&file, State::Hydrating).unwrap();
        }

        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let restarted = SyncService::new(Some(link), None, Some(persist(&config_file)));
        helper.forget();
        restarted.resume().await;

        assert_eq!(restarted.root_state(), "ready");
        assert_eq!(restarted.root().unwrap().path, resolved, "the root must come back");
        assert_eq!(
            state_of_path(&stuck),
            Some(State::OnlineOnly),
            "startup recovery never ran: a file a crash left mid-hydration stays that way, \
             holding content nothing may trust"
        );

        let seen = helper.seen();
        let registered = seen.iter().position(|s| *s == Seen::RegisterRoot);
        let cleared = seen.iter().position(|s| *s == Seen::ClearIgnore);
        assert!(registered.is_some(), "the root must be registered again: {seen:?}");
        assert!(cleared.is_some(), "recovery must have cleared the ignore mark: {seen:?}");
        assert!(
            registered < cleared,
            "recovery ran before the registration it depends on: {seen:?}"
        );

        // And forgetting the folder must un-persist it, or the next start
        // brings back a root the user got rid of.
        restarted.unregister_root().await.unwrap();
        assert_eq!(
            Config::load(restarted.persist.as_ref().unwrap().store.file()).unwrap().sync_root,
            "",
            "a forgotten root must not come back at the next start"
        );
    }

    /// §8 step 2: an intercepted root's files may carry the ignore mark, and
    /// a file punched while it still carries one is empty **and**
    /// permanently un-intercepted — zeros with nothing left to notice them.
    /// So a dehydration in that root needs a helper, and refuses without
    /// one, however convenient it would be to carry on.
    #[tokio::test]
    async fn dehydrate_is_refused_when_an_intercepted_root_has_lost_its_helper() {
        let (service, root_dir, _source_dir, _sockets, _helper) =
            populated_service(&vec![7u8; 4096]).await;
        let file = root_dir.path().join("f.bin");
        service.hydrate_now(&file).await.unwrap();

        service.set_link(None);

        let error = service.dehydrate(&file).await.unwrap_err();
        assert!(matches!(error, SyncError::NoHelper), "{error:?}");
        assert_eq!(
            std::fs::read(&file).unwrap(),
            vec![7u8; 4096],
            "and the file must be exactly as it was"
        );
    }

    /// `HelperLink` fails outstanding and later calls loudly,
    /// which is right — but nothing reconnected, and the published state
    /// said `ready` with an empty `LastError` the whole time the folder was
    /// dead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_helper_that_goes_away_is_published_and_reconnected_to() {
        a_helper_that_goes_away_is_published_and_reconnected_to_with(false).await;
    }

    /// The supervisor used to find out
    /// the helper was gone only when `serve_hydrations` returned, and that
    /// joined every fill still running first — so with one long download in
    /// flight, the loss was not published (`RootState` stayed `ready`, the
    /// dead link was still handed out) and nothing reconnected until the
    /// download ended, while a restarted helper, re-marking the tree from
    /// `roots.json`, had no daemon to ask and denied every open `EIO` after
    /// 30 s. Here the download never ends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn losing_the_helper_is_published_and_reconnected_while_a_fill_still_runs() {
        a_helper_that_goes_away_is_published_and_reconnected_to_with(true).await;
    }

    async fn a_helper_that_goes_away_is_published_and_reconnected_to_with(fill_running: bool) {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        let root_dir = tempfile::tempdir().unwrap();
        let service = SyncService::new(None, None, None);
        // Long enough that the window in which the helper is gone is
        // comfortably observable, short enough to keep the test quick.
        tokio::spawn(supervise_helper(
            Arc::clone(&service),
            socket_path.clone(),
            Duration::from_millis(300),
        ));
        wait_until("the supervisor connected", || service.link().is_some()).await;
        service.register_root(root_dir.path()).await.unwrap();
        assert_eq!(service.root_state(), "ready");

        let remote = tempfile::tempdir().unwrap();
        std::fs::write(remote.path().join("ITEM"), [1u8; 64]).unwrap();
        let slow = Arc::new(LocalDir::new(remote.path()).delay(Duration::from_secs(3600)));
        let files = tempfile::tempdir().unwrap();
        let _held = fill_running.then(|| {
            install_source(&service, Arc::clone(&slow) as Arc<dyn ContentSource>);
            let fd = placeholder(files.path(), "slow.bin", "ITEM", 64);
            helper.send_request(1, &fd);
            fd
        });
        if fill_running {
            wait_until("the fill began", || slow.fetches() > 0).await;
        }
        helper.forget();

        helper.hang_up();

        wait_until("the helper's absence was published", || service.root_state() == "error").await;
        assert!(
            service.last_error().contains("not connected"),
            "the published error must say what happened: {}",
            service.last_error()
        );
        wait_until("the root was registered again", || {
            helper.seen().contains(&Seen::RegisterRoot)
        })
        .await;
        wait_until("the folder came back", || service.root_state() == "ready").await;
    }

    // --- Guards over verified-correct behaviour ------------

    /// R3. Unregistering a root must tell the helper, or the helper keeps
    /// the tree marked — and, with the uid no longer owning a root, answers
    /// every placeholder open in it `EIO` (measurement).
    #[tokio::test]
    async fn unregister_root_tells_the_helper() {
        let (service, _sockets, helper) = service_with_helper().await;
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root(root_dir.path()).await.unwrap();
        helper.forget();

        service.unregister_root().await.unwrap();

        assert!(
            helper.seen().contains(&Seen::UnregisterRoot),
            "the helper was never told the root is gone: {:?}",
            helper.seen()
        );
    }

    /// R4. Unregistering a root must forget its content source. Item ids are
    /// paths relative to the source, so a placeholder in the *next* root with
    /// the same relative name matches the old source exactly: `Hydrate`
    /// would fill it with the old folder's bytes and report success.
    #[tokio::test]
    async fn unregister_root_forgets_the_content_source() {
        let (service, _sockets, _helper) = service_with_helper().await;
        let first_source = tempfile::tempdir().unwrap();
        std::fs::write(first_source.path().join("f.bin"), vec![0xAAu8; 4096]).unwrap();
        let first = tempfile::tempdir().unwrap();
        service.register_root(first.path()).await.unwrap();
        service.populate_from_directory(first_source.path()).await.unwrap();
        service.unregister_root().await.unwrap();

        let second = tempfile::tempdir().unwrap();
        service.register_root(second.path()).await.unwrap();
        drop(placeholder(second.path(), "f.bin", "f.bin", 4096));
        let file = second.path().join("f.bin");

        let outcome = service.hydrate_now(&file).await;

        assert!(
            matches!(outcome, Err(SyncError::NoSource)),
            "the second root was hydrated from the first root's source: {outcome:?}"
        );
        assert_ne!(
            std::fs::read(&file).unwrap(),
            vec![0xAAu8; 4096],
            "and it holds the first folder's bytes"
        );
        assert_eq!(service.item_state(&file).await, "online-only");
    }

    /// Design §8.3, review I2: an empty folder that carries another account's
    /// drive holds nothing to adopt — the usual Remove, then Add, on the same
    /// folder — so it is taken, and the stale drive comes off; a folder with
    /// anything in it is still refused.
    #[tokio::test]
    async fn an_empty_folder_that_carries_another_drive_is_taken_and_a_full_one_is_not() {
        let config_dir = tempfile::tempdir().unwrap();
        let persist = persist(&config_dir.path().join("config.toml"));
        persist.store.record_drive(&persist.account, "DB").unwrap();
        let service = SyncService::new(None, None, Some(persist));
        service.set_helper_socket(config_dir.path().join("no-helper.sock"));
        let (full, empty) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        for dir in [full.path(), empty.path()] {
            xattr::set(dir, "user.konedrive.drive", b"DA").unwrap();
        }
        std::fs::write(full.path().join("theirs.txt"), b"x").unwrap();

        let refused = service.register_root_without_interception(full.path()).await;
        assert!(matches!(refused, Err(SyncError::ForeignFolder)), "{refused:?}");
        assert_eq!(xattr::get(full.path(), "user.konedrive.drive").unwrap().as_deref(), Some(&b"DA"[..]));

        service.register_root_without_interception(empty.path()).await.unwrap();
        assert_eq!(xattr::get(empty.path(), "user.konedrive.drive").unwrap(), None, "the stale drive is taken off");
    }

    /// Review M2: an account being removed is retired under its lifecycle
    /// lock, and registers nothing from then on — not even a call that was
    /// waiting for that lock.
    #[tokio::test]
    async fn a_retired_account_registers_nothing() {
        let service = SyncService::new(None, None, None);
        service.retire().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let refused = service.register_root_without_interception(dir.path()).await;
        assert!(matches!(&refused, Err(SyncError::Io(why)) if why.contains("being removed")), "{refused:?}");
        assert_eq!(xattr::get(dir.path(), "user.konedrive.root").unwrap(), None, "the folder is not touched");
    }

    /// N6. The persisted "intercepted" flag must survive a restart. A root
    /// registered without interception on a machine with no helper would
    /// otherwise be restored as an
    /// intercepted root, which waits for a helper that never comes: the
    /// folder simply would not come back.
    #[tokio::test]
    async fn a_root_persisted_without_interception_comes_back_without_a_helper() {
        let config_dir = tempfile::tempdir().unwrap();
        let config_file = config_dir.path().join("config.toml");
        let root_dir = tempfile::tempdir().unwrap();
        {
            let service = SyncService::new(None, None, Some(persist(&config_file)));
            service.register_root_without_interception(root_dir.path()).await.unwrap();
        }
        assert!(
            !Config::load(&config_file).unwrap().sync_root_intercepted,
            "the mode must be written down with the root"
        );

        let restarted = SyncService::new(None, None, Some(persist(&config_file)));
        restarted.resume().await;

        assert_eq!(
            restarted.root().map(|r| r.path),
            Some(std::fs::canonicalize(root_dir.path()).unwrap()),
            "a root registered without interception did not come back after a restart"
        );
        assert_eq!(restarted.root_state(), "no-interception");
    }

    /// Q3, narrowed by. A root registered without interception
    /// *while a helper was connected* stays that way when the helper connects
    /// again — after a reconnect, and after a restart: the user asked for
    /// this mode by name with interception on offer, and quietly changing
    /// what protects their files is not this daemon's call. (One registered
    /// that way because no helper was connected does switch; see below.)
    #[tokio::test]
    async fn a_helper_reconnecting_does_not_upgrade_a_root_registered_without_interception_on_purpose() {
        let (service, helper, config_file, sockets, _config_dir) =
            service_with_config(Duration::ZERO).await;
        let socket_path = sockets.path().join("helper.sock");
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root_without_interception(root_dir.path()).await.unwrap();
        assert_eq!(
            Config::load(&config_file).unwrap().sync_root_upgrade_when_helper,
            Some(false),
            "a choice made with a helper connected must be written down as one"
        );

        // What `supervise_helper` does when the connection drops and the
        // helper answers again.
        service.set_link(None);
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        helper.forget();
        service.set_link(Some(link));
        service.resume().await;
        drop(service);

        // And a restart, with the helper there from the start.
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let restarted = SyncService::new(Some(link), None, Some(persist(&config_file)));
        restarted.restore().await;
        restarted.resume().await;

        assert_eq!(restarted.root_state(), "no-interception");
        assert!(
            restarted.last_error().contains("read as zeros"),
            "the warning must still be there: {}",
            restarted.last_error()
        );
        assert!(
            !helper.seen().contains(&Seen::RegisterRoot),
            "the root was registered with the helper behind the user's back: {:?}",
            helper.seen()
        );
    }

    // --- A folder registered without the helper, and the helper arriving

    /// A service that persists into a config file of its own, with no link
    /// and no helper at `helper.sock` yet — the machine before the helper is
    /// installed — and a folder registered there without interception.
    async fn registered_before_the_helper() -> (Arc<SyncService>, PathBuf, PathBuf, [tempfile::TempDir; 3]) {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let config_dir = tempfile::tempdir().unwrap();
        let config_file = config_dir.path().join("config.toml");
        let service = SyncService::new(None, None, Some(persist(&config_file)));
        service.set_helper_socket(&socket_path);
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root_without_interception(root_dir.path()).await.unwrap();
        assert_eq!(service.root_state(), "no-interception");
        (service, socket_path, config_file, [sockets, config_dir, root_dir])
    }

    /// Found in real use: a folder registered while the helper was
    /// not installed ("Use Without the Helper") stayed that way once it was,
    /// and every file in it read as zeros until a Forget and a new
    /// registration. The helper connecting switches it: the root is
    /// registered with the helper — whose walk marks every directory in it,
    /// as at every restart — recovered, and written down as intercepted.
    /// What is placed in it afterwards is marked first (invariant M1).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_folder_registered_without_the_helper_switches_to_interception_when_the_helper_connects() {
        let (service, socket_path, config_file, dirs) = registered_before_the_helper().await;
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join("a/b")).unwrap();
        std::fs::write(source.path().join("a/b/f.bin"), [7u8; 64]).unwrap();
        service.populate_from_directory(source.path()).await.unwrap();

        // The helper is installed and started after the folder was registered.
        let supervisor =
            tokio::spawn(supervise_helper(Arc::clone(&service), socket_path.clone(), Duration::from_millis(10)));
        let helper = FakeHelper::start(socket_path, Duration::ZERO);
        wait_until("the folder switched to interception", || service.root_state() == "ready").await;

        assert_eq!(
            helper.seen().first(),
            Some(&Seen::RegisterRoot),
            "the root must be registered with the helper, whose walk marks every directory: {:?}",
            helper.seen()
        );
        assert_eq!(service.last_error(), "", "the no-interception warning must go");
        let config = Config::load(&config_file).unwrap();
        assert!(config.sync_root_intercepted, "the switch must be written down, or a restart undoes it");
        assert_eq!(config.sync_root, resolved(dirs[2].path()));

        // `a/` and `a/b/` are marked again as they are passed (see
        // `a_directory_that_already_exists_is_marked_again`); `c/` is new.
        helper.forget();
        std::fs::create_dir(source.path().join("c")).unwrap();
        std::fs::write(source.path().join("c/g.bin"), [8u8; 64]).unwrap();
        service.populate_from_directory(source.path()).await.unwrap();
        assert!(
            helper.seen().contains(&Seen::MarkDir { entries: 0 }),
            "a directory placed after the switch must be marked before anything is created in \
             it: {:?}",
            helper.seen()
        );
        supervisor.abort();
    }

    /// A folder registered without interception because no helper was
    /// connected is written down as one to switch, so that a restart before
    /// the helper arrives still switches it when the helper does.
    #[tokio::test]
    async fn a_registration_made_with_no_helper_is_written_down_to_switch_and_switches_after_a_restart() {
        let (service, socket_path, config_file, dirs) = registered_before_the_helper().await;
        assert_eq!(Config::load(&config_file).unwrap().sync_root_upgrade_when_helper, Some(true));
        drop(service);

        let restarted = SyncService::new(None, None, Some(persist(&config_file)));
        restarted.set_helper_socket(&socket_path);
        restarted.restore().await;
        restarted.resume().await;
        assert_eq!(restarted.root_state(), "no-interception");

        let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        restarted.set_link(Some(link));
        restarted.resume().await;

        assert_eq!(restarted.root_state(), "ready", "{}", restarted.last_error());
        assert_eq!(helper.seen().first(), Some(&Seen::RegisterRoot));
        let config = Config::load(&config_file).unwrap();
        assert!(config.sync_root_intercepted);
        assert_eq!(config.sync_root_upgrade_when_helper, Some(false), "nothing is left to switch");
        assert_eq!(config.sync_root, resolved(dirs[2].path()));
    }

    /// Ruling 4 of: a `config.toml` written before the flag existed
    /// cannot say why its folder is without interception. It is read as a
    /// folder to switch — the user's own registration is exactly that case,
    /// and must switch once they restart the daemon or the helper reconnects
    /// — while an intercepted one has nothing to switch.
    #[tokio::test]
    async fn a_folder_without_interception_recorded_before_the_flag_existed_switches_when_the_helper_connects() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let config_dir = tempfile::tempdir().unwrap();
        let config_file = config_dir.path().join("config.toml");
        let root_dir = tempfile::tempdir().unwrap();
        let root_id = "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d";
        xattr::set(root_dir.path(), "user.konedrive.root", root_id.as_bytes()).unwrap();
        // What the daemon before wrote for such a folder (as migrated).
        write_config(
            &config_file,
            &format!(
                "path = \"{}\"\nid = \"{root_id}\"\nintercepted = false\nsource = \"local\"\nbaloo_excluded = false\n",
                resolved(root_dir.path())
            ),
        );
        assert_eq!(Config::load(&config_file).unwrap().sync_root_upgrade_when_helper, None);

        // The daemon restarts; the helper connects.
        let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        let restarted = SyncService::new(None, None, Some(persist(&config_file)));
        restarted.set_helper_socket(&socket_path);
        restarted.restore().await;
        restarted.resume().await;
        assert_eq!(restarted.root_state(), "no-interception");
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        restarted.set_link(Some(link));
        restarted.resume().await;

        assert_eq!(restarted.root_state(), "ready", "{}", restarted.last_error());
        assert_eq!(helper.seen().first(), Some(&Seen::RegisterRoot));
        assert!(Config::load(&config_file).unwrap().sync_root_intercepted);
    }

    /// Ruling 2 of: a switch that fails leaves the folder exactly as
    /// it was — without interception, written down that way — says why in
    /// `LastError`, and is tried again the next time the helper connects.
    #[tokio::test]
    async fn a_switch_the_helper_refuses_leaves_the_folder_as_it_was_and_is_tried_again_at_the_next_connect() {
        let (service, socket_path, config_file, _dirs) = registered_before_the_helper().await;
        let before = Config::load(&config_file).unwrap();
        let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        helper.refuse(Seen::RegisterRoot, libc::EIO);

        // What `supervise_helper` does the moment a helper answers.
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        service.set_link(Some(link));
        service.resume().await;

        assert_eq!(service.root_state(), "no-interception");
        let said = service.last_error();
        assert!(said.starts_with(NO_INTERCEPTION_WARNING), "the warning must stay: {said}");
        assert!(
            said.contains("switching this folder to interception failed") && said.contains("errno 5"),
            "LastError must say why the folder is still without interception: {said}"
        );
        assert_eq!(Config::load(&config_file).unwrap(), before, "config.toml must say what it said before");
        assert!(service.root().is_some());

        // The connection drops (`supervise_helper` lets go of the link), and
        // the helper connects again, and this time accepts.
        service.set_link(None);
        helper.refuse(Seen::RegisterRoot, 0);
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        service.set_link(Some(link));
        service.resume().await;

        assert_eq!(service.root_state(), "ready", "{}", service.last_error());
        assert_eq!(service.last_error(), "");
        assert!(Config::load(&config_file).unwrap().sync_root_intercepted);
    }

    /// A failed switch the helper may still hold — its registration failed,
    /// and it could not confirm it let go — is kept intercepted instead
    ///: a folder the helper may hold must never be one the
    /// daemon holds without interception. It is brought up at the next
    /// connect, as every intercepted folder is.
    #[tokio::test]
    async fn a_failed_switch_the_helper_may_still_hold_is_kept_intercepted_and_brought_up_at_the_next_connect() {
        let (service, socket_path, config_file, _dirs) = registered_before_the_helper().await;
        let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        helper.refuse(Seen::RegisterRoot, libc::EIO);
        helper.refuse(Seen::UnregisterRoot, libc::EIO);

        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        service.set_link(Some(link));
        service.resume().await;

        assert_eq!(service.root_state(), "error");
        assert!(
            service.last_error().contains("could not be told to let go"),
            "{}",
            service.last_error()
        );
        assert!(Config::load(&config_file).unwrap().sync_root_intercepted);
        // Held as intercepted: its Forget goes through the helper, which is
        // still refusing — a folder without interception would never ask.
        let error = service.unregister_root().await.unwrap_err();
        assert!(matches!(error, SyncError::Io(_)), "{error:?}");
        assert!(service.root().is_some());

        service.set_link(None);
        helper.refuse(Seen::RegisterRoot, 0);
        helper.refuse(Seen::UnregisterRoot, 0);
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        service.set_link(Some(link));
        service.resume().await;
        assert_eq!(service.root_state(), "ready", "{}", service.last_error());
    }

    // --- The mode boundary --------------------

    /// A fake helper, a service connected to it that persists into a config
    /// file of its own, and everything that has to outlive the test body.
    async fn service_with_config(
        register_root_delay: Duration,
    ) -> (Arc<SyncService>, FakeHelper, PathBuf, tempfile::TempDir, tempfile::TempDir) {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = FakeHelper::start(socket_path.clone(), register_root_delay);
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let config_file = config_dir.path().join("config.toml");
        let service = SyncService::new(Some(link), None, Some(persist(&config_file)));
        (service, helper, config_file, sockets, config_dir)
    }

    fn recorded_root(config_file: &Path) -> String {
        Config::load(config_file).unwrap().sync_root
    }

    fn resolved(path: &Path) -> String {
        std::fs::canonicalize(path).unwrap().display().to_string()
    }

    /// H133. An intercepted root may carry ignore marks, and only the
    /// helper's `UnregisterRoot` takes them off — its walk clears the mark of
    /// every file in the tree. A Forget that could not tell the helper used
    /// to be accepted anyway: the daemon forgot the folder while the helper
    /// kept it, marks, ignore marks, `roots.json` entry and all, and a later
    /// registration of the same folder without interception then punched a
    /// file that was still ignored. Measured in the VM suite: a reader got
    /// 65536 zero bytes and nothing was fetched.
    #[tokio::test]
    async fn forgetting_an_intercepted_root_needs_the_helper() {
        let (service, helper, config_file, _sockets, _config_dir) =
            service_with_config(Duration::ZERO).await;
        let link = service.link().unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root(root_dir.path()).await.unwrap();
        helper.forget();
        service.set_link(None);

        let error = service.unregister_root().await.unwrap_err();

        assert!(matches!(error, SyncError::NoHelper), "{error:?}");
        assert!(service.root().is_some(), "the refused Forget forgot the folder anyway");
        assert_eq!(
            recorded_root(&config_file),
            resolved(root_dir.path()),
            "and it must still be there at the next start"
        );
        let error =
            service.register_root_without_interception(root_dir.path()).await.unwrap_err();
        assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");

        // With the helper back, the same Forget goes through — to the helper.
        service.set_link(Some(link));
        service.unregister_root().await.unwrap();
        assert_eq!(helper.seen(), vec![Seen::UnregisterRoot]);
        assert!(service.root().is_none());
        assert_eq!(recorded_root(&config_file), "");
    }

    /// A Forget the helper answers `EPERM` has nothing left to undo: the
    /// helper holds no root of this uid under that id — it lost it, or never
    /// kept it — so no mark of that registration can be left, and keeping
    /// the folder would only make it impossible to forget. Any other refusal
    /// still keeps it, because then the helper may well hold it.
    #[tokio::test]
    async fn a_forget_the_helper_answers_eperm_goes_through_and_any_other_refusal_does_not() {
        let (service, helper, _config_file, _sockets, _config_dir) =
            service_with_config(Duration::ZERO).await;
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root(root_dir.path()).await.unwrap();

        helper.refuse(Seen::UnregisterRoot, libc::EIO);
        let error = service.unregister_root().await.unwrap_err();
        assert!(matches!(error, SyncError::Io(_)), "{error:?}");
        assert!(service.root().is_some(), "a helper that may still hold it was ignored");

        helper.refuse(Seen::UnregisterRoot, libc::EPERM);
        service.unregister_root().await.unwrap();
        assert!(service.root().is_none());
    }

    /// H134. A root registered without interception was never announced to
    /// the helper, so forgetting it has nothing to tell the helper — and
    /// telling it anyway made it impossible to forget while a helper was
    /// connected: the helper refuses to unregister a root the uid does not
    /// hold (`EPERM`), and the daemon kept the registration. Measured in
    /// small round 3, and again in the VM suite.
    #[tokio::test]
    async fn forgetting_a_root_registered_without_interception_never_asks_the_helper() {
        let (service, _sockets, helper) = service_with_helper().await;
        helper.refuse(Seen::UnregisterRoot, libc::EIO);
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root_without_interception(root_dir.path()).await.unwrap();

        service.unregister_root().await.unwrap();

        assert!(service.root().is_none());
        assert_eq!(service.root_state(), "none");
        assert!(helper.seen().is_empty(), "the helper was asked: {:?}", helper.seen());
    }

    /// H135, first half. `PopulateFromDirectory` used to mark every
    /// directory it created whenever a link merely existed — in a root
    /// registered without interception too. On a filesystem where the uid
    /// owns no helper root the helper refuses that `EPERM`, and the populate
    /// failed; where it owns one, the mark landed, the directory was
    /// intercepted, and an intercepted hydration ignore-marks the file —
    /// which is exactly what H135's skipped `ClearIgnore` relies on never
    /// happening. Both measured in the VM suite.
    #[tokio::test]
    async fn populating_a_root_registered_without_interception_marks_nothing() {
        let (service, _sockets, helper) = service_with_helper().await;
        helper.refuse(Seen::MarkDir { entries: 0 }, libc::EPERM);
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(source_dir.path().join("sub")).unwrap();
        std::fs::write(source_dir.path().join("sub").join("b.bin"), vec![4u8; 4096]).unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root_without_interception(root_dir.path()).await.unwrap();

        assert_eq!(service.populate_from_directory(source_dir.path()).await.unwrap(), 1);

        assert!(helper.seen().is_empty(), "the helper was asked: {:?}", helper.seen());
    }

    /// A root registered without interception, populated with one file
    /// `b.bin` and filled, ready to be freed up.
    async fn filled_without_interception(service: &SyncService) -> (tempfile::TempDir, PathBuf) {
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("b.bin"), vec![4u8; 64 * 1024]).unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root_without_interception(root_dir.path()).await.unwrap();
        service.populate_from_directory(source_dir.path()).await.unwrap();
        let file = root_dir.path().join("b.bin");
        service.hydrate_now(&file).await.unwrap();
        // The source goes; the file is what it holds now.
        std::mem::forget(source_dir);
        (root_dir, file)
    }

    fn data_blocks(path: &Path) -> u64 {
        std::fs::metadata(path).unwrap().blocks()
    }

    /// The daemon's local rule at a dehydration in a root registered
    /// without interception, with a link: the helper is asked to clear the
    /// file's ignore mark, as in any other root, and the punch follows. It
    /// used to be skipped, on the strength of a chain of reasoning — nothing
    /// in such a folder is intercepted, and interception resumes only through
    /// a walk that clears every mark — but that assumption failed again: a
    /// stale-marked file emptied here read zeros once the
    /// folder was intercepted again. The helper grants the clear on ownership
    /// alone now, so the `EPERM` that made this mode skip it is gone too.
    #[tokio::test]
    async fn a_dehydration_without_interception_clears_the_mark_through_its_link() {
        let (service, _sockets, helper) = service_with_helper().await;
        let (_root, file) = filled_without_interception(&service).await;

        service.dehydrate(&file).await.unwrap();

        assert_eq!(service.item_state(&file).await, "online-only");
        assert_eq!(helper.seen(), vec![Seen::ClearIgnore], "the mark must be cleared first");
    }

    /// And stopped by a clear that fails, as §8 step 2 has it everywhere
    /// else: the file is left hydrated, content and all.
    #[tokio::test]
    async fn a_dehydration_without_interception_whose_mark_is_not_cleared_changes_nothing() {
        let (service, _sockets, helper) = service_with_helper().await;
        let (_root, file) = filled_without_interception(&service).await;
        helper.refuse(Seen::ClearIgnore, libc::EIO);

        let refused = service.dehydrate(&file).await;

        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(service.item_state(&file).await, "hydrated");
        assert!(data_blocks(&file) > 64, "a file whose mark was not cleared was emptied");
    }

    /// with no link. A helper running with no link to this
    /// daemon — at startup before the first connection, or between a
    /// helper's restart and the reconnect — has a group that may hold a mark
    /// on the file, and nothing here can clear it: refused `NoHelper`, the
    /// file untouched. With no helper bound to the socket at all — the file
    /// a helper that exited left behind — no group of ours exists, and the
    /// punch goes ahead.
    #[tokio::test]
    async fn with_no_link_a_running_helper_stops_a_dehydration_and_an_exited_one_does_not() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let service = SyncService::new(None, None, None);
        service.set_helper_socket(&socket_path);
        let (_root, file) = filled_without_interception(&service).await;

        let running = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        let refused = service.dehydrate(&file).await;
        assert!(matches!(refused, Err(SyncError::NoHelper)), "{refused:?}");
        assert_eq!(service.item_state(&file).await, "hydrated");
        assert!(data_blocks(&file) > 64, "emptied while a helper ran with no link to it");
        assert!(running.seen().is_empty(), "the helper was connected to: {:?}", running.seen());

        // The helper exits: its socket file stays, with nothing bound to it.
        let stale = sockets.path().join("stale.sock");
        drop(socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None)
            .and_then(|fd| {
                bind(fd.as_raw_fd(), &UnixAddr::new(&stale).unwrap())?;
                Ok(fd)
            })
            .unwrap());
        assert!(stale.exists());
        service.set_helper_socket(&stale);
        service.dehydrate(&file).await.unwrap();
        assert_eq!(service.item_state(&file).await, "online-only");
    }

    /// at the other punch site: recovery of a root registered
    /// without interception clears an interrupted file's mark through its
    /// link, like any other recovery.
    #[tokio::test]
    async fn recovery_without_interception_clears_the_mark_through_its_link() {
        let (service, _sockets, helper) = service_with_helper().await;
        let (root_dir, stuck) = root_with_a_stuck_file();

        service.register_root_without_interception(root_dir.path()).await.unwrap();

        assert_eq!(state_of_path(&stuck), Some(State::OnlineOnly), "recovery did not run");
        assert_eq!(service.root_state(), "no-interception");
        assert_eq!(helper.seen(), vec![Seen::ClearIgnore], "the mark must be cleared first");
    }

    /// A folder with the root id already on it and one file a crash left
    /// `dehydrating`.
    fn root_with_a_stuck_file() -> (tempfile::TempDir, PathBuf) {
        let root_dir = tempfile::tempdir().unwrap();
        xattr::set(
            root_dir.path(),
            "user.konedrive.root",
            b"1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d",
        )
        .unwrap();
        let stuck = root_dir.path().join("stuck.bin");
        std::fs::write(&stuck, vec![1u8; 64 * 1024]).unwrap();
        {
            let file = std::fs::File::options().read(true).write(true).open(&stuck).unwrap();
            konedrive_fs::placeholder::write_state(&file, State::Dehydrating).unwrap();
        }
        (root_dir, stuck)
    }

    /// With no link while a helper runs, recovery of such a root leaves the
    /// interrupted file exactly as found — deferred, not failed — and runs
    /// again, clearing the mark, once the link is up. Without
    /// the second run the file would stay `dehydrating` until the next start.
    ///
    /// Here the folder is one registered without interception on purpose
    /// (`config.toml` says so), restored at a start that finds a helper
    /// running and no link to it yet.
    #[tokio::test]
    async fn recovery_deferred_while_an_unlinked_helper_runs_finishes_once_linked() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        let config_dir = tempfile::tempdir().unwrap();
        let config_file = config_dir.path().join("config.toml");
        let (root_dir, stuck) = root_with_a_stuck_file();
        write_config(
            &config_file,
            &format!("path = \"{}\"\nintercepted = false\nupgrade_when_helper = false\n", resolved(root_dir.path())),
        );
        let service = SyncService::new(None, None, Some(persist(&config_file)));
        service.set_helper_socket(&socket_path);

        service.resume().await;

        assert_eq!(state_of_path(&stuck), Some(State::Dehydrating), "reset with a mark unclearable");
        assert!(data_blocks(&stuck) > 64);
        assert_eq!(service.root_state(), "no-interception", "deferred is not an error");
        assert!(service.last_error().contains("not connected to it yet"), "{}", service.last_error());

        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        service.set_link(Some(link));
        service.resume().await;

        assert_eq!(state_of_path(&stuck), Some(State::OnlineOnly), "the deferred reset never ran");
        assert_eq!(helper.seen(), vec![Seen::ClearIgnore]);
        assert_eq!(service.last_error(), NO_INTERCEPTION_WARNING);
    }

    /// The same deferred file in a folder registered without interception
    /// because no helper was connected: the link's arrival switches the
    /// folder to interception, and the switch's own recovery —
    /// with the link, after the helper registered the root — resets it.
    #[tokio::test]
    async fn a_switch_to_interception_resets_what_recovery_deferred() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        let service = SyncService::new(None, None, None);
        service.set_helper_socket(&socket_path);
        let (root_dir, stuck) = root_with_a_stuck_file();

        service.register_root_without_interception(root_dir.path()).await.unwrap();
        assert_eq!(state_of_path(&stuck), Some(State::Dehydrating), "reset with a mark unclearable");

        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        service.set_link(Some(link));
        service.resume().await;

        assert_eq!(state_of_path(&stuck), Some(State::OnlineOnly), "the deferred reset never ran");
        assert_eq!(helper.seen(), vec![Seen::RegisterRoot, Seen::ClearIgnore]);
        assert_eq!(service.root_state(), "ready", "{}", service.last_error());
        assert_eq!(service.last_error(), "");
    }

    /// The route into no-interception mode that H133 alone leaves open. A
    /// root restored from `config.toml` used to exist nowhere in the daemon
    /// until the helper came back — `resume` returned early — so
    /// `RegisterRootWithoutInterception` of the very folder the helper still
    /// held (marks, ignore marks and all) was accepted, and the next
    /// dehydration there punched files that were still ignored. Measured in
    /// the VM suite: 65536 zero bytes. The root is held as registered now,
    /// and everything that has to go through the helper waits for it.
    #[tokio::test]
    async fn an_intercepted_root_restored_before_its_helper_is_back_is_held() {
        let (first, helper, config_file, sockets, _config_dir) =
            service_with_config(Duration::ZERO).await;
        let root_dir = tempfile::tempdir().unwrap();
        first.register_root(root_dir.path()).await.unwrap();
        let root_id = first.root().unwrap().root_id;
        drop(first);
        helper.forget();

        let restarted = SyncService::new(None, None, Some(persist(&config_file)));
        restarted.resume().await;

        let held = restarted.root().expect("a restored root must be held before the helper");
        assert_eq!(held.path.display().to_string(), resolved(root_dir.path()));
        assert_eq!(held.root_id, root_id, "under the id the helper holds it by");
        assert_eq!(restarted.root_state(), "error");
        assert!(
            restarted.last_error().contains("not connected"),
            "the published error must say what is missing: {}",
            restarted.last_error()
        );
        let error =
            restarted.register_root_without_interception(root_dir.path()).await.unwrap_err();
        assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");
        let elsewhere = tempfile::tempdir().unwrap();
        let error =
            restarted.register_root_without_interception(elsewhere.path()).await.unwrap_err();
        assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");
        let error = restarted.unregister_root().await.unwrap_err();
        assert!(matches!(error, SyncError::NoHelper), "{error:?}");
        let config = Config::load(&config_file).unwrap();
        assert_eq!(config.sync_root, resolved(root_dir.path()));
        assert!(config.sync_root_intercepted);
        assert!(helper.seen().is_empty(), "the helper was asked: {:?}", helper.seen());

        // The helper comes back: the same root is brought up, not a new one.
        let (link, _requests) =
            HelperLink::connect(&sockets.path().join("helper.sock")).await.unwrap();
        restarted.set_link(Some(link));
        restarted.resume().await;
        assert_eq!(restarted.root_state(), "ready");
        assert_eq!(helper.seen(), vec![Seen::RegisterRoot]);
    }

    /// ...and it stays held when bringing it up fails. A failed startup
    /// `bind` used to leave the daemon holding no root while the helper still
    /// held the folder, which is the same open door. Forgetting it still
    /// works, through the helper, under the id `config.toml` recorded — the
    /// folder is gone, so nothing could be read from it.
    #[tokio::test]
    async fn a_restored_root_that_cannot_be_brought_up_is_still_held() {
        let (first, helper, config_file, _sockets, _config_dir) =
            service_with_config(Duration::ZERO).await;
        let root_dir = tempfile::tempdir().unwrap();
        first.register_root(root_dir.path()).await.unwrap();
        let root_path = std::fs::canonicalize(root_dir.path()).unwrap();
        let link = first.link().unwrap();
        drop(first);
        drop(root_dir);
        helper.forget();

        let restarted = SyncService::new(Some(link), None, Some(persist(&config_file)));
        restarted.resume().await;

        assert_eq!(restarted.root_state(), "error");
        assert_eq!(restarted.root().map(|r| r.path), Some(root_path.clone()));
        let elsewhere = tempfile::tempdir().unwrap();
        let error =
            restarted.register_root_without_interception(elsewhere.path()).await.unwrap_err();
        assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");

        restarted.unregister_root().await.unwrap();
        assert_eq!(helper.seen(), vec![Seen::UnregisterRoot]);
        assert_eq!(recorded_root(&config_file), "");
    }

    /// A config written before the root id was recorded still restores its
    /// root as held: the id is read from the folder instead.
    #[tokio::test]
    async fn a_restored_root_with_no_recorded_id_takes_it_from_the_folder() {
        let config_dir = tempfile::tempdir().unwrap();
        let config_file = config_dir.path().join("config.toml");
        let root_dir = tempfile::tempdir().unwrap();
        let root_id = "1c2e4f5a-0b3c-4d5e-8f60-71829a3b4c5d";
        xattr::set(root_dir.path(), "user.konedrive.root", root_id.as_bytes()).unwrap();
        write_config(&config_file, &format!("path = \"{}\"\n", resolved(root_dir.path())));

        let restarted = SyncService::new(None, None, Some(persist(&config_file)));
        restarted.resume().await;

        assert_eq!(restarted.root().map(|r| r.root_id), Some(root_id.to_owned()));
        assert_eq!(restarted.root_state(), "error");
    }

    /// The id the helper holds a root by is what a restored root has to be
    /// forgotten by, so it is written down with the root, and removed with it.
    #[tokio::test]
    async fn the_root_id_is_recorded_with_the_root() {
        let (service, _helper, config_file, _sockets, _config_dir) =
            service_with_config(Duration::ZERO).await;
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root(root_dir.path()).await.unwrap();

        let config = Config::load(&config_file).unwrap();
        assert_eq!(config.sync_root_id, service.root().unwrap().root_id);

        service.unregister_root().await.unwrap();
        assert_eq!(Config::load(&config_file).unwrap().sync_root_id, "");
    }

    /// A root the helper holds and `config.toml` does not name is one the
    /// daemon cannot see after a restart, and so one it would accept for
    /// registration without interception. `RegisterRoot` used to write the
    /// root down only after the helper had saved it *and* recovery had
    /// walked the whole tree, so a crash anywhere in between left exactly
    /// that. It is written down first now.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_root_is_written_down_before_the_helper_hears_of_it() {
        let (service, helper, config_file, _sockets, _config_dir) =
            service_with_config(Duration::from_millis(400)).await;
        let root_dir = tempfile::tempdir().unwrap();

        let registering = {
            let service = Arc::clone(&service);
            let path = root_dir.path().to_path_buf();
            tokio::spawn(async move { service.register_root(&path).await })
        };
        wait_until("the helper was asked", || helper.seen().contains(&Seen::RegisterRoot)).await;

        assert_eq!(
            recorded_root(&config_file),
            resolved(root_dir.path()),
            "the helper was told about a root config.toml does not name"
        );
        registering.await.unwrap().unwrap();
    }

    /// And a root that cannot be written down is not registered at all: the
    /// helper is never told about it.
    ///. `config.toml` is the account
    /// sub-project's file too — it holds the `client_id` — and a copy that
    /// could not be read used to be treated as empty and written back from
    /// defaults, erasing everything in it. What could not be read is never
    /// overwritten: an intercepted registration is refused (its record must
    /// exist before the helper is told), and a registration without
    /// interception stands but is not recorded.
    #[tokio::test]
    async fn an_unreadable_config_is_never_overwritten() {
        let (service, helper, config_file, _sockets, _config_dir) =
            service_with_config(Duration::ZERO).await;
        let unreadable = "client_id = \"the account's own\"\nthis is not [toml\n";
        std::fs::write(&config_file, unreadable).unwrap();
        let root_dir = tempfile::tempdir().unwrap();

        let refused = service.register_root(root_dir.path()).await;
        assert!(matches!(refused, Err(SyncError::Io(_))), "{refused:?}");
        assert!(helper.seen().is_empty(), "the helper was told: {:?}", helper.seen());
        assert_eq!(std::fs::read_to_string(&config_file).unwrap(), unreadable);

        service.register_root_without_interception(root_dir.path()).await.unwrap();
        service.unregister_root().await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&config_file).unwrap(),
            unreadable,
            "config.toml was rewritten from defaults"
        );
    }

    #[tokio::test]
    async fn a_root_that_cannot_be_written_down_is_not_registered() {
        let (service, _sockets, helper) = service_with_helper().await;
        let config_dir = tempfile::tempdir().unwrap();
        let blocker = config_dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"").unwrap();
        let service = SyncService::new(service.link(), None, Some(persist(&blocker.join("config.toml"))));
        let root_dir = tempfile::tempdir().unwrap();

        let error = service.register_root(root_dir.path()).await.unwrap_err();

        assert!(matches!(error, SyncError::Io(_)), "{error:?}");
        assert!(service.root().is_none());
        assert!(helper.seen().is_empty(), "the helper was told: {:?}", helper.seen());
    }

    /// on both sides: a `RegisterRoot` that fails after the
    /// helper saved the root is undone at the helper, and in `config.toml`,
    /// so that neither is left holding a root the daemon does not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_registration_is_undone_at_the_helper_and_in_the_config() {
        let (service, helper, config_file, _sockets, _config_dir) =
            service_with_config(Duration::from_millis(400)).await;
        let root_dir = tempfile::tempdir().unwrap();

        let registering = {
            let service = Arc::clone(&service);
            let path = root_dir.path().to_path_buf();
            tokio::spawn(async move { service.register_root(&path).await })
        };
        wait_until("the helper was asked", || helper.seen().contains(&Seen::RegisterRoot)).await;
        xattr::remove(root_dir.path(), "user.konedrive.root").unwrap();

        let error = registering.await.unwrap().unwrap_err();
        assert!(matches!(error, SyncError::Io(_)), "{error:?}");
        assert!(service.root().is_none(), "a failed registration stored a root anyway");
        assert_eq!(helper.seen(), vec![Seen::RegisterRoot, Seen::UnregisterRoot]);
        assert_eq!(recorded_root(&config_file), "");
    }

    /// ...unless the helper cannot confirm it let go. Then the root is kept,
    /// intercepted, so it can only leave the way H133 allows — through the
    /// helper — and not by a registration without interception on top of
    /// a folder the helper may still be marking.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_registration_the_helper_may_still_hold_is_kept() {
        let (service, helper, config_file, _sockets, _config_dir) =
            service_with_config(Duration::from_millis(400)).await;
        helper.refuse(Seen::UnregisterRoot, libc::EIO);
        let root_dir = tempfile::tempdir().unwrap();

        let registering = {
            let service = Arc::clone(&service);
            let path = root_dir.path().to_path_buf();
            tokio::spawn(async move { service.register_root(&path).await })
        };
        wait_until("the helper was asked", || helper.seen().contains(&Seen::RegisterRoot)).await;
        xattr::remove(root_dir.path(), "user.konedrive.root").unwrap();

        let error = registering.await.unwrap().unwrap_err();
        assert!(matches!(error, SyncError::Io(_)), "{error:?}");
        assert!(service.root().is_some(), "a root the helper may hold was let go");
        assert_eq!(service.root_state(), "error");
        assert_eq!(recorded_root(&config_file), resolved(root_dir.path()));
        let error =
            service.register_root_without_interception(root_dir.path()).await.unwrap_err();
        assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");
    }

    /// The deterministic form of a D-Bus-activated first call: the bus name
    /// is claimed before `resume` runs, so a `RegisterRootWithoutInterception`
    /// can reach a restarted daemon before anything has looked at
    /// `config.toml`. It must find the restored root all the same.
    #[tokio::test]
    async fn a_registration_that_arrives_before_resume_still_finds_the_restored_root() {
        let (first, helper, config_file, _sockets, _config_dir) =
            service_with_config(Duration::ZERO).await;
        let root_dir = tempfile::tempdir().unwrap();
        first.register_root(root_dir.path()).await.unwrap();
        drop(first);
        helper.forget();

        let restarted = SyncService::new(None, None, Some(persist(&config_file)));
        let error =
            restarted.register_root_without_interception(root_dir.path()).await.unwrap_err();

        assert!(matches!(error, SyncError::AlreadyRegistered), "{error:?}");
        assert!(Config::load(&config_file).unwrap().sync_root_intercepted);
    }

    /// zbus runs every method call in a task of its own, so two
    /// registrations can be in flight at once. Both used to pass the "no
    /// root yet" check before either had committed, both reached the
    /// helper, and the last commit won: the helper then held a root the
    /// daemon did not — the state a later registration without interception
    /// of that folder turns into zeros. Registrations and Forgets now take
    /// turns, and the second one sees the first.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_registrations_at_once_leave_one_root_at_the_helper() {
        let (service, helper, _config_file, _sockets, _config_dir) =
            service_with_config(Duration::from_millis(300)).await;
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();

        let (a, b) = tokio::join!(
            service.register_root(first.path()),
            service.register_root(second.path())
        );

        assert!(
            a.is_ok() != b.is_ok(),
            "exactly one of two concurrent registrations may succeed: {a:?}, {b:?}"
        );
        let refused = a.err().or(b.err()).unwrap();
        assert!(matches!(refused, SyncError::AlreadyRegistered), "{refused:?}");
        assert_eq!(
            helper.seen(),
            vec![Seen::RegisterRoot],
            "the helper was told about a root the daemon does not hold"
        );
    }

    /// A dehydration decides whether to send `ClearIgnore` from the root's
    /// mode, and then may wait — for a fill of the same inode — before it
    /// punches. The mode must not change under it in the meantime: a root
    /// forgotten and registered again with interception while it waits
    /// could have the file ignore-marked by then, and the punch would skip
    /// the `ClearIgnore` that is suddenly needed.
    /// it waits for the fill without the lifecycle lock — a Forget is not
    /// held up by a download — and decides the mode only after, under the
    /// lock: a folder forgotten meanwhile is refused, nothing punched.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_dehydration_waiting_for_a_fill_does_not_hold_up_a_forget() {
        let (service, _sockets, _helper) = service_with_helper().await;
        let source_dir = tempfile::tempdir().unwrap();
        std::fs::write(source_dir.path().join("b.bin"), vec![4u8; 4096]).unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root_without_interception(root_dir.path()).await.unwrap();
        service.populate_from_directory(source_dir.path()).await.unwrap();
        let file = root_dir.path().join("b.bin");
        service.hydrate_now(&file).await.unwrap();

        // A fill of the same inode, in progress.
        let fill = service.locks().lock(key_of(&file)).await;
        let dehydrating = {
            let service = Arc::clone(&service);
            let file = file.clone();
            tokio::spawn(async move { service.dehydrate(&file).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        tokio::time::timeout(Duration::from_secs(2), service.unregister_root())
            .await
            .expect("the Forget waited for a dehydration waiting for a fill")
            .unwrap();

        drop(fill);
        let refused = dehydrating.await.unwrap();
        assert!(matches!(refused, Err(SyncError::NoRoot)), "{refused:?}");
        assert_eq!(std::fs::read(&file).unwrap(), vec![4u8; 4096], "nothing was punched");
    }

    // --- A folder that shows OneDrive ----------

    #[test]
    fn the_published_state_is_computed_from_the_registration_and_the_sync() {
        let mut s = SyncSnapshot { root_state: RootState::Ready, ..SyncSnapshot::default() };
        assert_eq!(published_state(&s), "ready");
        s.listing = true;
        assert_eq!(published_state(&s), "listing");
        s.sync_trouble = Some(SyncTrouble { text: "cannot reach OneDrive".into(), blocking: false });
        assert_eq!(published_state(&s), "listing", "no network is said, not an error");
        s.sync_trouble = Some(SyncTrouble { text: "signed out".into(), blocking: true });
        assert_eq!(published_state(&s), "error");
        s.root_state = RootState::Error;
        s.last_error = "the helper is not connected".into();
        s.replacement_note = "1 file(s) changed in OneDrive could not be updated here yet: no space".into();
        s.conflict_count = 1;
        assert_eq!(
            published_error(&s),
            "the helper is not connected. signed out. 1 file(s) changed in OneDrive could not be updated here yet: no space",
            "a conflict is not a problem; it is not in LastError"
        );
        assert_eq!(published_state(&SyncSnapshot::default()), "none");

        // `listing` never hides `no-interception`.
        let s = SyncSnapshot { root_state: RootState::NoInterception, listing: true, ..SyncSnapshot::default() };
        assert_eq!(published_state(&s), "no-interception");
    }

    /// HS3: while a folder waits for the helper, `RootState` reads `error`
    /// and `LastError` begins with what `HelperState` says — how to install
    /// it, start it, or see why it failed — ahead of whatever else is said.
    #[test]
    fn a_folder_waiting_for_the_helper_says_how_to_start_it() {
        let said = |helper_state| {
            let s = SyncSnapshot {
                root_state: RootState::Ready,
                waits_for_helper: true,
                helper_state,
                last_error: "recovery left 1 file".into(),
                ..SyncSnapshot::default()
            };
            (published_state(&s), published_error(&s))
        };
        let (state, error) = said(HelperState::NotInstalled);
        assert_eq!(state, "error");
        assert_eq!(
            error,
            "the konedrive helper is not installed: files are not kept in step and do not download when \
             opened. Install it: sudo scripts/install-helper.sh (see README). recovery left 1 file"
        );
        assert!(said(HelperState::Stopped).1.starts_with(
            "the konedrive helper is not running: start it with `sudo systemctl start konedrive-helper`. "
        ));
        assert!(said(HelperState::Failed).1.starts_with("the konedrive helper failed: see `systemctl status konedrive-helper`. "));
        assert!(said(HelperState::Unknown).1.starts_with("the konedrive helper is not connected. "));
        assert_eq!(said(HelperState::Connected).1, "recovery left 1 file", "nothing to say of a connected helper");

        let s = SyncSnapshot { root_state: RootState::Ready, helper_state: HelperState::Stopped, ..SyncSnapshot::default() };
        assert_eq!((published_state(&s), published_error(&s).as_str()), ("ready", ""), "a folder not waiting says nothing of it");
    }

    mod onedrive {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        use serde_json::json;
        use url::Url;
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use super::super::*;
        use super::{persist, wait_until, Config, FakeHelper, Seen};
        use crate::drive::{DriveClient, RetryPolicy};
        use crate::state::{AccountSnapshot, SignInState, StateHandle};
        use crate::sync::listing::Schedule;
        use crate::token::{AuthError, StaticToken, TokenSource};

        struct World {
            server: MockServer,
            config: tempfile::TempDir,
            folder: tempfile::TempDir,
            /// Where the fake `balooctl6` lives: `calls` gets every
            /// `add`/`rm` it is run with, appended one per line; its
            /// `baloofilerc` is what is excluded already — nothing, unless a
            /// test writes it.; `crate::sync::baloo`.
            baloo: tempfile::TempDir,
            /// A helper that acknowledges everything, at `sockets/helper.sock`:
            /// a folder that shows OneDrive is kept in step only with one
            /// (HS2). [`connected`] links a service to it.
            helper: FakeHelper,
            sockets: tempfile::TempDir,
        }

        impl Drop for World {
            fn drop(&mut self) {
                // A locked tree cannot be removed by the temporary directory.
                let _ = std::process::Command::new("chmod")
                    .args(["-R", "u+w"])
                    .arg(self.folder.path())
                    .status();
            }
        }

        /// A drive holding `docs/f.txt`, listed in full from the start and
        /// with no changes since from its delta link `L1`.
        async fn world() -> World {
            let server = MockServer::start().await;
            Mock::given(method("GET")).and(path("/me/drive"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D1"})))
                .mount(&server).await;
            Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": [], "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L1", server.uri())})))
                .with_priority(1)
                .mount(&server).await;
            Mock::given(method("GET")).and(path("/me/drive/root/delta"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "value": [
                        {"id": "R", "root": {}, "folder": {}},
                        {"id": "D", "name": "docs", "folder": {}, "parentReference": {"id": "R"}},
                        {"id": "F", "name": "f.txt", "size": 3, "cTag": "c1", "file": {}, "parentReference": {"id": "D"}}
                    ],
                    "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L1", server.uri())
                })))
                .with_priority(5)
                .mount(&server).await;
            let baloo = tempfile::tempdir().unwrap();
            write_fake_balooctl6(baloo.path());
            let sockets = tempfile::tempdir().unwrap();
            let helper = FakeHelper::start(sockets.path().join("helper.sock"), Duration::ZERO);
            World { server, config: tempfile::tempdir().unwrap(), folder: tempfile::tempdir().unwrap(), baloo, helper, sockets }
        }

        /// A new link to the world's helper.
        async fn link(w: &World) -> HelperLink {
            HelperLink::connect(&w.sockets.path().join("helper.sock")).await.unwrap().0
        }

        /// [`service`], linked to the world's helper: what a OneDrive folder
        /// is registered and kept in step with (HS2).
        async fn connected(w: &World, signed_in: bool) -> Arc<SyncService> {
            service_with(w, account(signed_in), Some(link(w).await), Arc::new(StaticToken::new("T")))
        }

        /// A fake `balooctl6`, so these tests never reach the real Baloo
        ///: `config add`/`config rm` are logged to `calls`, one
        /// call per line. What is excluded already is read from the
        /// `baloofilerc` beside it (`crate::sync::baloo`, B-I1b), never
        /// `~/.config`'s.
        fn write_fake_balooctl6(dir: &std::path::Path) {
            let script = dir.join("balooctl6");
            let log = dir.join("calls");
            std::fs::write(&script, format!("#!/bin/sh\necho \"$@\" >> '{}'\n", log.display())).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// Every `add`/`rm` the fake `balooctl6` was run with, in order.
        fn baloo_calls(w: &World) -> String {
            std::fs::read_to_string(w.baloo.path().join("calls")).unwrap_or_default()
        }

        /// Writes the test's `baloofilerc` as if `folder` (or, passed
        /// directly, a directory above it) were already excluded — the
        /// user's own doing, which says a registration must never
        /// add to or a Forget take off. In the form KConfig writes it.
        fn mark_already_excluded(w: &World, folder: &std::path::Path) {
            let line = format!("[General]\nexclude folders[$e]={}/\n", folder.display());
            std::fs::write(w.baloo.path().join("baloofilerc"), line).unwrap();
        }

        fn account(signed_in: bool) -> StateHandle {
            StateHandle::new(AccountSnapshot {
                state: if signed_in { SignInState::SignedIn } else { SignInState::SignedOut },
                ..AccountSnapshot::default()
            })
        }

        fn service(w: &World, signed_in: bool) -> Arc<SyncService> {
            service_with(w, account(signed_in), None, Arc::new(StaticToken::new("T")))
        }

        /// A service wired as `main` wires it — a drive, its paths — with an
        /// hour between cycles, so that any cycle a test sees was asked for.
        fn service_with(
            w: &World,
            account: StateHandle,
            link: Option<HelperLink>,
            tokens: Arc<dyn TokenSource>,
        ) -> Arc<SyncService> {
            let service = SyncService::new(link, Some(account), Some(persist(&w.config.path().join("config.toml"))));
            let drive = DriveClient::new(Url::parse(&format!("{}/", w.server.uri())).unwrap(), tokens)
                .unwrap()
                .with_retry(RetryPolicy { attempts: 2, default_wait: Duration::from_millis(5), max_wait: Duration::from_millis(10) });
            service.set_drive(drive);
            service.set_sync_paths(SyncPaths {
                tree_db: w.config.path().join("tree.sqlite"),
                rescue_dir: w.config.path().join("rescued"),
                thumbnails: Some(w.config.path().join("thumbnails")),
            });
            service.set_schedule(Schedule { interval: Duration::from_secs(3600), retry: vec![Duration::from_millis(50)] });
            // No helper in these tests, and none running: a punch goes by "no helper at all".
            service.set_helper_socket(w.config.path().join("no-helper.sock"));
            // The fake `balooctl6` and a `baloofilerc` of the test's own
            //: never the real ones, so these tests never touch
            // ~/.config/baloofilerc.
            service.set_baloo(crate::sync::baloo::Baloo {
                program: Some(w.baloo.path().join("balooctl6")),
                settings: Some(w.baloo.path().join("baloofilerc")),
                ..crate::sync::baloo::Baloo::disabled()
            });
            service
        }

        /// Tokens while the account reads signed in, and "signed out" — as
        /// `TokenManager` answers once the refresh token is gone — otherwise.
        struct AccountTokens {
            account: StateHandle,
            refused: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl TokenSource for AccountTokens {
            async fn access_token(&self) -> Result<String, AuthError> {
                if self.account.get().state == SignInState::SignedIn {
                    Ok("T".into())
                } else {
                    self.refused.fetch_add(1, Ordering::SeqCst);
                    Err(AuthError::SignedOut)
                }
            }

            async fn invalidate(&self) {}
        }

        fn config_of(w: &World) -> Config {
            Config::load(&w.config.path().join("config.toml")).unwrap()
        }

        fn mode(path: &std::path::Path) -> u32 {
            std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
        }

        async fn requests(w: &World) -> usize {
            w.server.received_requests().await.unwrap().len()
        }

        async fn deltas(w: &World) -> usize {
            w.server.received_requests().await.unwrap().iter().filter(|r| r.url.path() == "/me/drive/root/delta").count()
        }

        /// Delta requests that started a listing of the whole drive.
        async fn full_listings(w: &World) -> usize {
            w.server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.url.path() == "/me/drive/root/delta" && r.url.query().is_none())
                .count()
        }

        async fn wait_for_deltas(w: &World, more_than: usize) {
            for _ in 0..300 {
                if deltas(w).await > more_than {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("no delta request came");
        }

        /// The first cycle is over: its counts are published only once the
        /// folder has been made to match the tree.
        async fn listed(service: &SyncService) {
            wait_until("the drive is listed into the folder", || service.items() == (2, 2, 0)).await;
        }

        #[tokio::test]
        async fn a_folder_registered_while_signed_in_shows_onedrive_read_only() {
            let w = world().await;
            let service = connected(&w, true).await;
            service.register_root(w.folder.path()).await.unwrap();
            listed(&service).await;
            let file = w.folder.path().join("docs/f.txt");
            assert!(file.is_file());
            assert_eq!(config_of(&w).sync_root_source, "onedrive");
            assert_eq!((mode(&file), mode(&w.folder.path().join("docs"))), (0o444, 0o555));
            assert_eq!(service.root_state(), "ready");
            // A fresh OneDrive folder is excluded from KDE's
            // Baloo indexer, so reading a placeholder to index it does not
            // download the whole drive.
            let folder = std::fs::canonicalize(w.folder.path()).unwrap();
            assert_eq!(baloo_calls(&w), format!("config add excludeFolders {}\n", folder.display()));
            assert!(config_of(&w).sync_root_baloo_excluded);
            service.stop_sync().await;
        }

        /// A fresh OneDrive folder that is not already excluded
        /// from Baloo is excluded, and included again on Forget — the plain
        /// case, and the one the fake `balooctl6`'s empty `excluded` file
        /// gives by default.
        #[tokio::test]
        async fn baloo_excludes_a_fresh_onedrive_folder_and_includes_it_again_on_forget() {
            let w = world().await;
            let folder = std::fs::canonicalize(w.folder.path()).unwrap();
            let service = connected(&w, true).await;
            service.register_root(w.folder.path()).await.unwrap();
            listed(&service).await;
            assert_eq!(baloo_calls(&w), format!("config add excludeFolders {}\n", folder.display()));

            service.unregister_root().await.unwrap();
            assert_eq!(
                baloo_calls(&w),
                format!("config add excludeFolders {folder}\nconfig rm excludeFolders {folder}\n", folder = folder.display())
            );
        }

        /// Design §8.3 (test 7): a OneDrive folder remembers its account's
        /// drive — written once the first cycle has recorded it, and at the
        /// bring-up of a folder from before multiple accounts, which carries
        /// none — and, forgotten, it is refused `NotEmpty` to another account,
        /// while its own account may register it again.
        #[tokio::test]
        async fn a_onedrive_folder_remembers_its_drive_and_is_refused_to_another_account() {
            use std::os::unix::fs::PermissionsExt;
            let w = world().await;
            let drive = || xattr::get(w.folder.path(), konedrive_fs::placeholder::XATTR_DRIVE).unwrap();
            let service = connected(&w, true).await;
            service.register_root(w.folder.path()).await.unwrap();
            listed(&service).await;
            assert_eq!(drive().as_deref(), Some(&b"D1"[..]), "written with the drive the first cycle recorded");
            service.stop_sync().await;
            drop(service);

            // A folder from before carries no drive: its first bring-up writes it.
            let open = |mode| std::fs::set_permissions(w.folder.path(), std::fs::Permissions::from_mode(mode)).unwrap();
            open(0o755);
            xattr::remove(w.folder.path(), konedrive_fs::placeholder::XATTR_DRIVE).unwrap();
            open(0o555);
            {
                let restarted = connected(&w, true).await;
                restarted.restore().await;
                restarted.resume().await;
                assert_eq!(restarted.root_state(), "ready", "{}", restarted.last_error());
                assert_eq!(drive().as_deref(), Some(&b"D1"[..]));
                restarted.unregister_root().await.unwrap();
            }

            // The world's helper serves one connection at a time: each service
            // here goes before the next one connects.
            {
                let elsewhere = tempfile::tempdir().unwrap();
                let other = persist(&elsewhere.path().join("config.toml"));
                other.store.record_drive(&other.account, "D2").unwrap();
                let stranger = SyncService::new(Some(link(&w).await), Some(account(true)), Some(other));
                let refused = stranger.register_root(w.folder.path()).await;
                assert!(matches!(refused, Err(SyncError::ForeignFolder)), "{refused:?}");
            }

            let own = connected(&w, true).await;
            own.register_root(w.folder.path()).await.unwrap();
            own.stop_sync().await;
        }

        /// A folder the user has already excluded from Baloo —
        /// themselves, or through a parent directory — is never added again,
        /// and a later Forget must not remove an exclusion this daemon did
        /// not add.
        #[tokio::test]
        async fn baloo_leaves_a_folder_the_user_already_excluded_alone() {
            let w = world().await;
            let folder = std::fs::canonicalize(w.folder.path()).unwrap();
            mark_already_excluded(&w, &folder);
            let service = connected(&w, true).await;
            service.register_root(w.folder.path()).await.unwrap();
            listed(&service).await;
            assert_eq!(baloo_calls(&w), "", "already excluded, so nothing is added");
            assert!(!config_of(&w).sync_root_baloo_excluded);

            service.unregister_root().await.unwrap();
            assert_eq!(baloo_calls(&w), "", "we never added it, so Forget must not remove it");
        }

        /// Whether this daemon added the exclusion is persisted
        /// (`sync_root_baloo_excluded` in `config.toml`), so a restart
        /// between a registration and its Forget still gets the Forget
        /// right — the exclusion comes off, and it is not re-checked or
        /// re-added at the restart in between.
        #[tokio::test]
        async fn baloo_exclusion_survives_a_restart_and_is_still_removed_on_forget() {
            let w = world().await;
            let folder = std::fs::canonicalize(w.folder.path()).unwrap();
            {
                let first = connected(&w, true).await;
                first.register_root(w.folder.path()).await.unwrap();
                first.stop_sync().await;
            }
            let after_first = format!("config add excludeFolders {}\n", folder.display());
            assert_eq!(baloo_calls(&w), after_first);
            assert!(config_of(&w).sync_root_baloo_excluded);

            let second = connected(&w, false).await;
            second.restore().await;
            second.resume().await;
            assert_eq!(baloo_calls(&w), after_first, "not re-checked or re-added at a restart");
            assert!(config_of(&w).sync_root_baloo_excluded, "the flag survives the restart");

            second.unregister_root().await.unwrap();
            assert_eq!(baloo_calls(&w), format!("{after_first}config rm excludeFolders {}\n", folder.display()));
        }

        /// the exclusion used to be tried only by a
        /// fresh registration's commit. A registration kept after it failed
        /// (the helper could not confirm it let go) commits nothing, and when
        /// it was brought up later nothing asked Baloo again — the folder
        /// stayed indexed, and Baloo downloaded the whole drive. Every commit
        /// of a folder not recorded as excluded asks now.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_folder_kept_after_a_failed_registration_is_kept_out_of_baloo_when_brought_up() {
            let w = world().await;
            let folder = std::fs::canonicalize(w.folder.path()).unwrap();
            let sockets = tempfile::tempdir().unwrap();
            let socket_path = sockets.path().join("helper.sock");
            let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
            let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
            let service = service_with(&w, account(true), Some(link), Arc::new(StaticToken::new("T")));
            helper.refuse(Seen::RegisterRoot, libc::EIO);
            helper.refuse(Seen::UnregisterRoot, libc::EIO);
            service.register_root(w.folder.path()).await.unwrap_err();
            assert!(service.root().is_some(), "kept: the helper may still hold it");
            assert_eq!(baloo_calls(&w), "");

            helper.refuse(Seen::RegisterRoot, 0);
            service.resume().await;

            assert_eq!(service.root_state(), "ready", "{}", service.last_error());
            assert_eq!(baloo_calls(&w), format!("config add excludeFolders {}\n", folder.display()));
            assert!(config_of(&w).sync_root_baloo_excluded);
            service.stop_sync().await;
        }

        /// A `SyncService` that never had `set_baloo` called on it — as a
        /// test that forgot to, would be — starts with a `Baloo` that runs
        /// no program at all, so it never reaches the real `balooctl6` or
        /// `~/.config/baloofilerc`, on this host or the one running CI. This
        /// deliberately does not go through `service`/`service_with`, which
        /// always install the fake.
        #[tokio::test]
        async fn a_service_without_set_baloo_runs_no_program_on_registration() {
            let w = world().await;
            let account = account(true);
            let service = SyncService::new(Some(link(&w).await), Some(account), Some(persist(&w.config.path().join("config.toml"))));
            let drive = DriveClient::new(Url::parse(&format!("{}/", w.server.uri())).unwrap(), Arc::new(StaticToken::new("T")))
                .unwrap();
            service.set_drive(drive);
            service.set_sync_paths(SyncPaths {
                tree_db: w.config.path().join("tree.sqlite"),
                rescue_dir: w.config.path().join("rescued"),
                thumbnails: Some(w.config.path().join("thumbnails")),
            });
            service.set_helper_socket(w.config.path().join("no-helper.sock"));
            // No `set_baloo`: the default `Baloo::disabled()` stands.

            service.register_root(w.folder.path()).await.unwrap();
            listed(&service).await;

            assert!(!w.baloo.path().join("calls").exists(), "the fake was never even pointed to");
            assert!(!config_of(&w).sync_root_baloo_excluded, "nothing ran, so nothing was excluded");
            service.stop_sync().await;
        }

        /// A folder registered signed out is local, as in part 1 — and, since
        /// HS2, so is every folder registered without interception, signed
        /// in or not: that is the developer's mode, filled from a directory.
        #[tokio::test]
        async fn a_folder_registered_while_signed_out_or_without_interception_is_local() {
            for signed_in in [false, true] {
                let w = world().await;
                let service = service(&w, signed_in);
                service.register_root_without_interception(w.folder.path()).await.unwrap();
                assert_eq!(config_of(&w).sync_root_source, "local", "signed in: {signed_in}");
                let source = tempfile::tempdir().unwrap();
                std::fs::write(source.path().join("a.txt"), b"abc").unwrap();
                assert_eq!(service.populate_from_directory(source.path()).await.unwrap(), 1);
                assert_eq!(mode(&w.folder.path().join("a.txt")), 0o644, "no lock on a local folder");
                assert_eq!(requests(&w).await, 0, "a local folder never asks OneDrive");
            }
        }

        #[tokio::test]
        async fn populating_a_onedrive_folder_from_a_directory_is_refused() {
            let w = world().await;
            let service = connected(&w, true).await;
            service.register_root(w.folder.path()).await.unwrap();
            let source = tempfile::tempdir().unwrap();
            let err = service.populate_from_directory(source.path()).await.unwrap_err();
            assert!(matches!(err, SyncError::Unsupported(_)), "{err:?}");
            service.stop_sync().await;
        }

        #[tokio::test]
        async fn forgetting_a_onedrive_folder_stops_its_sync_unlocks_it_and_drops_its_tree() {
            let w = world().await;
            let service = connected(&w, true).await;
            service.register_root(w.folder.path()).await.unwrap();
            listed(&service).await;
            // A reader of the test's own — another program reading the store,
            // `sqlite3` say — so that the daemon's connection is not the last
            // one: SQLite then leaves its journal files when that closes, and
            // only the Forget itself removes them.
            let reader = rusqlite::Connection::open(w.config.path().join("tree.sqlite")).unwrap();
            reader.query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get::<_, i64>(0)).unwrap();
            for name in ["tree.sqlite-wal", "tree.sqlite-shm"] {
                assert!(w.config.path().join(name).exists(), "no {name} to remove");
            }
            service.unregister_root().await.unwrap();
            let file = w.folder.path().join("docs/f.txt");
            assert!(file.is_file(), "the files stay (spec §3.1)");
            assert_eq!((mode(&file), mode(&w.folder.path().join("docs"))), (0o644, 0o755));
            for name in ["tree.sqlite", "tree.sqlite-wal", "tree.sqlite-shm"] {
                assert!(!w.config.path().join(name).exists(), "{name} was left");
            }
            drop(reader);
            assert_eq!(service.items(), (0, 0, 0));
            assert_eq!(service.root_state(), "none");
            assert_eq!(config_of(&w).sync_root_source, "local");
            let before = requests(&w).await;
            service.refresh_now();
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(requests(&w).await, before, "nothing syncs any more");
        }

        /// A restart brings a OneDrive folder back syncing, and its first
        /// cycle reconciles the whole folder: the stored link has
        /// no changes since, so only a Full reconcile puts back the file
        /// removed while the daemon was down.
        ///
        /// The restarted daemon reads "signed out" (its Graph token here is
        /// static, so the cycle still succeeds): a restored folder keeps the
        /// source `config.toml` records, and a restart after a sign-out must
        /// not turn a OneDrive folder into a local one.
        #[tokio::test]
        async fn a_restart_brings_a_onedrive_folder_back_and_repairs_it() {
            let w = world().await;
            {
                let first = connected(&w, true).await;
                first.register_root(w.folder.path()).await.unwrap();
                listed(&first).await;
                first.stop_sync().await;
            }
            assert_eq!(config_of(&w).sync_root_source, "onedrive");
            std::process::Command::new("chmod").args(["-R", "u+w"]).arg(w.folder.path()).status().unwrap();
            std::fs::remove_file(w.folder.path().join("docs/f.txt")).unwrap();
            let listings = full_listings(&w).await;

            let second = connected(&w, false).await;
            second.restore().await;
            second.resume().await;
            wait_until("repaired by the first cycle's Full reconcile", || {
                w.folder.path().join("docs/f.txt").is_file()
            })
            .await;
            assert_eq!(full_listings(&w).await, listings, "asked from the stored link, not listed again");
            assert_eq!(config_of(&w).sync_root_source, "onedrive");
            second.stop_sync().await;
        }

        #[tokio::test]
        async fn refresh_runs_a_cycle_now() {
            let w = world().await;
            let service = connected(&w, true).await;
            service.register_root(w.folder.path()).await.unwrap();
            listed(&service).await;
            let before = deltas(&w).await;
            service.refresh().await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(deltas(&w).await, before + 1);
            service.stop_sync().await;
        }

        #[tokio::test]
        async fn refresh_on_a_local_folder_is_refused() {
            let w = world().await;
            let service = service(&w, false);
            service.register_root_without_interception(w.folder.path()).await.unwrap();
            assert!(matches!(service.refresh().await, Err(SyncError::Unsupported(_))));
        }

        #[tokio::test]
        async fn skipped_names_what_is_not_in_the_folder_by_its_full_path() {
            let w = world().await;
            Mock::given(method("GET")).and(path("/me/drive/root/delta"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "value": [
                        {"id": "R", "root": {}, "folder": {}},
                        {"id": "D", "name": "docs", "folder": {}, "parentReference": {"id": "R"}},
                        {"id": "F", "name": "f.txt", "size": 3, "cTag": "c1", "file": {}, "parentReference": {"id": "D"}},
                        {"id": "V", "name": "Personal Vault", "folder": {}, "specialFolder": {"name": "vault"}, "parentReference": {"id": "R"}}
                    ],
                    "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L1", w.server.uri())
                })))
                .with_priority(4)
                .mount(&w.server).await;
            let service = connected(&w, true).await;
            assert_eq!(service.skipped().await.unwrap(), Vec::<(String, String)>::new());
            service.register_root(w.folder.path()).await.unwrap();
            wait_until("listed", || service.items() == (3, 2, 1)).await;
            let vault = std::fs::canonicalize(w.folder.path()).unwrap().join("Personal Vault");
            assert_eq!(
                service.skipped().await.unwrap(),
                vec![(vault.display().to_string(), "personal-vault".to_owned())]
            );
            service.stop_sync().await;
        }

        /// A folder that reads "signed out" is brought up to date the moment
        /// the account signs in again, not up to a poll interval later (an
        /// hour here).
        #[tokio::test]
        async fn signing_in_brings_a_folder_that_reads_signed_out_up_to_date_at_once() {
            let w = world().await;
            let account = account(true);
            let tokens = Arc::new(AccountTokens { account: account.clone(), refused: AtomicUsize::new(0) });
            let service = service_with(&w, account.clone(), Some(link(&w).await), Arc::clone(&tokens) as Arc<dyn TokenSource>);
            service.register_root(w.folder.path()).await.unwrap();
            listed(&service).await;

            account.update(|s| s.state = SignInState::SignedOut);
            service.refresh_now();
            wait_until("the folder reads signed out", || service.root_state() == "error").await;
            assert!(service.last_error().contains("signed out"), "{}", service.last_error());
            // The one retry the schedule has, and then the hour-long wait.
            wait_until("the retry failed too", || tokens.refused.load(Ordering::SeqCst) >= 2).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(tokens.refused.load(Ordering::SeqCst), 2, "the poller waits out its interval now");

            let before = deltas(&w).await;
            account.update(|s| s.state = SignInState::SigningIn);
            account.update(|s| s.state = SignInState::SignedIn);
            wait_until("the folder is in step again", || service.root_state() == "ready").await;
            assert_eq!(deltas(&w).await, before + 1);
            service.stop_sync().await;
        }

        /// A Forget the helper refuses keeps the folder registered — and so
        /// locked, and kept in step.
        #[tokio::test]
        async fn a_forget_the_helper_refuses_leaves_the_folder_locked_and_in_step() {
            let w = world().await;
            let sockets = tempfile::tempdir().unwrap();
            let socket_path = sockets.path().join("helper.sock");
            let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
            let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
            let service = service_with(&w, account(true), Some(link), Arc::new(StaticToken::new("T")));
            service.register_root(w.folder.path()).await.unwrap();
            listed(&service).await;
            helper.refuse(Seen::UnregisterRoot, libc::EIO);

            let refused = service.unregister_root().await;

            assert!(matches!(refused, Err(SyncError::Io(_))), "{refused:?}");
            assert_eq!(mode(&w.folder.path().join("docs/f.txt")), 0o444);
            assert!(w.config.path().join("tree.sqlite").exists());
            assert_eq!(config_of(&w).sync_root_source, "onedrive");
            let before = deltas(&w).await;
            service.refresh().await.unwrap();
            wait_for_deltas(&w, before).await;
            service.stop_sync().await;
        }

        /// A listing's reconcile takes the very lock registrations and
        /// Forgets take: while that is held, the listing waits. (A replacement
        /// of a changed file does not take it — it swaps in one file under its
        /// inode lock — so this is about the listing, not every change.)
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn the_listing_waits_for_the_services_lifecycle_lock() {
            let w = world().await;
            // The first listing answers late enough for the lock to be taken first.
            Mock::given(method("GET")).and(path("/me/drive/root/delta"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "value": [
                        {"id": "R", "root": {}, "folder": {}},
                        {"id": "D", "name": "docs", "folder": {}, "parentReference": {"id": "R"}},
                        {"id": "F", "name": "f.txt", "size": 3, "cTag": "c1", "file": {}, "parentReference": {"id": "D"}}
                    ],
                    "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L1", w.server.uri())
                })).set_delay(Duration::from_millis(300)))
                .with_priority(4)
                .mount(&w.server).await;
            let service = connected(&w, true).await;
            service.register_root(w.folder.path()).await.unwrap();

            let held = service.lifecycle.write().await;
            wait_for_deltas(&w, 0).await;
            tokio::time::sleep(Duration::from_millis(700)).await;
            assert!(!w.folder.path().join("docs").exists(), "the folder was changed under the lock");
            drop(held);
            listed(&service).await;
            service.stop_sync().await;
        }

        /// A Forget stops the sync before it waits for the lifecycle lock, so
        /// that no reconcile keeps it waiting; a helper's reconnect that takes
        /// the lock first may start the sync again in between. That one is
        /// stopped too, before the folder is let go.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_sync_started_again_while_a_forget_waits_is_stopped_too() {
            let w = world().await;
            let service = connected(&w, true).await;
            service.register_root(w.folder.path()).await.unwrap();
            listed(&service).await;

            let held = service.lifecycle.write().await;
            let forgetting = {
                let service = Arc::clone(&service);
                tokio::spawn(async move { service.unregister_root().await })
            };
            wait_until("the Forget stopped the sync", || service.syncing.lock().unwrap().is_none()).await;
            // What a `resume` that has the lock does to a OneDrive folder.
            service.start_sync().await;
            drop(held);
            forgetting.await.unwrap().unwrap();

            let before = requests(&w).await;
            service.refresh_now();
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(requests(&w).await, before, "a sync runs on a forgotten folder");
            assert_eq!(mode(&w.folder.path().join("docs/f.txt")), 0o644);
        }

        /// `start_sync` waits for the tree store to open before it keeps the
        /// sync it starts. Two of them at once — which only the lifecycle lock
        /// its callers hold keeps from happening — must still leave one sync
        /// running, not a second one that nothing could ever stop.
        #[tokio::test]
        async fn two_starts_at_once_leave_one_sync() {
            let w = world().await;
            let service = connected(&w, true).await;
            service.register_root(w.folder.path()).await.unwrap();
            listed(&service).await;
            service.stop_sync().await;
            let before = deltas(&w).await;

            tokio::join!(service.start_sync(), service.start_sync());
            tokio::time::sleep(Duration::from_millis(300)).await;

            assert_eq!(deltas(&w).await, before + 1, "two syncs ran their first cycle");
            service.stop_sync().await;
        }

        /// A folder whose sync could not start (F18: its tree store could not
        /// be opened) is not reported as refreshed: `Refresh()` tries to start
        /// it again, says why when it still cannot, and starts it once it can.
        #[tokio::test]
        async fn refresh_starts_a_sync_that_could_not_start_or_says_why() {
            let w = world().await;
            let service = connected(&w, true).await;
            // A file where the tree store's directory has to be.
            let blocker = w.config.path().join("state");
            std::fs::write(&blocker, b"").unwrap();
            service.set_sync_paths(SyncPaths {
                tree_db: blocker.join("tree.sqlite"),
                rescue_dir: w.config.path().join("rescued"),
                thumbnails: Some(w.config.path().join("thumbnails")),
            });
            service.register_root(w.folder.path()).await.unwrap();
            assert_eq!(service.root_state(), "error");

            let refused = service.refresh().await;
            assert!(
                matches!(&refused, Err(SyncError::Io(why)) if why.contains("the tree store cannot be opened")),
                "{refused:?}"
            );
            assert_eq!(requests(&w).await, 0, "nothing synced");

            std::fs::remove_file(&blocker).unwrap();
            service.refresh().await.unwrap();
            listed(&service).await;
            assert_eq!(service.root_state(), "ready");
            service.stop_sync().await;
        }

        /// A folder held at startup until its helper is back has not been
        /// brought up — nor recovered — yet: `Refresh()` says so rather than
        /// start its sync ahead of that.
        #[tokio::test]
        async fn refresh_of_a_folder_waiting_for_its_helper_says_so() {
            let w = world().await;
            let sockets = tempfile::tempdir().unwrap();
            let socket_path = sockets.path().join("helper.sock");
            let _helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
            {
                let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
                let first = service_with(&w, account(true), Some(link), Arc::new(StaticToken::new("T")));
                first.register_root(w.folder.path()).await.unwrap();
                listed(&first).await;
                first.stop_sync().await;
            }
            let restarted = service(&w, true);
            restarted.restore().await;
            let before = requests(&w).await;

            let refused = restarted.refresh().await;

            assert!(matches!(refused, Err(SyncError::NoHelper)), "{refused:?}");
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(requests(&w).await, before, "a sync started ahead of the bring-up");
            restarted.stop_sync().await;
        }

        /// A OneDrive folder is locked read-only after its first listing (W2),
        /// the folder itself too, and bringing it up again after a restart
        /// re-checked it with a write probe — refused, so no locked folder came
        /// back after a restart, in either mode: "cannot bring up the sync
        /// folder: Permission denied". Found by, whose switch to
        /// interception goes through the same check. A folder that already
        /// carries its root id was probed when it was first registered, and is
        /// not probed again — the helper's own re-registration skips its probe
        /// for the same reason. The mode without interception is
        /// a folder recorded that way before HS2 (`legacy_without_interception`):
        /// no new OneDrive folder is made so.
        #[tokio::test]
        async fn a_locked_onedrive_folder_comes_back_after_a_restart_in_either_mode() {
            for intercepted in [false, true] {
                let w = world().await;
                {
                    let first = connected(&w, true).await;
                    first.register_root(w.folder.path()).await.unwrap();
                    listed(&first).await;
                    first.stop_sync().await;
                    first.set_link(None);
                }
                if !intercepted {
                    legacy_without_interception(&w);
                }
                assert_eq!(mode(w.folder.path()), 0o555, "the folder itself is locked");

                let restarted = connected(&w, true).await;
                restarted.restore().await;
                restarted.resume().await;

                assert!(
                    !restarted.last_error().contains("cannot bring up"),
                    "intercepted = {intercepted}: {}",
                    restarted.last_error()
                );
                assert_eq!(restarted.root_state(), "ready", "{}", restarted.last_error());
                assert_eq!(mode(w.folder.path()), 0o555, "and it stays locked");
                restarted.stop_sync().await;
            }
        }

        /// Rewrites `config.toml` as a daemon from before HS2 left a folder
        /// that shows OneDrive registered without interception on purpose —
        /// with a helper connected, so not one to switch.
        fn legacy_without_interception(w: &World) {
            let persist = persist(&w.config.path().join("config.toml"));
            persist
                .store
                .update_account(&persist.account, |account| {
                    let root = account.root.as_mut().expect("a folder");
                    root.intercepted = false;
                    root.upgrade_when_helper = Some(false);
                    Ok::<_, crate::config::ConfigError>(())
                })
                .unwrap();
        }

        /// HS2: a folder that shows OneDrive and is not intercepted — as a
        /// daemon from before HS left one registered on purpose, with a helper
        /// connected — is not kept in step while there is no helper: OneDrive
        /// is not asked, `Refresh()` is refused `NoHelper`, and the folder
        /// reads `error` with the helper's advice first in `LastError`. When
        /// the helper connects it switches to interception whatever it was
        /// registered as (switch; there is no "on purpose" for a
        /// OneDrive folder any more). And, Ruling 1: the switch keeps
        /// invariant M1 for everything its sync places afterwards — the sync
        /// starts intercepted, so a folder that arrives from the drive later is
        /// marked before it is filled.
        #[tokio::test]
        async fn a_onedrive_folder_without_interception_waits_for_the_helper_then_switches() {
            let w = world().await;
            {
                let first = connected(&w, true).await;
                first.register_root(w.folder.path()).await.unwrap();
                listed(&first).await;
                first.stop_sync().await;
            }
            legacy_without_interception(&w);

            // From now on the drive holds a new folder, `new/g.txt`.
            w.server.reset().await;
            Mock::given(method("GET")).and(path("/me/drive"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D1"})))
                .mount(&w.server).await;
            Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L1"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "value": [
                        {"id": "N", "name": "new", "folder": {}, "parentReference": {"id": "R"}},
                        {"id": "G", "name": "g.txt", "size": 3, "cTag": "c1", "file": {}, "parentReference": {"id": "N"}}
                    ],
                    "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L2", w.server.uri())
                })))
                .mount(&w.server).await;
            Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L2"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": [], "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L2", w.server.uri())})))
                .mount(&w.server).await;

            // A restart with no helper.
            let service = service(&w, true);
            service.restore().await;
            service.resume().await;
            assert_eq!(service.root_state(), "error");
            assert!(service.last_error().starts_with("the konedrive helper is not connected"), "{}", service.last_error());
            assert!(matches!(service.refresh().await, Err(SyncError::NoHelper)));
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(deltas(&w).await, 0, "OneDrive was asked with no helper");

            // The helper starts, and connects.
            w.helper.forget();
            service.set_link(Some(link(&w).await));
            service.resume().await;

            wait_until("the new folder was placed", || w.folder.path().join("new/g.txt").exists()).await;
            let seen = w.helper.seen();
            assert_eq!(seen.first(), Some(&Seen::RegisterRoot), "{seen:?}: {}", service.last_error());
            let marks: Vec<_> = seen.iter().filter(|s| matches!(s, Seen::MarkDir { .. })).collect();
            assert!(!marks.is_empty(), "the sync placed a directory after the switch without marking it: {seen:?}");
            assert!(
                marks.iter().all(|s| matches!(s, Seen::MarkDir { entries: 0 })),
                "a directory was filled before it was marked: {seen:?}"
            );
            assert_eq!(service.root_state(), "ready", "{}", service.last_error());
            service.stop_sync().await;
        }

        /// `Skipped()` reads the tree store under the lifecycle lock, so a
        /// Forget — which removes the store with that lock held for writing —
        /// waits for a read under way instead of removing the files under it.
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn skipped_reads_the_tree_under_the_lifecycle_lock() {
            let w = world().await;
            let service = connected(&w, true).await;
            service.register_root(w.folder.path()).await.unwrap();
            listed(&service).await;

            let held = service.lifecycle.write().await;
            let reading = {
                let service = Arc::clone(&service);
                tokio::spawn(async move { service.skipped().await })
            };
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(!reading.is_finished(), "Skipped() read the tree while the lock was held for writing");
            drop(held);
            assert_eq!(reading.await.unwrap().unwrap(), Vec::<(String, String)>::new());
            service.stop_sync().await;
        }
    }
}
