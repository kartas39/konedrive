//! The outbox worker (`docs/design/writes.md` §5, §6, §10, §7): sends
//! the outbox's rows to OneDrive, one step each, and commits each answer.
//!
//! **Order.** Rows run as the outbox's four rules allow ([`TreeStore::outbox_pick`]), in `seq`
//! order: metadata rows (`mkdir`, `move`, `delete`) one at a time, content
//! rows (`create`, `update`) beside them, each in a slot of the account's
//! transfer pool (`konedrive_graph::pool`), small or large alike. A row is `running` from the moment it is taken until its commit, so
//! an examination never merges into it; a `running` row the worker does not
//! hold (a crash, a stop) is replayed first, as its dependencies allow.
//!
//! **Each step can be replayed** (WR7). A new file or folder goes up with
//! `conflictBehavior=fail`, a change with `If-Match`, so a replay whose first
//! request landed meets `409` or `412` and settles it by content hash or by
//! place ("did my last request land?"). Upload sessions are persisted before
//! the first byte and after every fragment, and resumed from where the
//! server stands.
//!
//! **The commit** (§3.5): the file's attributes through its own descriptor
//! (the stamp from the snapshot, the cTag, `hydrated`, then the item id),
//! then one store transaction ([`TreeStore::outbox_commit`]), both under the
//! per-root tree lock a cycle's stage-to-swap takes too (§3.7).
//!
//! **Guards that fail** are resolved by reading again, never by forcing
//! (WR2): §3.6's answers and §6's conflicts, keeping both versions where
//! both changed. A name still held by an item a live row is freeing is
//! taken through a temporary name (`.konedrive-swap-*`, F55 (7)): never
//! adopted, never copied.
//!
//! **Throttling, offline, pause, sign-in** stop the whole worker: the rows
//! keep their states. **A full OneDrive** stops only what sends content, and
//! a file too big for what is left waits alone ([`space`]). A read-write folder's sync starts it beside the
//! watcher and stops it with it (`sync::write_mode`); the watcher's
//! examination wakes it whenever it records rows.
//!
//! [`TreeStore::outbox_pick`]: konedrive_tree::TreeStore::outbox_pick
//! [`TreeStore::outbox_commit`]: konedrive_tree::TreeStore::outbox_commit

mod content;
pub(crate) mod engine;
pub mod local;
pub mod move_out;
pub mod space;
mod steps;

pub mod kept_back;
#[cfg(test)]
pub(crate) mod tests;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use konedrive_graph::drive::DriveClient;
use crate::folder::root::SyncRoot;
use crate::folder::locks::InodeLocks;
use konedrive_tree::outbox::{Reason, SessionUrl};
use konedrive_tree::{ActivityRow, Store, TreeError};

pub(crate) use engine::Engine;

/// Takes `user.konedrive.sync` off the files of `rows`, which left the outbox with no
/// worker to do it: the outbox was emptied (`sync::outbox`, `drop_outbox`). Best effort,
/// by name.
pub fn clear_marks(root: &SyncRoot, rows: &[konedrive_tree::outbox::OutboxRow]) {
    let disk = match crate::folder::disk::Disk::open(root, false) {
        Ok(disk) => disk,
        Err(e) => {
            tracing::debug!("the upload marks of {} dropped row(s) stay: the folder cannot be opened: {e}", rows.len());
            return;
        }
    };
    for row in rows {
        local::mark(&disk, &row.rel, None);
    }
}

/// A name the worker gives an item in OneDrive while the name it takes is
/// still another item's (§4.4, F55 (7)); `.konedrive-*` names are never
/// uploaded from the folder, so none can be a user's.
pub use konedrive_tree::outbox::SWAP_PREFIX;

/// Retries of a row that failed for a reason expected to pass: 1 s,
/// doubling, at most an hour (§3.6; provisional). Never dropped.
pub const BACKOFF_FIRST: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(3600);
/// Throttled without `Retry-After`: 10 s, doubling, at most an hour (§4.10;
/// provisional).
pub const THROTTLE_FIRST: Duration = Duration::from_secs(10);

/// The activity kinds the worker writes (§9; the outbox on the bus adds them to the D-Bus
/// surface's list).
pub mod kind {
    pub const UPLOADED: &str = "uploaded";
    pub const CLOUD_MOVED: &str = "cloud-moved";
    pub const CLOUD_DELETED: &str = "cloud-deleted";
    pub const UPLOAD_FAILED: &str = "upload-failed";
    pub const RESTORED: &str = "restored";
    pub const CONFLICT: &str = "conflict";
    /// A file or folder removed here before its upload finished.
    pub const NOT_UPLOADED: &str = "not-uploaded";
}

