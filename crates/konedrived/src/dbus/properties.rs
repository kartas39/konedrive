//! The properties of one account's folder that the daemon announces by itself, in one table
//! (quality finding `SY10`): under which interface and name each goes out, and how it is read
//! from the published state. The getter of a property answers through its row
//! ([`Property::of`]), and `signals` compares and sends by the same rows, so a property
//! cannot be read one way and announced another.
//!
//! Not here: the properties a call sets and announces itself (`IgnorePatterns`,
//! `Thumbnails`), `Source`, which is the folder's record and is announced with `Path`, and
//! `MachineName`, which nothing changes while the daemon runs.

use std::collections::{BTreeMap, HashMap};

use konedrive_dbus::rows;
use konedrive_dbus::{CONFLICTS_INTERFACE_NAME, FOLDER_INTERFACE_NAME, LOCAL_SCAN_INTERFACE_NAME, TRANSFERS_INTERFACE_NAME, UPLOAD_QUEUE_INTERFACE_NAME};
use zbus::zvariant::Value;

use crate::status::snapshot::{published_error, published_state, SyncSnapshot};
use crate::status::transfers::{large_files, Transfer};
use crate::sync::SyncService;

/// What the properties are read from: the published state and the downloads under way.
#[derive(Debug, Clone, Default)]
pub(crate) struct Seen {
    pub snapshot: SyncSnapshot,
    pub downloads: BTreeMap<u64, Transfer>,
}

impl Seen {
    /// As `service` publishes it now.
    pub(crate) fn of(service: &SyncService) -> Self {
        Self { snapshot: service.state().get(), downloads: service.report().transfers.subscribe().borrow().clone() }
    }
}

/// One property: where it is on the bus, and its value.
pub(crate) struct Property<T> {
    pub interface: &'static str,
    pub name: &'static str,
    read: fn(&Seen) -> T,
}

impl<T> Property<T> {
    /// Its value as `service` publishes it now: what the property's getter answers.
    pub(crate) fn of(&self, service: &SyncService) -> T {
        (self.read)(&Seen::of(service))
    }
}

/// A [`Property`] of any type, for the code that announces.
pub(crate) trait Announced: Sync {
    fn interface(&self) -> &'static str;
    fn name(&self) -> &'static str;
    /// The value in `new`, when it is not the one in `old`.
    fn changed(&self, old: &Seen, new: &Seen) -> Option<Value<'static>>;
}

impl<T: PartialEq + Into<Value<'static>>> Announced for Property<T> {
    fn interface(&self) -> &'static str {
        self.interface
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn changed(&self, old: &Seen, new: &Seen) -> Option<Value<'static>> {
        let now = (self.read)(new);
        ((self.read)(old) != now).then(|| now.into())
    }
}

/// The properties that changed, by interface, then by name, with their values now.
pub(crate) type Changed = BTreeMap<&'static str, HashMap<&'static str, Value<'static>>>;

/// The properties of `list` whose value in `new` is not the one in `old`. An interface with
/// nothing changed is not there.
pub(crate) fn changed(list: &[&dyn Announced], old: &Seen, new: &Seen) -> Changed {
    let mut changed = Changed::new();
    for property in list {
        if let Some(value) = property.changed(old, new) {
            changed.entry(property.interface()).or_default().insert(property.name(), value);
        }
    }
    changed
}

