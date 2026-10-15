use std::sync::Arc;

use tokio::sync::watch;

use crate::status::totals;

/// `LiveChanges` on the bus: how changes made in OneDrive reach this computer now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LiveChanges {
    /// No socket: the account is stopped (pause or hold), or the folder is not a OneDrive
    /// folder, or no sync runs.
    #[default]
    Off,
    /// Trying to connect, or waiting before the next try: the poll runs at its normal interval.
    Connecting,
    /// The socket is up: changes arrive at once.
    Connected,
}

impl LiveChanges {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
        }
    }
}

/// `LocalScan.State`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ScanState {
    /// No check ran since the daemon started: the folder's sync was not started yet.
    #[default]
    None,
    Idle,
    Running,
}

impl ScanState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Idle => "idle",
            Self::Running => "running",
        }
    }
}

/// The check of the folder on disk at its start, as `LocalScan`'s properties publish it.
/// While idle, the reason and the start are the last check's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LocalScan {
    pub(crate) state: ScanState,
    /// Empty before the first scan.
    pub(crate) reason: String,
    /// Unix seconds; 0 before the first scan.
    pub(crate) started: i64,
    /// Unix seconds when the last scan finished; 0 for none since the daemon started.
    pub(crate) finished: i64,
    /// How long the last finished scan took, in seconds.
    pub(crate) took: u32,
}

/// What `Folder.State` reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RootState {
    /// No root is registered.
    #[default]
    None,
    /// A root is registered and, as far as this daemon knows, healthy.
    Ready,
    /// A folder is recorded and not up yet, and nothing is known to be wrong: it is being
    /// brought up (the start of a session).
    Waiting,
    /// A root is recorded, but something about it needs attention: it could not be
    /// brought up, or its account is held back. See `LastError`.
    Error,
}

impl RootState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Ready => "ready",
            Self::Waiting => "waiting",
            Self::Error => "error",
        }
    }
}

/// The observable sync state; `dbus::signals` turns changes into
/// `PropertiesChanged`, exactly as `account::state::AccountSnapshot` does for
/// `Account`. Grouped by who writes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncSnapshot {
    pub folder: FolderStatus,
    pub cycle: CycleStatus,
    pub local: LocalStatus,
    pub upload_queue: UploadQueueStatus,
    pub pause: PauseStatus,
    pub transfers: TransferStatus,
}

/// The folder's registration, worked out from the folder's state in one place
/// (`account_folder::publish`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderStatus {
    pub root_path: String,
    pub root_state: RootState,
    /// What the registration ran into.
    pub last_error: String,
    /// `Writable`: the folder's mount takes changes, and what is changed in the folder is
    /// uploaded now — its account is read-write and its plan's task runs. False from the
    /// moment a change of the folder tells its sync to stop.
    pub(crate) writable: bool,
}

/// What the cycles with OneDrive say (`listing::listing_cycle`, `listing::live`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CycleStatus {
    /// An initial or `410` listing of the drive is running.
    pub(crate) listing: bool,
    pub items_listed: u64,
    pub items_placed: u64,
    pub(crate) skipped_count: u64,
    /// What the folder's sync last ran into; `None` once a cycle succeeds.
    pub(crate) sync_trouble: Option<SyncTrouble>,
    /// `LastChecked`: unix seconds of the last cycle that
    /// succeeded, 0 for never.
    pub last_checked: i64,
    /// `LiveChanges`: whether changes made in OneDrive arrive at once, through the
    /// notification socket (`listing::live`).
    pub live_changes: LiveChanges,
}

