//! Everything this sub-project adds to the daemon: the helper link, the
//! content source, the hydration loop, and `SyncService` — the `org.konedrive.Sync1`
//! D-Bus surface's own half of the work (`dbus.rs` is the thin zbus wrapper
//! around it, the same split `crate::account`/`crate::dbus` uses for
//! `Account1`).

pub mod dbus;
pub mod helper;
pub mod root;
pub mod source;

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::future::Future;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::FutureExt;
use helper::{Clearance, HelperError, HelperLink, HydrateRequest, NotCleared};
use konedrive_fs::placeholder::{read_stamp, read_state, stamp_matches, State, StateError, XATTR_STATE};
use root::{DehydrateError, RecoveryError, RecoveryReport, RegisterError, SyncRoot};
use source::{ContentSource, Fetched, FillError, LocalDir, SourceError};
use tokio::sync::watch;

use crate::config::Config;
use crate::state::{SignInState, StateHandle};

/// Answers hydration requests until the helper goes away. At most four run at
/// once; everything else waits, and no request is ever dropped silently.
///
/// The permit is acquired *before* spawning (Ruling H29), not inside the
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
/// own and waits on [`HelperLink::closed`] instead (Ruling H141).
///
/// # Per-inode serialization (Task 11, Ruling H101)
///
/// Spec §8 promises "the daemon serializes operations per inode, so a
/// hydration request for a file being dehydrated runs after the dehydration
/// finishes", but nothing enforced that: `grep` finds no such lock anywhere
/// in this crate before this task, because nothing before it ever ran
/// `serve_hydrations` and `root::dehydrate` at once — `main.rs` called
/// neither. Task 11 is what wires both into the same running daemon (see
/// [`SyncService::dehydrate`], which shares the same `locks` table), so this
/// is the first point at which two fills of the *same* file — one a
/// hydration, one a dehydration's punch — could run concurrently and tear
/// it.
///
/// `locks` is keyed by `(st_dev, st_ino)` read from the descriptor itself
/// (Ruling H101), which is what §8 means by "per inode" and the only key the
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
    mut requests: tokio::sync::mpsc::Receiver<HydrateRequest>,
    source: Arc<dyn ContentSource>,
    locks: InodeLocks,
) {
    let permits = Arc::new(tokio::sync::Semaphore::new(4));
    let mut running = tokio::task::JoinSet::new();
    while let Some(HydrateRequest { req_id, fd }) = requests.recv().await {
        // Reap whatever finished while we were waiting; the set must not
        // accumulate the results of completed fills for the life of the
        // daemon.
        while running.try_join_next().is_some() {}
        // A request that arrives, or reaches the front, after its
        // connection ended is not filled (Ruling H141): the helper answered
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
        let source = Arc::clone(&source);
        let locks = locks.clone();
        running.spawn(async move {
            let _permit = permit;
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
            let _inode_guard = match key {
                Some(key) => Some(locks.lock(key).await),
                None => None,
            };
            // Ruling H52: a panic anywhere in the fill — including inside a
            // `ContentSource` we did not write — must not become an
            // unanswerable event in the kernel. Unwinding out of here would
            // close the event fd and produce no errno at all, so
            // `hydrate_done` would never be called and the suspended
            // `open()` would wait forever: §5.2's 30 s bound covers only "the
            // owner's daemon is not connected", and this daemon is connected.
            // Degrading it to an `EIO` denial costs the user one failed open.
            //
            // What the request finds under the lock decides what it does
            // (Ruling H137): a file filled while the request waited is
            // answered as it is — see `source::answer_request`.
            let filled = AssertUnwindSafe(source::answer_request(fd, source.as_ref(), Some(&link)))
                .catch_unwind()
                .await;
            let errno = match filled {
                Ok(errno) => errno,
                Err(_) => {
                    tracing::error!(
                        "the hydration of request {req_id} panicked; denying that open with EIO \
                         rather than leaving it suspended forever"
                    );
                    libc::EIO
                }
            };
            if let Err(e) = link.hydrate_done(req_id, errno).await {
                tracing::error!("cannot report hydration {req_id}: {e}");
            }
        });
    }
    while running.join_next().await.is_some() {}
}

/// A file's identity, the way spec §8 means "per inode": the `(st_dev,
/// st_ino)` pair, read from an open descriptor and never spelled as a name
/// (Ruling H101). Two links to one inode share a key; a rename changes no
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
    /// it takes the lock (Ruling H102/H103).
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

/// Serializes hydration and dehydration of the same inode: spec §8's promise
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
    /// (Ruling H147): a fill or a free-up of the same file is running in this
    /// daemon, and waiting for it would hold the whole reconnect behind a
    /// download (the very thing Ruling H141 took away), while the file it is
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

/// What `RootState` (spec §3.1) reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootState {
    /// No root is registered.
    None,
    /// A root is registered and, as far as this daemon knows, healthy.
    Ready,
    /// A root is registered, but **nothing intercepts opens inside it**
    /// (Ruling H105): it was registered through
    /// `RegisterRootWithoutInterception`, so a placeholder nobody fills
    /// reads as zeros until it is hydrated by hand. Distinct from `ready`
    /// precisely because a client must be able to tell the two apart.
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
}

