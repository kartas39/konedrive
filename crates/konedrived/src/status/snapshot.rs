use std::sync::Arc;

use tokio::sync::watch;

use crate::helper::status::HelperState;
use crate::config::Mode;
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
pub enum ScanState {
    /// A read-only folder: no watcher, no local scan.
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

/// The folder's local scan, as `LocalScan`'s properties publish it. While idle, the
/// reason, the start and the counts are the last scan's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalScan {
    pub state: ScanState,
    /// Empty before the first scan.
    pub reason: String,
    /// Unix seconds; 0 before the first scan.
    pub started: i64,
    /// Directories and other entries seen so far, the root not counted.
    pub directories: u64,
    pub files: u64,
    /// Items the base has placed in the folder when the scan started: about how many it
    /// will see. The disk's own count is not known in advance.
    pub expected: u64,
    /// Unix seconds when the last scan finished; 0 for none since the daemon started.
    pub finished: i64,
    /// How long the last finished scan took, in seconds.
    pub took: u32,
}

impl LocalScan {
    /// Follows the folder's mode: read-only has no scan; read-write is idle until one runs.
    pub fn follow(&mut self, mode: Mode) {
        self.state = match (mode, self.state) {
            (Mode::ReadOnly, _) => ScanState::None,
            (Mode::ReadWrite, ScanState::None) => ScanState::Idle,
            (Mode::ReadWrite, state) => state,
        };
    }
}

/// What `Folder.State` reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RootState {
    /// No root is registered.
    #[default]
    None,
    /// A root is registered and, as far as this daemon knows, healthy.
    Ready,
    /// A root is registered, but **nothing intercepts opens inside it**
    ///: it was registered through
    /// `RegisterWithoutInterception`, so a placeholder nobody fills
    /// reads as zeros until it is hydrated by hand. Distinct from `ready`
    /// precisely because a client must be able to tell the two apart. A
    /// folder registered that way because no helper was connected leaves
    /// this state when one connects (`sync/bring_up.rs`, `switch`).
    NoInterception,
    /// A folder is recorded and not up yet, and nothing is known to be wrong: it is being
    /// brought up, or waits for the helper to connect (the start of a session). Published
    /// as `waiting`, or as `error` once the helper is known to be missing, stopped or
    /// failed (`published_state`).
    Waiting,
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
    pub outbox: OutboxStatus,
    pub pause: PauseStatus,
    pub transfers: TransferStatus,
}

/// The folder's registration and the helper it needs. All but
/// [`helper_state`](Self::helper_state) are worked out from the folder's
/// state in one place (`sync::publish`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderStatus {
    pub root_path: String,
    pub root_state: RootState,
    /// What the registration ran into.
    pub last_error: String,
    /// Why a folder registered without the helper could not be switched to interception
    /// once the helper connected: said right behind `last_error`, and gone with it.
    pub switch_note: Option<SwitchNote>,
    /// `HelperState` (HS1).
    pub helper_state: HelperState,
    /// The registered folder needs the helper and does not have it (HS2,
    /// HS3): a folder with interception whose link is down, or one that
    /// shows OneDrive and is not intercepted yet. `RootState` reads `error`
    /// then, and `LastError` begins with what [`HelperState::advice`] says.
    /// A cycle that finds no helper makes sure of it (`remote::listing`).
    pub waits_for_helper: bool,
}

/// What the cycles with OneDrive say (`remote::listing`, `remote::live`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CycleStatus {
    /// An initial or `410` listing of the drive is running.
    pub listing: bool,
    pub items_listed: u64,
    pub items_placed: u64,
    pub skipped_count: u64,
    /// What the folder's sync last ran into; `None` once a cycle succeeds.
    pub sync_trouble: Option<SyncTrouble>,
    /// Why files changed in the cloud are not updated here yet; `None` when
    /// none waits.
    pub replacement_note: Option<ReplacementNote>,
    /// `LastChecked`: unix seconds of the last cycle that
    /// succeeded, 0 for never.
    pub last_checked: i64,
    /// `LiveChanges`: whether changes made in OneDrive arrive at once, through the
    /// notification socket (`remote::live`).
    pub live_changes: LiveChanges,
}