/// What is on this computer: the space, the conflicts and the pins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalStatus {
    /// `LocalBytes`: what the folder's files take on disk, as last measured
    /// (`status::space::LocalSpace`).
    pub local_bytes: u64,
    /// `Conflicts.Count`: conflicts whose kept copy is still there.
    pub(crate) conflict_count: u32,
    /// `PinnedCount`: files and folders with a pin of their own, as the folder's mount
    /// has them.
    pub pinned_count: u32,
    /// `LocalScan`'s `State`, `Reason`, `Started`, `Directories`, `Files`,
    /// `Expected`, `Finished`, `Took`: nothing scans a folder any more, and these stay as
    /// they start (`none`, empty, 0) for the clients that read them.
    pub(crate) scan: LocalScan,
}

/// The upload queue as the plan's task last published it (`upload_queue::publish_queue_status`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UploadQueueStatus {
    /// What keeps the upload queue's changes from going: the account may not write
    /// though its folder is read-write, or OneDrive throttles its uploads. `None`
    /// otherwise.
    pub note: Option<UploadQueueNote>,
    /// `PendingCount`, `PendingBytes`, `BlockedCount`.
    pub pending_count: u32,
    pub(crate) pending_bytes: u64,
    pub blocked_count: u32,
    /// `Transfers.Uploads`: (full path, bytes sent, bytes in all), as `Downloads`.
    pub(crate) uploads: Vec<(String, u64, u64)>,
    /// `QuotaFull`: OneDrive is full and no content goes up.
    pub(crate) quota_full: bool,
    /// `QuotaWaitingCount`, `QuotaWaitingBytes`: while full, the changes
    /// that send content; `TooBigCount`: files too big for the space left.
    pub(crate) space_waiting_count: u32,
    pub(crate) space_waiting_bytes: u64,
    pub(crate) too_big_count: u32,
    /// Their size: not on the bus, but taken off what is left to upload ([`totals`]).
    pub(crate) too_big_bytes: u64,
}

/// Whether the account's background work stops (`account_folder/pause.rs`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PauseStatus {
    /// `Paused` and `PausedUntil`: `Some(until)` while paused, unix seconds,
    /// 0 meaning until resumed.
    pub(crate) paused_until: Option<i64>,
    /// `HeldBack`: why the account holds its background work back by itself
    /// (`conditions::running::Hold`), empty when it does not.
    pub held_back: String,
}

/// What moves, and how much is left.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferStatus {
    /// `DownloadSpeed`, `UploadSpeed`, `PoolInUse`, `PoolSize`, `PoolCeiling`, `LargeStreams`,
    /// `LargeStreamLimit`, `RetryAfter`: the account's transfer pool, once a second while
    /// anything moves or a `Retry-After` runs. `ActiveDownloads`, `ActiveUploads` and
    /// `LargeFiles` count the files of `Transfers.Downloads` and `Uploads` instead.
    pub(crate) throughput: konedrive_graph::pool::Throughput,
    /// `RetryAfterRefusal`: the full path of the file a program was first refused the
    /// content of during the `Retry-After` that runs (`status::throttle_wait`); empty when
    /// none was, and when no wait runs. Written by `ThrottleWait::tell_refusal`, taken off
    /// when the pool says the wait is over ([`SyncStateHandle::set_throughput`]).
    pub(crate) retry_after_refusal: String,
    /// The pinned files whose content is still to be downloaded, as the store had them at
    /// the last plan that asked. ONE writer: the plan's task, with every plan whose
    /// reading counted them (`SyncPlanTask::publish_round`), and to none when it stops. On
    /// the bus only through `queue`, which counts what is left to download from it.
    pub pinned_download: konedrive_tree::PinnedDownloadTotal,
    /// The downloads of pinned files that are not started because they would not leave
    /// the free space the account keeps on this computer's disk; `None` while none
    /// waits. ONE writer: the plan's task, each time it starts the steps of a plan
    /// (`SyncPlanTask::start_steps`), and to none when it stops. On the bus as a part
    /// of `LastError` and `Folder.Trouble` ([`running_trouble`]).
    pub disk_space_shortage: Option<DiskSpaceShortage>,
    /// `DownloadLeftCount`, `DownloadLeftBytes`, `DownloadDoneBytes`, `DownloadTimeLeft` and
    /// the same four for uploads: counted from the rest by [`totals::run`].
    pub queue: totals::QueueTotals,
}