impl Default for SyncSnapshot {
    fn default() -> Self {
        Self { root_path: String::new(), root_state: RootState::None, last_error: String::new() }
    }
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
    #[error("a sync root is already registered; forget it first")]
    AlreadyRegistered,
    #[error("nobody is signed in")]
    NotSignedIn,
    #[error("no content source is registered; call PopulateFromDirectory first")]
    NoSource,
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

/// The sync folder: registration, the manual `PopulateFromDirectory` fill,
/// and per-file hydrate/dehydrate/state — everything `org.konedrive.Sync1`
/// exposes.
///
/// # Why `hydrate_now` fills directly rather than only through interception
///
/// The brief this was built from describes a helper-connected `Hydrate()`
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
    /// Replaceable, because the helper can go away and come back (Ruling
    /// H107): `supervise_helper` swaps it for `None` the moment the
    /// connection drops and back to a live link when it reconnects.
    link: Mutex<Option<HelperLink>>,
    /// The account interface's own state, on the same object path. §3.1
    /// refuses `RegisterRoot` when nobody is signed in (Ruling H110), and
    /// this is what it asks. `None` only where nothing wired it up.
    account: Option<StateHandle>,
    /// Where the registered root is persisted, so it survives a restart
    /// (§3.1, Ruling H106). `None` disables persistence entirely.
    config_file: Option<PathBuf>,
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
    lifecycle: tokio::sync::RwLock<()>,
    source: Mutex<Option<Arc<dyn ContentSource>>>,
    locks: InodeLocks,
    /// Where the helper's socket is, for Ruling H146's local rule: with no
    /// link, a punch first looks there to see whether a helper — and so a
    /// fanotify group that could hold a mark — exists at all. Set by
    /// [`supervise_helper`] to the path it connects to.
    helper_socket: Mutex<PathBuf>,
}

/// A registered root and how — or whether — opens inside it are intercepted.
#[derive(Clone)]
struct Registration {
    root: SyncRoot,
    /// False only for a root registered through
    /// `RegisterRootWithoutInterception` (Ruling H105).
    intercepted: bool,
    /// Whether its last recovery left interrupted files as found because a
    /// helper was running that this daemon had no link to (Ruling H146), so
    /// the next link runs it again ([`SyncService::resume`]).
    recovery_deferred: bool,
}

/// A root as `config.toml` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Persisted {
    path: PathBuf,
    /// Empty in a config written before the id was recorded.
    root_id: String,
    intercepted: bool,
}

impl Persisted {
    fn of(root: &SyncRoot, intercepted: bool) -> Self {
        Self { path: root.path.clone(), root_id: root.root_id.clone(), intercepted }
    }
}

/// What `LastError` says while a root is registered without interception.
/// Spelled out rather than hinted at: this mode's whole risk is that a file
/// looks present and reads as zeros, so the one thing a user must not have
/// to infer is that they are in it.
pub const NO_INTERCEPTION_WARNING: &str =
    "this folder is registered WITHOUT interception: nothing fills a placeholder when it is \
     opened, so files in this folder read as zeros until they are explicitly hydrated";

/// What `LastError` says while the helper is gone and a root needs it
/// (Ruling H107). The published state must not keep saying `ready` while the
/// sync folder is, in the only sense that matters, dead.
pub const HELPER_LOST_WARNING: &str =
    "the konedrive helper is not connected: opens inside the sync folder are not intercepted, \
     so files that are not downloaded read as zeros until it comes back";