/// What the worker needs from the account it serves.
pub trait OutboxHost: Send + Sync {
    /// An activity event, already in the tree store: for the live signal.
    fn activity(&self, _event: &ActivityRow) {}
    /// The worker's status changed: its counts, its uploads, its trouble.
    fn status(&self, _status: &WorkerStatus) {}
    /// What is kept back, summed again (`NotUploadedSummary()`, issue #38).
    fn kept_back(&self, _summary: &[crate::upload::kept_back::SummaryRow]) {}
    /// OneDrive changed under a row (§6), or a folder a row needs is gone
    /// there: a delta cycle should run soon, so that the base catches up and
    /// the reconcile places what came back. The delta carries it: a plain
    /// cycle, not a Full reconcile, which scans the whole folder.
    fn cycle_wanted(&self) {}
    /// Whether the account's background work stops now (`conditions::running`): the user's pause,
    /// kept in `store`, or what else the account's one place decides. Asked before each row
    /// is taken, and between the fragments of an upload.
    fn stopped(&self, store: &Store) -> bool {
        crate::conditions::running::user_pause(store, self.now()).is_some()
    }
    /// The time, in unix seconds, by the account's clock (`conditions::running::Clock`): a
    /// timed pause is over for the worker when it is for everything else of the account.
    fn now(&self) -> i64 {
        crate::status::activity::unix_now()
    }
    /// Whether the account may change OneDrive now (`docs/design/writes.md` §2): asked
    /// before each row is taken, and between the fragments of an upload. `Err` says why not:
    /// nothing more is sent then, and the rows wait. The answer may take reading a file
    /// (`config.toml`): the worker asks from a blocking thread (`Engine::may_write`).
    fn may_write(&self) -> Result<(), String> {
        Ok(())
    }
    /// An item's local object was forgotten and OneDrive's version must be
    /// placed again (§6: delete × edit), though the delta may have carried it
    /// already: a cycle with a Full reconcile should run soon (the outbox on
    /// the bus).
    fn full_cycle_wanted(&self) {
        self.cycle_wanted();
    }
}

/// Cancels upload session `url`, given up (issue #47): cancelled, or gone
/// already, it leaves the store's list of sessions; a cancel that fails keeps
/// it there, for a later look ([`cancel_given_up`]). Whether it was cancelled.
pub(crate) async fn cancel_session(store: &Store, drive: &DriveClient, url: &SessionUrl) -> Result<bool, TreeError> {
    match drive.cancel_upload(url.as_str()).await {
        Ok(()) => {
            let url = url.clone();
            store.call(move |s| s.upload_session_closed(&url)).await?;
            Ok(true)
        }
        Err(err) => {
            tracing::info!("an upload session given up was not cancelled; it is cancelled later: {err}");
            Ok(false)
        }
    }
}

/// Cancels up to `limit` of the upload sessions given up (issue #47): listed,
/// and pointed at by no row. Stops at the first cancel that fails; whether
/// none did.
pub async fn cancel_given_up(store: &Store, drive: &DriveClient, limit: usize) -> bool {
    // With the look, the records of openings whose row left long ago go (issue #89).
    let now = crate::status::activity::unix_now();
    if let Err(e) = store.call(move |s| s.upload_openings_expire(now)).await {
        tracing::warn!("cannot expire the upload openings: {e}");
    }
    let urls = match store.call(move |s| s.upload_sessions_given_up(limit)).await {
        Ok(urls) => urls,
        Err(e) => {
            tracing::warn!("cannot read the upload sessions to cancel: {e}");
            return false;
        }
    };
    for url in urls {
        if !matches!(cancel_session(store, drive, &url).await, Ok(true)) {
            return false;
        }
    }
    true
}

/// Runs `work` — the store's jobs and what follows them — as a task of the
/// runtime it is asked on, without waiting for it. Every caller is on the
/// daemon's runtime; asked anywhere else, the work is not done, and the
/// journal says so.
fn detach(what: &str, work: impl std::future::Future<Output = ()> + Send + 'static) {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            runtime.spawn(work);
        }
        Err(_) => tracing::warn!("{what}, asked with no runtime to do it on, is ignored"),
    }
}

/// A host that listens to nothing.
pub struct NoHost;

impl OutboxHost for NoHost {}

/// How files are cut (provisional numbers; the tests make them small). How many
/// run at once is the account's transfer pool's (`konedrive_graph::pool`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The fragment a file goes up in, a multiple of 320 KiB: a file up to
    /// this size is one fragment.
    pub chunk: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { chunk: konedrive_graph::drive::CHUNK_SIZE }
    }
}