impl SyncSnapshot {
    /// Whether the account's background work stops: paused by the user, or held back.
    pub(crate) fn stopped(&self) -> bool {
        self.pause.paused_until.is_some() || !self.pause.held_back.is_empty()
    }
}

/// What the folder's sync last ran into. `blocking` trouble —
/// signed out, another account, an unusable store — makes `RootState` read
/// `error`; the rest (no network) is said in `LastError` and retried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyncTrouble {
    pub(crate) text: String,
    pub(crate) blocking: bool,
    /// What kind of trouble it is: what `status::overall` decides by, never by `text`.
    pub(crate) kind: TroubleKind,
}

/// The kinds of [`SyncTrouble`] that the account's overall state tells apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TroubleKind {
    /// OneDrive cannot be reached: the account reads offline when nothing else is wrong.
    Unreachable,
    /// Anything else.
    Other,
}

/// What keeps the folder's changes from going. ONE writer: the plan's task, which writes
/// it with every plan it publishes, from the hold that is shown (`SyncPlanTask::publish_round`:
/// `SendingHold::WritesNotAllowed` and `SendingHold::Throttled`, each as the note of its name),
/// and takes it off when the sync stops (`upload_queue::publish_stopped_queue`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadQueueNote {
    /// The account may not write though its folder is read-write: why.
    WritesNotAllowed(String),
    /// OneDrive asked the account's uploads to wait: until when (Unix seconds).
    Throttled(i64),
}

impl UploadQueueNote {
    /// The note as `LastError` says it.
    pub(crate) fn text(&self) -> String {
        match self {
            // The minute the wait is over in, never one already past when it ends.
            Self::Throttled(until) => format!("OneDrive asked to slow down; uploads continue at {}", clock(until.div_euclid(60) * 60 + if until.rem_euclid(60) > 0 { 60 } else { 0 })),
            Self::WritesNotAllowed(why) => format!("nothing is uploaded: {why}"),
        }
    }
}

/// The downloads of pinned files that wait for space on this computer's disk: how many
/// and for how many bytes, with the free space of the filesystem the backing directory is
/// on and what of it the account keeps free, as they were when the downloads were held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpaceShortage {
    pub(crate) count: u32,
    pub(crate) bytes: u64,
    pub(crate) free_space: u64,
    pub(crate) reserve: u64,
}

impl DiskSpaceShortage {
    /// The shortage as `LastError` says it.
    pub(crate) fn text(&self) -> String {
        let size = crate::status::activity::human_size;
        // TODO: wording
        format!(
            "{} file(s) kept on this device wait for disk space: {} to download, {} free, {} GiB stay free",
            self.count,
            size(self.bytes),
            size(self.free_space),
            self.reserve >> 30
        )
    }
}

/// `at` (Unix seconds) as the clock on the wall shows it here, `HH:MM`.
fn clock(at: i64) -> String {
    let seconds = at as libc::time_t;
    // SAFETY: `tm` is plain data that `localtime_r` fills in; both pointers are valid for
    // the call and nothing keeps them after it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
        return "a later time".to_owned();
    }
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

/// The problems of a folder that keeps running, in the order `LastError` says them and
/// joined as it joins them: what the registration ran into, the sync's trouble, then the
/// upload queue's note, then the pinned files that wait for disk space; `None` with none.
///
/// With the sentence, its kind: [`TroubleKind::Unreachable`] only when OneDrive out of
/// reach is all that is wrong — the kind the sync's trouble carries, and no other part
/// saying anything.
pub fn running_trouble(s: &SyncSnapshot) -> Option<(String, TroubleKind)> {
    let upload_queue = s.upload_queue.note.as_ref().map(UploadQueueNote::text).unwrap_or_default();
    let (sync, kind) = s.cycle.sync_trouble.as_ref().map_or(("", TroubleKind::Other), |t| (t.text.as_str(), t.kind));
    let disk_space = s.transfers.disk_space_shortage.as_ref().map(DiskSpaceShortage::text).unwrap_or_default();
    let alone = s.folder.last_error.is_empty() && upload_queue.is_empty() && disk_space.is_empty();
    let text = joined([s.folder.last_error.as_str(), sync, upload_queue.as_str(), disk_space.as_str()]);
    (!text.is_empty()).then(|| (text, if alone { kind } else { TroubleKind::Other }))
}