/// What is on this computer: the space, the conflicts, the pins, and what
/// the watcher of a read-write folder says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalStatus {
    /// `LocalBytes`: what the folder's files take on disk, as last measured
    /// (`status::space::LocalSpace`).
    pub local_bytes: u64,
    /// `Conflicts.Count`: conflicts whose rescued file is still there.
    pub conflict_count: u32,
    /// `PinnedCount`: files and folders with a pin of their own
    /// (`hydration::pin::Pins`).
    pub pinned_count: u32,
    /// The pinned files waiting to download (not those under way), and their size
    /// (`hydration::pin::Pins`).
    pub pinned_waiting: (u32, u64),
    /// What the watcher of a read-write folder says while it runs: that part
    /// of the folder is found only by a periodic scan, or that new folders
    /// wait for the helper's mark (`local::watcher::WatchStatus::note`). Empty
    /// otherwise. The folder moved or deleted is said in `last_error`, since
    /// it outlasts the watcher.
    pub watch_note: String,
    /// The folder's filesystem changed since its file handles were recorded,
    /// and they were taken again: said until the next Full local scan
    /// that finds them current. Empty otherwise.
    pub handles_note: String,
    /// `LocalScan`'s `State`, `Reason`, `Started`, `Directories`, `Files`,
    /// `Expected`, `Finished`, `Took`: the Full local scan (issue #8).
    pub scan: LocalScan,
}

/// The outbox as its worker last saw it (`sync/outbox.rs`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboxStatus {
    /// What keeps the outbox's changes from going: the write gate
    /// closed under a read-write folder, OneDrive's throttle of its uploads, a folder the
    /// outbox worker cannot open, or a read-only
    /// folder whose sync holds its cycles while changes wait. `None` otherwise.
    pub note: Option<OutboxNote>,
    /// `PendingCount`, `PendingBytes`, `BlockedCount`.
    pub pending_count: u32,
    pub pending_bytes: u64,
    pub blocked_count: u32,
    /// `HeldCount`: removals the mass-delete guard holds for `ConfirmDeletes`
    /// or `RestoreDeletes` (the outbox on the bus).
    pub held_count: u32,
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
}

/// Whether the account's background work stops (`sync/pause.rs`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PauseStatus {
    /// `Paused` and `PausedUntil`: `Some(until)` while paused, unix seconds,
    /// 0 meaning until resumed.
    pub paused_until: Option<i64>,
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
    /// `LargeFiles` count the files of `Transfers.Downloads` and `Uploads` instead (issue #50).
    pub throughput: konedrive_graph::pool::Throughput,
    /// `DownloadLeftCount`, `DownloadLeftBytes`, `DownloadDoneBytes`, `DownloadTimeLeft` and
    /// the same four for uploads: counted from the rest by [`totals::run`].
    pub queue: totals::QueueTotals,
}

impl SyncSnapshot {
    /// Whether the account's background work stops: paused by the user, or held back.
    pub fn stopped(&self) -> bool {
        self.pause.paused_until.is_some() || !self.pause.held_back.is_empty()
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

/// Why files that changed in OneDrive are not updated here yet: their
/// replacements failed, and are tried again after every cycle
/// (`remote::listing::replacements`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacementNote {
    /// How many files wait.
    pub files: usize,
    /// What the newest of the failures said.
    pub why: String,
}

impl ReplacementNote {
    /// The note as `LastError` says it.
    pub fn text(&self) -> String {
        format!("{} file(s) changed in OneDrive could not be updated here yet: {}", self.files, self.why)
    }
}

/// Why a switch to interception did not go through (`sync/bring_up.rs`, `switch`); the folder
/// stays as it was, and the next connect tries again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchNote {
    pub why: String,
}

