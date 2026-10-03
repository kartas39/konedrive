use std::sync::Arc;

use tokio::sync::watch;

use crate::helper::status::HelperState;
use crate::remote::live;
use crate::status::totals;

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
    pub throughput: konedrive_graph::pool::Throughput,
    /// The pinned files waiting to download (not those under way), and their size
    /// ([`pin::Pins`]).
    pub pinned_waiting: (u32, u64),
    /// `DownloadLeftCount`, `DownloadLeftBytes`, `DownloadDoneBytes`, `DownloadTimeLeft` and
    /// the same four for uploads: counted from the rest by [`totals::run`].
    pub queue: totals::QueueTotals,
    /// `LocalScan`'s `State`, `Reason`, `Started`, `Directories`, `Files`,
    /// `Expected`, `Finished`, `Took`: the Full local scan (issue #8).
    pub scan: crate::local::scan::LocalScan,
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
            throughput: konedrive_graph::pool::Throughput::default(),
            pinned_waiting: (0, 0),
            queue: totals::QueueTotals::default(),
            scan: crate::local::scan::LocalScan::default(),
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
    pub fn set_throughput(&self, throughput: konedrive_graph::pool::Throughput) {
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

#[cfg(test)]
mod tests;