/// The parts that say something, joined as `LastError` joins its problems.
fn joined<const N: usize>(parts: [&str; N]) -> String {
    parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join(". ")
}

/// `RootState` as published: the registration's state, unless the sync is blocked
/// (`error`), or an initial listing runs (`listing`).
///
/// A folder that is recorded and not up yet reads `waiting` while nothing is known to be
/// wrong.
pub fn published_state(s: &SyncSnapshot) -> &'static str {
    let blocked = s.cycle.sync_trouble.as_ref().is_some_and(|t| t.blocking);
    match s.folder.root_state {
        RootState::Ready if blocked => "error",
        RootState::Ready if s.cycle.listing => "listing",
        other => other.as_str(),
    }
}

/// `LastError` as published: the registration's text, the sync's trouble, the upload queue's
/// note, then the pinned files that wait for disk space, in that order — problems only. Where local work was moved out of the way is a
/// conflict (`Conflicts.List()`, `Conflicts.Count`), not a problem, and is not said here:
/// said here, it stayed until a Forget, and a folder that
/// ever had a conflict read as trouble for good.
pub fn published_error(s: &SyncSnapshot) -> String {
    let upload_queue = s.upload_queue.note.as_ref().map(UploadQueueNote::text).unwrap_or_default();
    let disk_space = s.transfers.disk_space_shortage.as_ref().map(DiskSpaceShortage::text).unwrap_or_default();
    joined([s.folder.last_error.as_str(), s.cycle.sync_trouble.as_ref().map_or("", |t| t.text.as_str()), upload_queue.as_str(), disk_space.as_str()])
}

/// Shared, observable sync state (see `account::state::StateHandle`, the same
/// shape for `Account`).
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

    /// [`update`](Self::update), told only when `change` changed something.
    pub fn update_if_changed(&self, change: impl FnOnce(&mut SyncSnapshot)) {
        self.tx.send_if_modified(|s| {
            let before = s.clone();
            change(s);
            *s != before
        });
    }

    /// What `read` makes of the state as it is now, without a copy of it.
    pub fn read<T>(&self, read: impl FnOnce(&SyncSnapshot) -> T) -> T {
        read(&self.tx.borrow())
    }

    /// The transfer pool's throughput, told only when it changed. With a `Retry-After`
    /// that is over, the file refused during it goes: the refusal is the wait's.
    pub(crate) fn set_throughput(&self, throughput: konedrive_graph::pool::Throughput) {
        self.tx.send_if_modified(|s| {
            let is_refusal_over = throughput.retry_after == 0 && !s.transfers.retry_after_refusal.is_empty();
            if is_refusal_over {
                s.transfers.retry_after_refusal.clear();
            }
            std::mem::replace(&mut s.transfers.throughput, throughput) != throughput || is_refusal_over
        });
    }

    /// The queue totals, told only when they changed.
    pub fn set_queue(&self, queue: totals::QueueTotals) {
        self.tx.send_if_modified(|s| std::mem::replace(&mut s.transfers.queue, queue) != queue);
    }

    /// `LiveChanges`, told only when it changed.
    pub(crate) fn set_live_changes(&self, live: LiveChanges) {
        self.tx.send_if_modified(|s| std::mem::replace(&mut s.cycle.live_changes, live) != live);
    }

    pub fn subscribe(&self) -> watch::Receiver<SyncSnapshot> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests;