/// Writes each row as a constant, and the list of them all.
macro_rules! properties {
    ($(#[$about:meta])* $list:ident: $($(#[$doc:meta])* $id:ident: $ty:ty = $interface:expr, $name:literal, $read:expr;)*) => {
        $($(#[$doc])* pub(crate) const $id: Property<$ty> = Property { interface: $interface, name: $name, read: $read };)*
        $(#[$about])*
        pub(crate) static $list: &[&dyn Announced] = &[$(&$id),*];
    };
}

const FOLDER: &str = FOLDER_INTERFACE_NAME;
const QUEUE: &str = UPLOAD_QUEUE_INTERFACE_NAME;
const MOVING: &str = TRANSFERS_INTERFACE_NAME;
const SCAN: &str = LOCAL_SCAN_INTERFACE_NAME;

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

properties! {
    /// Announced the moment they change, each change by itself: a client that reads between
    /// two changes a coalescing window apart would keep the first for good (a pause and its
    /// resume), and the tray says once that OneDrive is full.
    AT_ONCE:
    /// From the published state, as `State` and `LastError` are: a folder that could not be
    /// brought up reads `error` and still says which folder it is.
    PATH: String = FOLDER, "Path", |s| s.snapshot.folder.root_path.clone();
    /// Worked out from the registration and the sync together (`published_state`).
    STATE: String = FOLDER, "State", |s| published_state(&s.snapshot).to_owned();
    LAST_ERROR: String = FOLDER, "LastError", |s| published_error(&s.snapshot);
    PAUSED: bool = FOLDER, "Paused", |s| s.snapshot.pause.paused_until.is_some();
    /// Unix seconds; 0 while paused until resumed, and while not paused.
    PAUSED_UNTIL: i64 = FOLDER, "PausedUntil", |s| s.snapshot.pause.paused_until.unwrap_or(0);
    /// Why the account holds back by itself now: `metered`, `on-battery`, `power-saver`, or
    /// empty.
    HELD_BACK: String = FOLDER, "HeldBack", |s| s.snapshot.pause.held_back.clone();
    /// Whether what is changed in the folder is uploaded now: false for a folder that runs
    /// read-only although its account is read-write.
    WRITABLE: bool = FOLDER, "Writable", |s| s.snapshot.folder.writable;
    /// How changes made in OneDrive reach this computer: `connected` (at once, through the
    /// notification socket), `connecting` (trying; the poll runs meanwhile), or `off`
    /// (stopped, or not a OneDrive folder).
    LIVE_CHANGES: String = FOLDER, "LiveChanges", |s| s.snapshot.cycle.live_changes.as_str().to_owned();
    /// OneDrive is full: no content goes up.
    QUOTA_FULL: bool = QUEUE, "QuotaFull", |s| s.snapshot.outbox.quota_full;
}

properties! {
    /// Announced at most four times a second, everything that changed of an interface in
    /// one message: a listing changes the counters with every page, and a download its
    /// entry with every read.
    COALESCED:
    ITEMS_LISTED: u64 = FOLDER, "ItemsListed", |s| s.snapshot.cycle.items_listed;
    ITEMS_PLACED: u64 = FOLDER, "ItemsPlaced", |s| s.snapshot.cycle.items_placed;
    SKIPPED_COUNT: u64 = FOLDER, "SkippedCount", |s| s.snapshot.cycle.skipped_count;
    /// Unix seconds of the last cycle that succeeded; 0 for never.
    LAST_CHECKED: i64 = FOLDER, "LastChecked", |s| s.snapshot.cycle.last_checked;
    LOCAL_BYTES: u64 = FOLDER, "LocalBytes", |s| s.snapshot.local.local_bytes;
    /// Files and folders with a pin of their own.
    PINNED_COUNT: u32 = FOLDER, "PinnedCount", |s| s.snapshot.local.pinned_count;
    /// Conflicts whose kept file is still there.
    CONFLICT_COUNT: u32 = CONFLICTS_INTERFACE_NAME, "Count", |s| s.snapshot.local.conflict_count;

    /// Changes waiting to be uploaded (not blocked, not held).
    PENDING_COUNT: u32 = QUEUE, "PendingCount", |s| s.snapshot.outbox.pending_count;
    /// The size of the files those changes send.
    PENDING_BYTES: u64 = QUEUE, "PendingBytes", |s| s.snapshot.outbox.pending_bytes;
    /// Changes that need the user to go up.
    BLOCKED_COUNT: u32 = QUEUE, "BlockedCount", |s| s.snapshot.outbox.blocked_count;
    /// Removals the mass-delete guard holds for `ConfirmDeletes` or `RestoreDeletes`.
    HELD_COUNT: u32 = QUEUE, "HeldCount", |s| s.snapshot.outbox.held_count;
    QUOTA_WAITING_COUNT: u32 = QUEUE, "QuotaWaitingCount", |s| s.snapshot.outbox.space_waiting_count;
    QUOTA_WAITING_BYTES: u64 = QUEUE, "QuotaWaitingBytes", |s| s.snapshot.outbox.space_waiting_bytes;
    TOO_BIG_COUNT: u32 = QUEUE, "TooBigCount", |s| s.snapshot.outbox.too_big_count;

    /// Every download under way.
    DOWNLOADS: Vec<rows::Transfer> = MOVING, "Downloads",
        |s| s.downloads.values().map(|t| rows::Transfer { path: t.path.clone(), done: t.done, total: t.total }).collect();
    /// Uploads under way, shaped as `Downloads`.
    UPLOADS: Vec<rows::Transfer> = MOVING, "Uploads",
        |s| s.snapshot.outbox.uploads.iter().map(|(path, done, total)| rows::Transfer { path: path.clone(), done: *done, total: *total }).collect();
    /// Bytes a second downloaded, the average of the last 3 s.
    DOWNLOAD_SPEED: u64 = MOVING, "DownloadSpeed", |s| s.snapshot.transfers.throughput.down_speed;
    /// Bytes a second uploaded, the average of the last 3 s.
    UPLOAD_SPEED: u64 = MOVING, "UploadSpeed", |s| s.snapshot.transfers.throughput.up_speed;
    /// Files downloading now: the entries of `Downloads`, each file once however many
    /// streams it runs.
    ACTIVE_DOWNLOADS: u32 = MOVING, "ActiveDownloads", |s| count(s.downloads.len());
    /// Files uploading now: the entries of `Uploads`.
    ACTIVE_UPLOADS: u32 = MOVING, "ActiveUploads", |s| count(s.snapshot.outbox.uploads.len());
    /// The large files (100 MiB and up) the sync moves now, each once however many streams
    /// it runs; files being opened left out.
    LARGE_FILES: u32 = MOVING, "LargeFiles", |s| large_files(&s.downloads, &s.snapshot.outbox.uploads);
    /// Every slot of the pool held now, all four classes, the opens' reserve included: may
    /// be above `PoolSize`.
    POOL_IN_USE: u32 = MOVING, "PoolInUse", |s| s.snapshot.transfers.throughput.in_use;
    /// The size of the account's transfer pool now.
    POOL_SIZE: u32 = MOVING, "PoolSize", |s| s.snapshot.transfers.throughput.size;
    /// Its ceiling (`[transfers] max` in `config.toml`).
    POOL_CEILING: u32 = MOVING, "PoolCeiling", |s| s.snapshot.transfers.throughput.ceiling;
    /// The streams of large sync transfers (100 MiB and up) under way now; a file being
    /// opened is never one.
    LARGE_STREAMS: u32 = MOVING, "LargeStreams", |s| s.snapshot.transfers.throughput.large;
    /// How many streams of large sync transfers may run at once (`[transfers] large` in
    /// `config.toml`).
    LARGE_STREAM_LIMIT: u32 = MOVING, "LargeStreamLimit", |s| s.snapshot.transfers.throughput.large_limit;
    /// Seconds left of OneDrive's `Retry-After` wait, during which no transfer starts; 0
    /// when there is none.
    RETRY_AFTER: u32 = MOVING, "RetryAfter", |s| s.snapshot.transfers.throughput.retry_after;
    /// Files left to download: the pinned files waiting and every download under way
    /// (`status::totals`).
    DOWNLOAD_LEFT_COUNT: u32 = MOVING, "DownloadLeftCount", |s| s.snapshot.transfers.queue.down.left_count;
    /// Their size, less what the downloads under way have received.
    DOWNLOAD_LEFT_BYTES: u64 = MOVING, "DownloadLeftBytes", |s| s.snapshot.transfers.queue.down.left_bytes;
    /// Bytes downloaded since nothing was last left to download, or since the daemon started.
    DOWNLOAD_DONE_BYTES: u64 = MOVING, "DownloadDoneBytes", |s| s.snapshot.transfers.queue.down.done_bytes;
    /// Seconds the downloads left take at the last 30 s's speed; 0 when unknown.
    DOWNLOAD_TIME_LEFT: u32 = MOVING, "DownloadTimeLeft", |s| s.snapshot.transfers.queue.down.time_left;
    /// Changes left to upload: `PendingCount` less those waiting for space or too big for it.
    UPLOAD_LEFT_COUNT: u32 = MOVING, "UploadLeftCount", |s| s.snapshot.transfers.queue.up.left_count;
    /// `PendingBytes`, less what the uploads under way have sent.
    UPLOAD_LEFT_BYTES: u64 = MOVING, "UploadLeftBytes", |s| s.snapshot.transfers.queue.up.left_bytes;
    /// Bytes uploaded since nothing was last left to upload, or since the daemon started.
    UPLOAD_DONE_BYTES: u64 = MOVING, "UploadDoneBytes", |s| s.snapshot.transfers.queue.up.done_bytes;
    /// Seconds the uploads left take at the last 30 s's speed; 0 when unknown, and while paused.
    UPLOAD_TIME_LEFT: u32 = MOVING, "UploadTimeLeft", |s| s.snapshot.transfers.queue.up.time_left;

    /// The Full local scan: `running`, `idle`, or `none` for a read-only folder.
    SCAN_STATE: String = SCAN, "State", |s| s.snapshot.local.scan.state.as_str().to_owned();
    /// Why the running (or the last) scan runs: start, read-write, helper-back, overflow,
    /// ignore-list, periodic.
    SCAN_REASON: String = SCAN, "Reason", |s| s.snapshot.local.scan.reason.clone();
    /// Unix seconds when it started; 0 before the first.
    SCAN_STARTED: i64 = SCAN, "Started", |s| s.snapshot.local.scan.started;
    /// Directories it has seen so far.
    SCAN_DIRECTORIES: u64 = SCAN, "Directories", |s| s.snapshot.local.scan.directories;
    /// Files (and other entries that are not directories) it has seen so far.
    SCAN_FILES: u64 = SCAN, "Files", |s| s.snapshot.local.scan.files;
    /// About how many items it will see: the items the base had placed when it started.
    SCAN_EXPECTED: u64 = SCAN, "Expected", |s| s.snapshot.local.scan.expected;
    /// Unix seconds when the last scan finished; 0 for none since the daemon started.
    SCAN_FINISHED: i64 = SCAN, "Finished", |s| s.snapshot.local.scan.finished;
    /// How long the last finished scan took, in seconds.
    SCAN_TOOK: u32 = SCAN, "Took", |s| s.snapshot.local.scan.took;
}

#[cfg(test)]
mod tests;