/// Everything the worker works with, for one account's folder.
pub struct WorkerConfig {
    pub root: SyncRoot,
    pub store: Store,
    pub drive: DriveClient,
    /// The folder's per-inode locks: a read for an upload and a free-up of
    /// the same file exclude each other.
    pub locks: InodeLocks,
    /// For conflict copies (§6): `machine_name` from the account's config.
    pub machine_name: String,
    /// The per-root tree mutex (§3.7): held across each commit; a cycle
    /// holds it from staging to swap.
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
    pub host: Arc<dyn OutboxHost>,
    pub limits: Limits,
    /// The helper and the fills `move-out` rows need; `None` leaves
    /// them waiting.
    pub moved_out: Option<move_out::MoveOuts>,
    /// The account's one quota (`crate::account::quota`): what the space check reads
    /// and adjusts, keeping no copy of its own ([`space`]).
    pub quota: crate::account::quota::Quota,
}

/// One upload under way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upload {
    /// Relative to the root.
    pub rel: PathBuf,
    pub sent: u64,
    pub total: u64,
}

/// The worker's own state, handed to the host on every change ([`OutboxHost::status`]):
/// only what the host shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerStatus {
    /// OneDrive asked to wait until then (unix seconds): the folder's note says so.
    pub throttled_until: Option<i64>,
    /// Why the folder could not be opened at the last drain: the folder's note says so.
    pub folder_closed: Option<String>,
    /// Content going up now, as `Uploads` shows it.
    pub uploads: Vec<Upload>,
    /// What the outbox held when the worker last looked: `PendingCount`,
    /// `PendingBytes`, `BlockedCount`.
    pub counts: OutboxCounts,
    /// `QuotaFull`: OneDrive is full, and no content goes up ([`space`]).
    pub quota_full: bool,
}

/// What the outbox holds, for `PendingCount`, `PendingBytes` and
/// `BlockedCount` (§9).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutboxCounts {
    /// Rows waiting, ready, running or in backoff.
    pub pending: u32,
    /// The size of the files those rows send.
    pub pending_bytes: u64,
    /// Rows that need the user.
    pub blocked: u32,
    /// Removals held by the mass-delete guard.
    pub held: u32,
    /// `QuotaWaitingCount`, `QuotaWaitingBytes`: while OneDrive is full,
    /// the changes that send content, which wait for space.
    pub space_waiting: u32,
    pub space_waiting_bytes: u64,
    /// `TooBigCount`: files refused as too big for the space left; and their size, which only
    /// the queue totals use (issue #16: kept back, so not left to upload).
    pub too_big: u32,
    pub too_big_bytes: u64,
}

impl OutboxCounts {
    /// The counts of the outbox whose rows are `groups` ([`TreeStore::outbox_groups`]),
    /// `full` while OneDrive is full.
    ///
    /// [`TreeStore::outbox_groups`]: konedrive_tree::TreeStore::outbox_groups
    pub fn of(groups: &[konedrive_tree::outbox::OutboxGroup], full: bool) -> Self {
        use konedrive_tree::outbox::OutboxState;
        let mut counts = OutboxCounts::default();
        for group in groups {
            let n = u32::try_from(group.count).unwrap_or(u32::MAX);
            match group.state() {
                OutboxState::Blocked => counts.blocked = counts.blocked.saturating_add(n),
                OutboxState::Held => counts.held = counts.held.saturating_add(n),
                _ => {
                    counts.pending = counts.pending.saturating_add(n);
                    counts.pending_bytes = counts.pending_bytes.saturating_add(group.bytes);
                    counts.add_space(group.kind(), group.reason().as_ref(), full, n, group.bytes);
                }
            }
        }
        counts
    }

    /// Counts `n` pending rows of `kind` and `reason`, of `bytes` in all, where they wait for space.
    fn add_space(&mut self, kind: konedrive_tree::outbox::OutboxKind, reason: Option<&Reason>, full: bool, n: u32, bytes: u64) {
        if reason.is_some_and(|r| r.sizes().is_some()) {
            self.too_big = self.too_big.saturating_add(n);
            self.too_big_bytes = self.too_big_bytes.saturating_add(bytes);
        } else if full && kind.sends_content() || reason == Some(&Reason::WaitingForSpace) {
            self.space_waiting = self.space_waiting.saturating_add(n);
            self.space_waiting_bytes = self.space_waiting_bytes.saturating_add(bytes);
        }
    }
}

/// The counts of the outbox in `store`, `full` while OneDrive is full: one
/// SQL sum, nothing read from the disk.
pub fn outbox_counts(store: &konedrive_tree::TreeStore, full: bool) -> Result<OutboxCounts, TreeError> {
    Ok(OutboxCounts::of(&store.outbox_groups()?, full))
}