impl SwitchNote {
    /// The note as `LastError` says it.
    pub fn text(&self) -> String {
        format!(
            "the konedrive helper is connected, but switching this folder to interception failed: {}; it is \
             tried again the next time the helper connects",
            self.why
        )
    }
}

/// What keeps the outbox's changes from going, and who says so: the write gate's note is
/// the gate's alone to take back (`SyncService::write_gate`), the throttle's and the
/// unopenable folder's are the outbox worker's (its host, `sync/outbox.rs`), the other two are the poller's
/// (`remote::listing::poller`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxNote {
    /// The write gate is closed under a read-write folder: why.
    GateClosed(String),
    /// A read-only folder holds changes waiting to upload, so its cycles wait: how many.
    HeldBack(usize),
    /// A read-only folder whose waiting changes cannot be read.
    Unreadable,
    /// OneDrive asked the account's uploads to wait: until when (Unix seconds).
    Throttled(i64),
    /// The outbox worker cannot open the folder, which is still there: the error.
    FolderClosed(String),
}

/// A throttle shorter than this many seconds is not said ([`OutboxNote::after_worker`]).
pub const THROTTLE_SAID: i64 = 5;

impl OutboxNote {
    /// What the write gate makes of the note `shown`, the gate being closed for `refusal`
    /// or open: the note to show instead, or `None` when nothing changes. The gate says
    /// why it is closed over whatever is shown, and takes back only its own note.
    pub fn after_gate(shown: &Option<Self>, refusal: Option<&str>) -> Option<Option<Self>> {
        let note = refusal.map(|why| Self::GateClosed(why.to_owned()));
        let own = matches!(shown, Some(Self::GateClosed(_)));
        (*shown != note && (refusal.is_some() || own)).then_some(note)
    }

    /// What the outbox worker's state makes of the note `shown` at `now`: its folder
    /// cannot be opened (`folder`, the error), OneDrive asked its uploads to wait until
    /// `until`, or neither. The note to show instead, or `None` when nothing changes.
    ///
    /// - The worker's notes are said only where nothing else is, and the worker takes back
    ///   only its own: the gate's and the poller's stay.
    /// - The folder's note stands over the throttle's.
    /// - A throttle is said only when it begins [`THROTTLE_SAID`] seconds or more before its
    ///   end: a shorter wait is over before anyone could read of it. Once said, it stays
    ///   until the wait is over.
    pub fn after_worker(shown: &Option<Self>, folder: Option<&str>, until: Option<i64>, now: i64) -> Option<Option<Self>> {
        let own = matches!(shown, None | Some(Self::Throttled(_) | Self::FolderClosed(_)));
        let throttle = until.map(Self::Throttled).filter(|note| shown.as_ref() == Some(note) || until.is_some_and(|at| at - now >= THROTTLE_SAID));
        let note = folder.map(|error| Self::FolderClosed(error.to_owned())).or(throttle);
        (own && *shown != note).then_some(note)
    }