impl SyncService {
    /// `account` gates `RegisterRoot` on somebody being signed in (§3.1);
    /// `config_file` is where the registered root is persisted so it
    /// survives a restart. Both are `None` in tests that exercise neither.
    pub fn new(
        link: Option<HelperLink>,
        account: Option<StateHandle>,
        config_file: Option<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self {
            link: Mutex::new(link),
            account,
            config_file,
            state: SyncStateHandle::new(SyncSnapshot::default()),
            root: Mutex::new(None),
            lifecycle: tokio::sync::RwLock::new(()),
            source: Mutex::new(None),
            locks: InodeLocks::new(),
            helper_socket: Mutex::new(PathBuf::from(konedrive_proto::SOCKET_PATH)),
        })
    }

    /// Where the helper's socket is (see `helper_socket`). Defaults to
    /// `konedrive_proto::SOCKET_PATH`.
    pub fn set_helper_socket(&self, path: impl Into<PathBuf>) {
        *self.helper_socket.lock().unwrap() = path.into();
    }

    /// What a punch goes by when nothing ties it to a link of its own
    /// (Ruling H146's local rule, on [`Clearance`]): the live link if there
    /// is one, the helper's socket if not.
    fn clearance(&self) -> Clearance {
        match self.link() {
            Some(link) => Clearance::Link(link),
            None => Clearance::NoLink(self.helper_socket.lock().unwrap().clone()),
        }
    }

    pub fn state(&self) -> &SyncStateHandle {
        &self.state
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

    /// Publishes a new helper link, or its loss (Ruling H107).
    pub fn set_link(&self, link: Option<HelperLink>) {
        *self.link.lock().unwrap() = link;
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

    pub fn root_state(&self) -> String {
        self.state.get().root_state.as_str().to_owned()
    }

    pub fn last_error(&self) -> String {
        self.state.get().last_error
    }

    /// Binds an empty (or previously-registered, Ruling H78) folder to the
    /// account, then runs startup recovery on it (Ruling H80: recovery
    /// always runs *after* registration, on the same live helper link, so a
    /// file left `dehydrating` mid-`ClearIgnore` can still be cleaned up).
    ///
    /// Refused before anything is touched when nobody is signed in, or when
    /// a root is already registered (§3.1, Ruling H110). The second of those
    /// used to be accepted: a second `register_root` returned `Ok(())` and
    /// silently replaced the root, leaving the first one registered with the
    /// helper — still marked, still walked — while `ItemState` started
    /// calling its files `not-managed`.
    ///
    /// Refused without a helper, too (Ruling H105): no helper means no
    /// interception, and a placeholder nobody intercepts reads as zeros.
    /// [`register_root_without_interception`](Self::register_root_without_interception)
    /// is the explicit way to ask for that anyway.
    pub async fn register_root(&self, path: &Path) -> Result<(), SyncError> {
        let _lifecycle = self.lifecycle.write().await;
        self.restore_locked().await;
        self.require_sign_in()?;
        self.check_no_root_yet()?;
        self.require_link()?;
        self.bind(path, true, true).await
    }

    /// `RegisterRoot` for a machine with no privileged helper (Ruling H105):
    /// the same folder checks, the same root id, the same placeholders, and
    /// nothing intercepting anything.
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
    /// drive". This method exists for the case where there is no drive and
    /// no helper: a standing project ruling keeps the privileged helper
    /// inside a VM, and the folder is driven from a local directory
    /// (`PopulateFromDirectory`) rather than from OneDrive. Requiring a
    /// Microsoft sign-in here would put the one path that works without the
    /// cloud behind the cloud, which is the whole thing Ruling H105 set out
    /// to unblock. Nothing in this mode touches the account: the content
    /// comes from a directory the caller names.
    pub async fn register_root_without_interception(
        &self,
        path: &Path,
    ) -> Result<(), SyncError> {
        let _lifecycle = self.lifecycle.write().await;
        self.restore_locked().await;
        self.check_no_root_yet()?;
        self.bind(path, false, true).await
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

    /// Registers `path`, recovers it, and publishes the result — the half
    /// shared by a `RegisterRoot` call, a `RegisterRootWithoutInterception`
    /// call, and a root brought back up at startup or after the helper
    /// reconnected. `fresh` is true for the first two: a registration the
    /// daemon did not hold before this call.
    ///
    /// # Ruling H110: nothing is committed until nothing can still fail
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
    /// be fixed: a refused `ClearIgnore` leaves a file in the state spec §8
    /// calls silently unrecoverable, so it must not stay silent here.
    ///
    /// # Whatever the helper holds, the daemon holds (Ruling H135, link 2)
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
    /// helper (Rulings H105, H134, H135): not registered, not marked, not
    /// unregistered. What its recovery may still ask is `ClearIgnore`, by the
    /// same local rule every punch follows (Ruling H146).
    async fn bind(&self, path: &Path, intercepted: bool, fresh: bool) -> Result<(), SyncError> {
        if !intercepted {
            let root = root::register_root_unprotected(path).await?;
            let recovery = root::recover(&self.clearance(), &root, &self.locks).await;
            let report = self.recovered(&root, recovery)?;
            self.commit(root, false, report);
            return Ok(());
        }

        let link = self.require_link()?;
        let (dir, root) = root::prepare(path).await?;
        let previous = if fresh {
            let previous = self.persisted_root();
            self.save_root(Some(&Persisted::of(&root, true))).map_err(SyncError::Io)?;
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
                self.commit(root, true, report);
                Ok(())
            }
            Err(error) => {
                if let Some(previous) = previous {
                    self.abandon(&link, root, previous, &error).await;
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
    /// recovered.
    fn commit(&self, root: SyncRoot, intercepted: bool, report: RecoveryReport) {
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
            // Not an error (the final review's m11): what has such a file
            // open is, as often as not, the very open that will fill it.
            trouble = Some(format!(
                "startup recovery left {} interrupted file(s) as they were because they were in \
                 use; each is filled when it is next opened, or reset at the next start",
                report.busy
            ));
            tracing::info!("{}", trouble.as_deref().unwrap_or_default());
        } else if report.deferred > 0 {
            // Not an error either (Ruling H146): recovery runs again the
            // moment the link is up.
            trouble = Some(format!(
                "startup recovery left {} interrupted file(s) as they were: a konedrive helper \
                 is running and this daemon is not connected to it yet; they are reset once it is",
                report.deferred
            ));
            tracing::info!("{}", trouble.as_deref().unwrap_or_default());
        }

        // `config.toml` names what is registered now: a fresh root without
        // interception is written down here (a fresh intercepted one already
        // was, before the helper heard of it), and a root brought back up
        // under an id other than the recorded one is corrected.
        self.remember(&Persisted::of(&root, intercepted));
        let path = root.path.display().to_string();
        let recovery_deferred = report.deferred > 0;
        *self.root.lock().unwrap() = Some(Registration { root, intercepted, recovery_deferred });
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
        });
    }

    /// Undoes a fresh intercepted registration that failed after the helper
    /// may have saved it: the helper is told to let go, and `config.toml` is
    /// put back the way it was (Ruling H110, on both sides).
    ///
    /// If the helper cannot confirm it let go — the link dropped, the call
    /// timed out, anything but an answer — the root is kept instead, as
    /// intercepted and published as an error. A folder the helper may still
    /// hold must never be one the daemon holds nothing of: the next thing it
    /// would accept for that folder is a registration without interception.
    /// It leaves the way every intercepted root leaves, through the helper
    /// (Ruling H133), and a retry is answered "already registered" until it
    /// has.
    ///
    /// `EPERM` counts as having let go: the helper answers it when it holds
    /// no root of this uid under that id, which is what a registration it
    /// refused leaves behind.
    async fn abandon(
        &self,
        link: &HelperLink,
        root: SyncRoot,
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
                *self.root.lock().unwrap() =
                    Some(Registration { root, intercepted: true, recovery_deferred: false });
                self.state.update(|s| {
                    s.root_path = path;
                    s.root_state = RootState::Error;
                    s.last_error = message;
                });
            }
        }
    }

    /// Forgets the root and clears the published state. The files themselves
    /// are left exactly as they are (spec §3.1).
    ///
    /// # Ruling H133: an intercepted root is forgotten through the helper, or not at all
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
    /// # Ruling H134: a root registered without interception never involves the helper
    ///
    /// It was never announced to the helper, so there is nothing to tell it.
    /// Telling it anyway made such a root impossible to forget while a helper
    /// was connected: the helper refuses `EPERM` to unregister a root the uid
    /// does not hold, and the daemon kept the registration (measured).
    pub async fn unregister_root(&self) -> Result<(), SyncError> {
        let _lifecycle = self.lifecycle.write().await;
        self.restore_locked().await;
        let reg = self.require_registration()?;
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
        self.state.update(|s| {
            s.root_path.clear();
            s.root_state = RootState::None;
            s.last_error.clear();
        });
        Ok(())
    }

    /// §3.1: the root is "persisted, so it survives a restart" (Ruling
    /// H106) — with its mode, and with the id the helper holds it by.
    /// Read-modify-write, because this file is the account sub-project's
    /// `config.toml` and holds its `client_id` too. `Err` when the file could
    /// not be written — or could not be read: what could not be read is
    /// never overwritten (the final review's m5). A missing file is not
    /// unreadable; it is an empty configuration.
    fn save_root(&self, root: Option<&Persisted>) -> Result<(), String> {
        let Some(config_file) = &self.config_file else {
            return Ok(());
        };
        let mut config = Config::load(config_file).map_err(|e| {
            format!(
                "cannot record the sync folder in {}: it cannot be read ({e}), and it is not \
                 overwritten, since it holds the account's settings too",
                config_file.display()
            )
        })?;
        match root {
            Some(root) => {
                config.sync_root = root.path.display().to_string();
                config.sync_root_id = root.root_id.clone();
                config.sync_root_intercepted = root.intercepted;
            }
            None => {
                config.sync_root.clear();
                config.sync_root_id.clear();
                config.sync_root_intercepted = true;
            }
        }
        config
            .save(config_file)
            .map_err(|e| format!("cannot record the sync folder in {}: {e}", config_file.display()))
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
        if self.config_file.is_some() && self.persisted_root().as_ref() != Some(root) {
            self.persist_or_log(Some(root));
        }
    }

    fn persisted_root(&self) -> Option<Persisted> {
        let config_file = self.config_file.as_ref()?;
        let config = Config::load(config_file)
            .map_err(|e| tracing::warn!("ignoring unreadable {}: {e}", config_file.display()))
            .ok()?;
        (!config.sync_root.is_empty()).then(|| Persisted {
            path: PathBuf::from(&config.sync_root),
            root_id: config.sync_root_id,
            intercepted: config.sync_root_intercepted,
        })
    }

    /// Brings the sync folder up, or back up: re-registers the root with the
    /// helper — which re-marks the whole tree a restarted helper has
    /// forgotten — and re-runs §4.4's recovery walk, or, when no root is
    /// registered yet, restores the one persisted at the last start.
    ///
    /// Called once at startup and again after every helper reconnect
    /// (Rulings H106 and H107). Without the first of those, §4.4's walk —
    /// which several rounds of this plan built — never ran in the shipped
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
        self.restore_locked().await;
        match self.registration() {
            // Registered without interception: there is no helper
            // registration to renew, and a helper appearing later does not
            // silently upgrade a mode the user asked for explicitly. But a
            // recovery that had to leave files alone because a helper was
            // running with no link to it (Ruling H146) runs again now that
            // there is one.
            Some(reg) if !reg.intercepted => {
                if reg.recovery_deferred && self.link().is_some() {
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
        if self.registration().is_some() {
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

    /// Takes an intercepted root restored from `config.toml` as this
    /// daemon's registration, before anything is asked of the helper, and
    /// publishes it as waiting for the helper.
    ///
    /// Its id comes from `config.toml` — it is the name the helper holds the
    /// root by, and all a Forget needs even when the folder is gone — or,
    /// from a config written before the id was recorded, from the folder
    /// itself. With neither, the root is not held, and the failure is
    /// published as a startup failure always was; that leaves the one case
    /// spec §12 lists.
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
            root: SyncRoot { path: persisted.path, root_id },
            intercepted: true,
            recovery_deferred: false,
        });
        self.state.update(|s| {
            s.root_path = shown;
            s.root_state = RootState::Error;
            s.last_error = HELPER_LOST_WARNING.to_owned();
        });
    }

    /// Publishes the helper's disappearance (Ruling H107): `RootState` used
    /// to stay `ready` with an empty `LastError` while the sync folder was,
    /// in the only sense that matters, dead — nothing intercepting, nothing
    /// reconnecting, and every un-hydrated file reading as zeros.
    pub fn report_helper_lost(&self) {
        let Some(reg) = self.registration() else {
            return;
        };
        if !reg.intercepted {
            return;
        }
        self.state.update(|s| {
            s.root_state = RootState::Error;
            s.last_error = HELPER_LOST_WARNING.to_owned();
        });
    }

    /// Mirrors `source_dir` into the root as placeholders (spec §3.1): the
    /// offline stand-in for the real fill. Also remembers `source_dir` as
    /// this service's `ContentSource`, so `Hydrate()` (and any real
    /// interception-driven fill routed through `serve_hydrations`) has
    /// somewhere to fetch bytes from afterwards.
    pub async fn populate_from_directory(&self, source_dir: &Path) -> Result<u64, SyncError> {
        // The mode decides what is asked of the helper below, so it must not
        // change until this is done (see `lifecycle`).
        let _lifecycle = self.lifecycle.read().await;
        let reg = self.require_registration()?;
        // The final review's m10: a source that overlaps the root is refused
        // — one inside it is a directory of placeholders, which a fill would
        // copy as zeros into a file it then stamps `hydrated`, and one around
        // it would be mirrored into itself. Compared by resolved path, so a
        // symlink to either is seen through; a bind mount is not — but a
        // file reached through one carries konedrive's attributes, and every
        // source file is refused on those, or on leading into the folder,
        // both when it is mirrored and when it is read (Ruling H148).
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
        // Ruling H135: a root registered without interception is never
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
        // Ruling H148: the source refuses, when the bytes are read, any file
        // that leads into the folder by then — a symlink swapped since.
        *self.source.lock().unwrap() =
            Some(Arc::new(LocalDir::new(source).refusing_files_of(root)) as Arc<dyn ContentSource>);
        Ok(created)
    }

    /// Fills one placeholder now — see the type's own doc comment for why
    /// this fills directly rather than only through kernel interception.
    ///
    /// # Rulings H102 and H103: open first, then lock, then look again
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
        let reg = self.require_registration()?;
        let Some(source) = self.source.lock().unwrap().clone() else {
            return Err(SyncError::NoSource);
        };

        let root = reg.root.clone();
        let target = path.to_path_buf();
        let file = tokio::task::spawn_blocking(move || root.open_inside(&target))
            .await
            .map_err(|e| SyncError::Io(format!("the hydration task failed: {e}")))??;
        let key = InodeKey::of(&file).map_err(|e| SyncError::Io(e.to_string()))?;

        // Serializes against `dehydrate()` and against `serve_hydrations`'s
        // own fills of the same inode (both share this table).
        let _guard = self.locks.lock(key).await;

        let (file, decision) = tokio::task::spawn_blocking(move || {
            let decision = classify_for_hydration(&file);
            (file, decision)
        })
        .await
        .map_err(|e| SyncError::Io(format!("the hydration task failed: {e}")))?;
        let may_be_marked = match decision? {
            Fill::AlreadyThere => return Ok(()),
            Fill::Needed { may_be_marked } => may_be_marked,
        };
        // A file that could be carrying an ignore mark has the way cleared
        // before the fill can fail and punch it (`source::hydrate_with`), by
        // Ruling H146's local rule. An intercepted root needs its link for
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
        match source::hydrate_with(fd, source.as_ref(), clearance.as_ref()).await {
            Ok(()) => Ok(()),
            Err(FillError::NotCleared(NotCleared::Unlinked)) => Err(SyncError::NoHelper),
            Err(FillError::NotCleared(e)) => Err(SyncError::Io(format!("nothing was filled: {e}"))),
            Err(FillError::Errno(errno)) => Err(SyncError::Io(format!(
                "hydration failed: {}",
                std::io::Error::from_raw_os_error(errno)
            ))),
        }
    }

    /// Frees a hydrated file's space back to a placeholder (spec §8).
    ///
    /// The file is opened here, through the same `SyncRoot::open_inside`
    /// gate, so that the per-inode lock can be taken on the inode that is
    /// about to be emptied — `(st_dev, st_ino)` from that very descriptor,
    /// never a name (Ruling H101) — and so that the descriptor the lock was
    /// taken on is the one `root::dehydrate_opened` marks, clears and
    /// punches (Ruling H68: one open per dehydration).
    pub async fn dehydrate(&self, path: &Path) -> Result<(), SyncError> {
        // The mode decides what the punch may go by, and the punch may wait
        // for a fill of the same inode after that; the mode must not change in
        // between (see `lifecycle`).
        let _lifecycle = self.lifecycle.read().await;
        let reg = self.require_registration()?;
        // Ruling H146's local rule decides at the punch (`Clearance`,
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

        let root = reg.root.clone();
        let target = path.to_path_buf();
        let file = tokio::task::spawn_blocking(move || root.open_inside(&target))
            .await
            .map_err(|e| SyncError::Io(format!("the dehydration task failed: {e}")))??;
        let key = InodeKey::of(&file).map_err(|e| SyncError::Io(e.to_string()))?;

        let _guard = self.locks.lock(key).await;
        root::dehydrate_opened(&clearance, file).await.map_err(SyncError::from)
    }

    /// The file's own state, or `not-managed` for anything that is not a
    /// plain file this daemon actually manages inside the current root —
    /// including a file outside the root altogether, per spec §3.1.
    ///
    /// # Ruling H108: a query never opens the file
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
        // Ruling H76/I7: `canonicalize` and `getxattr` are blocking syscalls
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
/// # Ruling H109: the `hydrated` label is not believed on its own
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
        // (`root::dehydrate`'s Ruling N3) — left behind, and §5.2 says
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

/// One file's state read by name, with no open at all (Ruling H108).
/// `xattr::get` is `lgetxattr`: it does not follow a final symlink, and the
/// path it is given has already been canonicalized.
fn state_of_path(path: &Path) -> Option<State> {
    let raw = xattr::get(path, XATTR_STATE).ok()??;
    String::from_utf8_lossy(&raw).parse().ok()
}

/// `SyncService` is itself a valid, if initially empty, `ContentSource`:
/// `serve_hydrations` is started once, at daemon startup (Ruling H80),
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

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The recursive half of `populate_from_directory`: mirrors `source` into
/// `dest` as placeholders, marking every newly-created directory before
/// anything is created inside it (invariant M1) and skipping any name that
/// already exists. `relative` accumulates the `item_id` — the entry's path
/// relative to the original `source_dir` — as the walk descends.
///
/// This is explicitly the offline test path (spec §3.1's own description),
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
/// then the whole populate is refused (Ruling H148), since a file filled
/// from it would be filled with a placeholder's zeros.
/// What every level of [`populate_walk`] needs besides where it is: the
/// link to mark new directories through, if any, and the resolved sync
/// folder that no source file may lead into (Ruling H148).
#[derive(Clone, Copy)]
struct Walk<'a> {
    link: Option<&'a HelperLink>,
    root: &'a Path,
}