/// A point where the worker's tests make it stop as if the daemon had died
/// there (§5). Each armed point fires once; the row stays `running` and the
/// worker takes nothing new until it is built again on the same store. Only
/// the names are compiled into the daemon: nothing can be armed outside the
/// crate's tests, and a point costs nothing there (`Engine::fault`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// A request went out and was answered; the answer is lost.
    AfterSend,
    /// An upload session was opened and not persisted.
    SessionNotPersisted,
    /// After this many fragments were accepted and persisted.
    MidSession(u32),
    /// Commit step 1 half done: stamp, cTag and state, not the item id.
    CommitStep1Partial,
    /// Commit step 1 done, step 2 not.
    AfterCommitStep1,
    /// A move out: the content local and the attributes taken off, the
    /// item not deleted yet.
    AfterStrip,
    /// A folder's move out: the first of its files stripped, the rest not.
    MidStrip,
}

/// The worker of one account's outbox. Nothing runs until [`start`]; the
/// state it keeps (pause, throttling, offline) is its own, the pause also in
/// the store.
///
/// [`start`]: OutboxWorker::start
pub struct OutboxWorker {
    engine: Arc<Engine>,
    /// The run's token and task; the task is taken by [`close`](Self::close)
    /// to be waited for there.
    task: Mutex<Option<(CancellationToken, Option<tokio::task::JoinHandle<()>>)>>,
}

impl OutboxWorker {
    pub fn new(config: WorkerConfig) -> Self {
        Self { engine: Arc::new(Engine::new(config)), task: Mutex::new(None) }
    }

    /// Starts the worker on the current runtime (the mode switch calls it when the
    /// account becomes read-write, after the Full local scan). Rows a
    /// previous run left `running` are replayed first. Idempotent.
    pub fn start(&self) {
        let mut task = self.task.lock().unwrap_or_else(|p| p.into_inner());
        if task.is_some() {
            return;
        }
        let cancel = CancellationToken::new();
        let engine = Arc::clone(&self.engine);
        let token = cancel.clone();
        engine.silence(false);
        let handle = tokio::spawn(async move { engine.run(token).await });
        *task = Some((cancel, Some(handle)));
    }

    /// Stops the worker and waits for it. A request under way is cut off;
    /// its row stays `running` and is replayed at the next start (§5).
    pub async fn stop(&self) {
        let task = self.task.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some((cancel, handle)) = task {
            cancel.cancel();
            if let Some(handle) = handle {
                let _ = handle.await;
            }
        }
        // What the host clears after this stays cleared.
        self.engine.silence(true);
    }

    /// The daemon is stopping (issue #84): no row is taken any more, and
    /// the rows in flight finish what they sent — an opened session is
    /// persisted, an upload in fragments stops after the fragment in flight.
    /// The future ends once the worker has; the caller bounds the wait
    /// (`crate::daemon::stop`), and whatever is still in flight then is cut as
    /// [`stop`](Self::stop) cuts it. For good: not started again.
    pub fn close(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        self.engine.close();
        let handle = self.task.lock().unwrap_or_else(|p| p.into_inner()).as_mut().and_then(|(_, handle)| handle.take());
        async move {
            if let Some(handle) = handle {
                let _ = handle.await;
            }
        }
    }

    /// Looks at the outbox now: new rows, or anything that may have unblocked
    /// them. The examination calls it after recording rows.
    pub fn wake(&self) {
        self.engine.wake();
    }

    /// Sends nothing until [`cycle_done`](Self::cycle_done): a folder's
    /// first delta cycle runs before its outbox (`docs/design/writes.md` §3), and so
    /// does the one after the network came back (§4.9, `network_back`),
    /// whose rows in backoff then go at once.
    pub fn wait_for_cycle(&self, network_back: bool) {
        self.engine.wait_for_cycle(network_back);
    }

    /// A delta cycle went through.
    pub fn cycle_done(&self) {
        let engine = Arc::clone(&self.engine);
        detach("a cycle that went through", async move { engine.cycle_done().await });
    }

    /// The quota was read elsewhere (`RefreshInfo`, `Refresh`), into the
    /// account's quota already: *full* is decided again by it, and the
    /// waiting files that fit now go ([`space`]).
    pub fn quota_read(&self, quota: &konedrive_graph::drive::DriveQuota) {
        // Applied as a task of its own: it writes the rows it lets go.
        let (engine, quota) = (Arc::clone(&self.engine), quota.clone());
        detach("a quota read", async move { engine.decide_quota(&quota).await });
    }

    /// `Refresh()`: rows in backoff are tried now.
    pub fn retry_now(&self) {
        let engine = Arc::clone(&self.engine);
        detach("a retry of the outbox's waiting rows", async move {
            if let Err(e) = engine.retry_now().await {
                tracing::warn!("cannot make the outbox's waiting rows due: {e}");
            }
        });
    }

    /// The helper is back, with none of its marks (`docs/design/writes.md` §10): what
    /// the pending `move-out` rows name is marked again, before any row runs.
    pub fn helper_back(&self) {
        self.engine.helper_back();
    }

    /// Arms a fault point.
    #[cfg(test)]
    pub fn arm(&self, fault: Fault) {
        self.engine.arm(fault);
    }
}