    /// The note as `LastError` says it.
    pub fn text(&self) -> String {
        match self {
            // The minute the wait is over in, never one already past when it ends.
            Self::Throttled(until) => format!("OneDrive asked to slow down; uploads continue at {}", clock(until.div_euclid(60) * 60 + if until.rem_euclid(60) > 0 { 60 } else { 0 })),
            Self::FolderClosed(error) => format!("the folder cannot be opened ({error}); uploads wait"),
            Self::GateClosed(why) => format!("nothing is uploaded: {why}"),
            Self::HeldBack(n) => format!(
                "{n} change(s) made here wait to be uploaded, so the folder is not kept in step with \
                 OneDrive: they go once the account is read-write again, or are dropped by a forced \
                 switch to read-only"
            ),
            Self::Unreadable => {
                "the changes waiting to be uploaded cannot be read, so the folder is not kept in step with OneDrive".to_owned()
            }
        }
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

/// What the registration says in `LastError`: its error, and behind it the note of a
/// failed switch.
fn registration_error(s: &SyncSnapshot) -> String {
    let Some(note) = &s.folder.switch_note else { return s.folder.last_error.clone() };
    let before = s.folder.last_error.trim_end_matches(". ");
    if before.is_empty() {
        note.text()
    } else {
        format!("{before}. {}", note.text())
    }
}

/// `RootState` as published: the registration's state, unless
/// the folder waits for the helper or the sync is blocked (`error`), or an
/// initial listing runs (`listing`).
///
/// A folder that is recorded and not up yet reads `waiting` while nothing is known to be
/// wrong, and `error` once the helper it waits for is known to be missing, stopped or
/// failed.
///
/// `listing` stands only for `ready`: a folder
/// without interception keeps saying `no-interception`, the one word that
/// warns its files read as zeros. Since HS2 such a folder never lists
/// anyway — it is local, or it shows OneDrive and waits for the helper.
pub fn published_state(s: &SyncSnapshot) -> &'static str {
    let blocked = s.folder.waits_for_helper || s.cycle.sync_trouble.as_ref().is_some_and(|t| t.blocking);
    match s.folder.root_state {
        RootState::Ready | RootState::NoInterception if blocked => "error",
        RootState::Ready if s.cycle.listing => "listing",
        RootState::Waiting if s.folder.waits_for_helper && s.folder.helper_state.known_down() => "error",
        other => other.as_str(),
    }
}

/// `LastError` as published: what the helper's absence means, the
/// registration's text with the note of a failed switch, the sync's and the
/// replacement note, then the watcher's, the handles' and the outbox's notes, in
/// that order — problems only. Where local work was moved out of the way is a conflict
/// (`Conflicts.List()`, `Conflicts.Count`), not a problem, and is not said here
///: said here, it stayed until a Forget, and a folder that
/// ever had a conflict read as trouble for good.
///
/// The helper's part (HS3) is worked out from `HelperState` whenever that
/// changes, never frozen when the link dropped: "not running" becomes
/// "failed" when systemd says so.
pub fn published_error(s: &SyncSnapshot) -> String {
    // A folder that only waits, with nothing known to be wrong, says nothing of the helper.
    let calm = s.folder.root_state == RootState::Waiting && !s.folder.helper_state.known_down();
    let helper = if s.folder.waits_for_helper && !calm { s.folder.helper_state.advice().unwrap_or("") } else { "" };
    let registration = registration_error(s);
    let outbox = s.outbox.note.as_ref().map(OutboxNote::text).unwrap_or_default();
    let replacement = s.cycle.replacement_note.as_ref().map(ReplacementNote::text).unwrap_or_default();
    [
        helper,
        registration.as_str(),
        s.cycle.sync_trouble.as_ref().map_or("", |t| t.text.as_str()),
        replacement.as_str(),
        s.local.watch_note.as_str(),
        s.local.handles_note.as_str(),
        outbox.as_str(),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(". ")
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

    /// The transfer pool's throughput, told only when it changed.
    pub fn set_throughput(&self, throughput: konedrive_graph::pool::Throughput) {
        self.tx.send_if_modified(|s| std::mem::replace(&mut s.transfers.throughput, throughput) != throughput);
    }

    /// The queue totals, told only when they changed.
    pub fn set_queue(&self, queue: totals::QueueTotals) {
        self.tx.send_if_modified(|s| std::mem::replace(&mut s.transfers.queue, queue) != queue);
    }

    /// `LiveChanges`, told only when it changed.
    pub fn set_live_changes(&self, live: LiveChanges) {
        self.tx.send_if_modified(|s| std::mem::replace(&mut s.cycle.live_changes, live) != live);
    }

    pub fn subscribe(&self) -> watch::Receiver<SyncSnapshot> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests;