/// A source file refused because it leads into the sync folder (Ruling
/// H148): `PopulateFromDirectory` answers `Unsupported` with this text.
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

/// Runs one blocking step of the populate walk off the reactor (Ruling
/// H76/I7): `read_dir`, `stat`, `create_dir`, `openat` and `linkat` are all
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
    // Ruling H148: a file that leads into the folder — a symlink to a
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

/// Keeps a helper link alive for the life of the daemon (Ruling H107).
///
/// Connects, brings the sync folder up on that link, serves hydration
/// requests until the connection drops, publishes the drop at once — not
/// once the downloads under way have finished (Ruling H141) — and tries
/// again after a backoff that grows to a cap. Nothing reconnected before: when the
/// helper went away `serve_hydrations` simply returned, `RootState` stayed
/// `ready` with `LastError` empty, and every un-hydrated file in the folder
/// read as zeros with nothing saying so.
///
/// `backoff` is the first delay; each failure doubles it up to
/// `MAX_HELPER_BACKOFF`. A successful connection resets it.
pub async fn supervise_helper(
    service: Arc<SyncService>,
    socket_path: PathBuf,
    backoff: Duration,
) {
    let mut wait = backoff;
    loop {
        match HelperLink::connect(&socket_path).await {
            Ok((link, requests)) => {
                wait = backoff;
                tracing::info!("connected to the konedrive helper at {}", socket_path.display());
                service.set_helper_socket(&socket_path);
                service.set_link(Some(link.clone()));
                // Re-register the root before serving anything: a helper
                // that has just started has no marks at all, and the root's
                // own registration is what puts them back.
                service.resume().await;
                let source = Arc::clone(&service) as Arc<dyn ContentSource>;
                // A task of its own, and the end of the connection is
                // waited for on the link itself (Ruling H141). Awaiting
                // `serve_hydrations` here waited for every fill still
                // running as well — a download of any length — and until
                // then the loss was not published, the dead link was still
                // handed out, and nothing reconnected. The fills already
                // running finish in that task, their `HydrateDone` going
                // nowhere; the per-inode locks keep each of them ahead of
                // any fill of the same file on the next connection.
                let serving =
                    tokio::spawn(serve_hydrations(link.clone(), requests, source, service.locks()));
                link.closed().await;
                tracing::error!("the konedrive helper connection dropped");
                service.set_link(None);
                service.report_helper_lost();
                drop(serving);
            }
            Err(e) => {
                tracing::warn!(
                    "cannot connect to the konedrive helper at {}: {e}; retrying in {:?}",
                    socket_path.display(),
                    wait
                );
            }
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(MAX_HELPER_BACKOFF);
    }
}

/// The longest [`supervise_helper`] ever waits between attempts.
pub const MAX_HELPER_BACKOFF: Duration = Duration::from_secs(30);

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

    /// Ruling H52. A panicking fill answers no one on its own: it closes the
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

    /// Ruling H53/H29, pinned the only way it can be: the discriminator is
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

    // --- C1 of the final review: a request looks again (Ruling H137) -----

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

    /// C1 of the final review, on the host (Ruling H137). A request the helper
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

    /// The credit contract's true worst case (Ruling H142, the final
    /// review's I4). Four fills have finished and sent `HydrateDone`, and
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

    // --- InodeLocks (Task 11, Ruling: spec §8's serialization) -----------

    /// A file's `(dev, ino)`, the way every caller of `InodeLocks` gets one.
    fn key_of(path: &std::path::Path) -> InodeKey {
        InodeKey::of(&std::fs::File::open(path).unwrap()).unwrap()
    }

    /// Ruling H101, the property a path key cannot have: two names for one
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

    /// Ruling H122. The lock has two constructors for one key — `of` on the
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

    /// `try_lock` (Ruling H147) is refused while the key is held, leaves no
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
        // The final review's m11: an interrupted file that something has open
        // — on reconnect, the suspended opener whose request is not served
        // yet, or a fill still running from the connection before (Ruling
        // H141) — refused recovery's lease and published `RootState = error`
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
        assert!(service.last_error().contains("in use"), "{}", service.last_error());
        assert_eq!(service.item_state(&path).await, "hydrating", "and it is left as found");
    }

    /// A `RecoveryReport` with `failed > 0` is spec §8's "silently
    /// unrecoverable" case (a refused `ClearIgnore`) — it must not stay
    /// silent: `RegisterRoot` still succeeds (the root itself is usable),
    /// but `RootState`/`LastError` must say so.
    #[tokio::test]
    async fn a_failed_recovery_surfaces_through_root_state_and_last_error() {
        let (service, _sockets, helper) = service_with_helper().await;
        let root_dir = tempfile::tempdir().unwrap();

        // Ruling H78: a folder that already carries a root id is exempt from
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
        // force this; it is `busy` now, not a failure — the final review's
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

    /// Ruling H101, the whole of C1. Two names for one inode must serialize.
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

    /// The other pair spec §8 names: "a hydration request for a file being
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

    // --- The gate `hydrate_now` writes through (Ruling H103) -------------

    /// A registration that has been removed or replaced no longer authorises
    /// writing inside that folder. `dehydrate` has checked this since Ruling
    /// H76 — through `SyncRoot::open_inside`, which verifies the folder
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

    /// Ruling H109. A file labelled `hydrated` over a hole is §9's named
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

    /// The final review's m6 (Ruling H144). A zero-byte file is created
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

    /// The final review's m10 (Ruling H144). A populate source inside the
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

    /// Ruling H148 (the final re-review found m10 only partly fixed). A
    /// source *directory* that overlaps the root is refused, but a source
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

    /// The same, decided again where the bytes are read (Ruling H148): a
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

    // --- `ItemState` is a query (Ruling H108) ----------------------------

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

    /// Ruling H110: a `RegisterRoot` that fails must leave nothing behind —
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

    // --- Registering without interception (Ruling H105) ------------------

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
        // (Ruling H146), whatever this machine has at the real socket path.
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

    // --- Startup and the helper supervisor (Rulings H106, H107) ----------

    /// §3.1's "persisted, so it survives a restart" — and §4.4's walk, which
    /// without it never ran at a startup at all: recovery only ever ran
    /// inside a `RegisterRoot` call, so after a crash a file left
    /// `hydrating` stayed that way until a human registered the folder
    /// again.
    ///
    /// Ruling H80's order is pinned here too, for the first time: the helper
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
            let service = SyncService::new(Some(link), None, Some(config_file.clone()));
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
        let restarted = SyncService::new(Some(link), None, Some(config_file));
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
            Config::load(restarted.config_file.as_ref().unwrap()).unwrap().sync_root,
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

    /// Ruling H107. `HelperLink` fails outstanding and later calls loudly,
    /// which is right — but nothing reconnected, and the published state
    /// said `ready` with an empty `LastError` the whole time the folder was
    /// dead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_helper_that_goes_away_is_published_and_reconnected_to() {
        a_helper_that_goes_away_is_published_and_reconnected_to_with(false).await;
    }

    /// Ruling H141, the final review's I3. The supervisor used to find out
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

    // --- Guards over verified-correct behaviour (Ruling H122) ------------

    /// R3. Unregistering a root must tell the helper, or the helper keeps
    /// the tree marked — and, with the uid no longer owning a root, answers
    /// every placeholder open in it `EIO` (Ruling H58's measurement).
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

    /// N6. The persisted "intercepted" flag must survive a restart. A root
    /// registered without interception on a machine with no helper — the
    /// case the user's own machine is in — would otherwise be restored as an
    /// intercepted root, which waits for a helper that never comes: the
    /// folder simply would not come back.
    #[tokio::test]
    async fn a_root_persisted_without_interception_comes_back_without_a_helper() {
        let config_dir = tempfile::tempdir().unwrap();
        let config_file = config_dir.path().join("config.toml");
        let root_dir = tempfile::tempdir().unwrap();
        {
            let service = SyncService::new(None, None, Some(config_file.clone()));
            service.register_root_without_interception(root_dir.path()).await.unwrap();
        }
        assert!(
            !Config::load(&config_file).unwrap().sync_root_intercepted,
            "the mode must be written down with the root"
        );

        let restarted = SyncService::new(None, None, Some(config_file));
        restarted.resume().await;

        assert_eq!(
            restarted.root().map(|r| r.path),
            Some(std::fs::canonicalize(root_dir.path()).unwrap()),
            "a root registered without interception did not come back after a restart"
        );
        assert_eq!(restarted.root_state(), "no-interception");
    }

    /// Q3. A root registered without interception stays that way when a
    /// helper connects later. `resume` refuses the upgrade on purpose: the
    /// user asked for this mode by name, and quietly changing what protects
    /// their files — in either direction — is not this daemon's call.
    #[tokio::test]
    async fn a_helper_appearing_does_not_upgrade_a_root_registered_without_interception() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        let service = SyncService::new(None, None, None);
        let root_dir = tempfile::tempdir().unwrap();
        service.register_root_without_interception(root_dir.path()).await.unwrap();

        // What `supervise_helper` does the moment a helper answers.
        let (link, _requests) = HelperLink::connect(&socket_path).await.unwrap();
        helper.forget();
        service.set_link(Some(link));
        service.resume().await;

        assert_eq!(service.root_state(), "no-interception");
        assert!(
            service.last_error().contains("read as zeros"),
            "the warning must still be there: {}",
            service.last_error()
        );
        assert!(
            !helper.seen().contains(&Seen::RegisterRoot),
            "the root was registered with the helper behind the user's back: {:?}",
            helper.seen()
        );
    }

    // --- The mode boundary (Rulings H133, H134, H135) --------------------

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
        let service = SyncService::new(Some(link), None, Some(config_file.clone()));
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

    /// Ruling H146's local rule at a dehydration in a root registered
    /// without interception, with a link: the helper is asked to clear the
    /// file's ignore mark, as in any other root, and the punch follows. It
    /// used to be skipped, on the strength of a chain of reasoning — nothing
    /// in such a folder is intercepted, and interception resumes only through
    /// a walk that clears every mark — which the final re-review broke a
    /// third time (N2): a stale-marked file emptied here read zeros once the
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

    /// Ruling H146 with no link. A helper running with no link to this
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

    /// Ruling H146 at the other punch site: recovery of a root registered
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
    /// again, clearing the mark, once the link is up (Ruling H146). Without
    /// the second run the file would stay `dehydrating` until the next start.
    #[tokio::test]
    async fn recovery_deferred_while_an_unlinked_helper_runs_finishes_once_linked() {
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let helper = FakeHelper::start(socket_path.clone(), Duration::ZERO);
        let service = SyncService::new(None, None, None);
        service.set_helper_socket(&socket_path);
        let (root_dir, stuck) = root_with_a_stuck_file();

        service.register_root_without_interception(root_dir.path()).await.unwrap();

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

        let restarted = SyncService::new(None, None, Some(config_file.clone()));
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

        let restarted = SyncService::new(Some(link), None, Some(config_file.clone()));
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
        Config { sync_root: resolved(root_dir.path()), ..Config::default() }
            .save(&config_file)
            .unwrap();

        let restarted = SyncService::new(None, None, Some(config_file));
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
    /// The final review's m5 (Ruling H144). `config.toml` is the account
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
        let service = SyncService::new(service.link(), None, Some(blocker.join("config.toml")));
        let root_dir = tempfile::tempdir().unwrap();

        let error = service.register_root(root_dir.path()).await.unwrap_err();

        assert!(matches!(error, SyncError::Io(_)), "{error:?}");
        assert!(service.root().is_none());
        assert!(helper.seen().is_empty(), "the helper was told: {:?}", helper.seen());
    }

    /// Ruling H110 on both sides: a `RegisterRoot` that fails after the
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

        let restarted = SyncService::new(None, None, Some(config_file.clone()));
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
    /// the `ClearIgnore` that is suddenly needed. A Forget waits for it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_forget_waits_for_a_dehydration_that_is_already_under_way() {
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
        let forgetting = {
            let service = Arc::clone(&service);
            tokio::spawn(async move { service.unregister_root().await })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(
            !forgetting.is_finished(),
            "the root was forgotten under a dehydration that had already decided its mode"
        );
        drop(fill);
        dehydrating.await.unwrap().unwrap();
        forgetting.await.unwrap().unwrap();
    }
}
