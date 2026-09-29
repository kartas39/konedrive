//! The interfaces of one account's folder — `org.konedrive.Folder`, `Transfers`,
//! `UploadQueue`, `Conflicts`, `LocalScan` and `ActivityLog` — on the account's object
//! `/org/konedrive/Accounts/<id>`, beside its `Account` (definitions: `dbus/*.xml`).
//! The per-file calls are `org.konedrive.Files`'s, routed by path (`crate::accounts`).
//!
//! Follows the same split as `crate::dbus`/`crate::account`: this module is
//! the thin zbus wrapper, and `SyncService` (in `sync/mod.rs`) does the
//! actual work, so the latter can be exercised without a bus at all.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use konedrive_dbus::{
    CONFLICTS_INTERFACE_NAME, FOLDER_INTERFACE_NAME, LOCAL_SCAN_INTERFACE_NAME, TRANSFERS_INTERFACE_NAME,
    UPLOAD_QUEUE_INTERFACE_NAME,
};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use zbus::object_server::{InterfaceRef, SignalEmitter};
use zbus::zvariant::ObjectPath;
use zbus::{interface, Connection, DBusError};

use super::activity::Transfer;
use super::{published_error, published_state, SyncError, SyncService, SyncSnapshot};

/// The shortest time between two coalesced `PropertiesChanged`: at most four
/// a second.
pub const COALESCE: Duration = Duration::from_millis(250);

/// The interfaces of one account's folder, each over the same `SyncService`
/// (definitions: `dbus/org.konedrive.{Folder,Transfers,UploadQueue,Conflicts,LocalScan,ActivityLog}.xml`).
macro_rules! over_the_service {
    ($($name:ident),*) => {$(
        pub struct $name {
            service: Arc<SyncService>,
        }

        impl $name {
            pub fn new(service: Arc<SyncService>) -> Self {
                Self { service }
            }
        }
    )*};
}

over_the_service!(Folder, Transfers, UploadQueue, Conflicts, LocalScan, ActivityLog);

/// Every way the folder's interfaces (and `Files`) can refuse, as a D-Bus error *name* rather than a
/// sentence (asks for named errors on refusals the user can act
/// on).
///
/// All of these used to collapse into `org.freedesktop.DBus.Error.Failed`
/// with the reason in the message, which leaves a client — the CLI, the
/// window, a script — nothing to branch on but English prose. "The file was
/// modified locally" and "the file is not downloaded" are exactly the two
/// refusals a named error is for: the user can do something about each, and
/// they are not the same something.
#[derive(Debug, DBusError)]
#[zbus(prefix = "org.konedrive.Error")]
pub enum SyncFault {
    /// Anything zbus itself reports, passed through unchanged.
    #[zbus(error)]
    ZBus(zbus::Error),
    NotEmpty(String),
    Unsupported(String),
    InUse(String),
    NoRoot(String),
    NoHelper(String),
    NotManaged(String),
    NotHydrated(String),
    ModifiedLocally(String),
    OutsideRoot(String),
    AlreadyRegistered(String),
    NotSignedIn(String),
    NoSource(String),
    /// `Conflicts.Dismiss` of a path that names no conflict; the message
    /// names the path.
    NoConflict(String),
    /// A free-up of something "Always keep on this device" keeps here:
    /// `FreeUp` of a path a folder above it pins, or `Dehydrate` of a pinned
    /// file. The message is "<path> is pinned by <folder>: unpin it first".
    NotAllowed(String),
    /// `Register` or `RegisterWithoutInterception` of a folder that
    /// is, is inside, or contains another account's folder; the message
    /// names that account's label.
    Overlaps(String),
    /// `Accounts.Remove` of a path that names no account.
    NoAccount(String),
    /// A free-up of a file whose change waits to be uploaded (write design
    /// §3.8): freeing it up would lose that change. The message names it.
    NotUploaded(String),
    /// `Unregister`, or `Accounts.Remove`, while changes wait to be uploaded: the folder's
    /// record holding them would go. The message says how many.
    PendingUploads(String),
    /// Everything with no name of its own: an I/O failure, mostly.
    Failed(String),
}

type Result<T> = std::result::Result<T, SyncFault>;

#[interface(name = "org.konedrive.Folder")]
impl Folder {
    async fn register(&self, path: &str) -> Result<()> {
        self.service.register_root(Path::new(path)).await.map_err(to_fault)
    }

    /// Binds a folder with **nothing intercepting opens inside it**.
    /// Separate from `Register`, rather than a flag on it, so
    /// that nobody enters this mode without naming it: a placeholder nobody
    /// intercepts reads as zeros, which `State = no-interception` and
    /// `LastError` then say in as many words.
    async fn register_without_interception(&self, path: &str) -> Result<()> {
        self.service
            .register_root_without_interception(Path::new(path))
            .await
            .map_err(to_fault)
    }

    async fn unregister(&self) -> Result<()> {
        self.service.unregister_root().await.map_err(to_fault)
    }

    /// Mirrors a local directory as placeholders. Part 2 replaces the source
    /// with the Graph listing; this stays as the offline test path.
    async fn populate_from_directory(&self, source_dir: &str) -> Result<u64> {
        self.service.populate_from_directory(Path::new(source_dir)).await.map_err(to_fault)
    }

    async fn refresh(&self) -> Result<()> {
        self.service.refresh().await.map_err(to_fault)
    }

    async fn skipped(&self) -> Result<Vec<(String, String)>> {
        self.service.skipped().await.map_err(to_fault)
    }

    #[zbus(out_args("files", "bytes", "busy"))]
    async fn free_up_space(&self) -> Result<(u32, u64, u32)> {
        let freed = self.service.free_up_space().await.map_err(to_fault)?;
        Ok((freed.files, freed.bytes, freed.busy))
    }

    /// Nothing is uploaded, and OneDrive is not asked for changes, for
    /// `seconds` — or until `Resume()` when 0.
    async fn pause(&self, seconds: u32) -> Result<()> {
        self.service.pause_syncing(seconds).await.map_err(to_fault)
    }

    async fn resume(&self) -> Result<()> {
        self.service.resume_syncing().await.map_err(to_fault)
    }

    /// The account's ignore list from now on; a Full local scan follows.
    async fn set_ignore_patterns(
        &self,
        patterns: Vec<String>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<()> {
        self.service.set_ignore_patterns(patterns).await.map_err(to_fault)?;
        self.ignore_patterns_changed(&emitter).await.map_err(SyncFault::ZBus)
    }

    /// Whether Graph's thumbnails of images and videos are fetched; written to `config.toml`.
    async fn set_thumbnails(&self, on: bool, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> Result<()> {
        self.service.change_run_settings(move |s| s.thumbnails = on).await.map_err(to_fault)?;
        self.thumbnails_changed(&emitter).await.map_err(SyncFault::ZBus)
    }

    /// Whether the account holds back on a metered connection; written to `config.toml`.
    async fn set_pause_on_metered(&self, on: bool, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> Result<()> {
        self.service.change_run_settings(move |s| s.pause_on_metered = on).await.map_err(to_fault)?;
        self.pause_on_metered_changed(&emitter).await.map_err(SyncFault::ZBus)
    }

    /// `sync`, `power-saver` or `pause`; refused `InvalidArgs` otherwise. Written to
    /// `config.toml`.
    async fn set_on_battery(&self, choice: &str, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> Result<()> {
        self.service.set_on_battery(choice).await.map_err(to_fault)?;
        self.on_battery_changed(&emitter).await.map_err(SyncFault::ZBus)
    }

    #[zbus(property)]
    async fn thumbnails(&self) -> bool {
        self.service.run_settings().thumbnails
    }

    #[zbus(property)]
    async fn pause_on_metered(&self) -> bool {
        self.service.run_settings().pause_on_metered
    }

    #[zbus(property)]
    async fn on_battery(&self) -> String {
        self.service.run_settings().on_battery.as_str().to_owned()
    }

    #[zbus(property)]
    /// From the published state, as `State` and `LastError` are: a
    /// folder that could not be brought up reads
    /// `error` and still says which folder it is.
    async fn path(&self) -> String {
        self.service.state().get().root_path
    }

    #[zbus(property)]
    async fn state(&self) -> String {
        self.service.root_state()
    }

    #[zbus(property)]
    async fn last_error(&self) -> String {
        self.service.last_error()
    }

    #[zbus(property)]
    async fn source(&self) -> String {
        self.service.root_source()
    }

    #[zbus(property)]
    async fn items_listed(&self) -> u64 {
        self.service.items().0
    }

    #[zbus(property)]
    async fn items_placed(&self) -> u64 {
        self.service.items().1
    }

    #[zbus(property)]
    async fn skipped_count(&self) -> u64 {
        self.service.items().2
    }

    #[zbus(property)]
    async fn last_checked(&self) -> i64 {
        self.service.status().0
    }

    #[zbus(property)]
    async fn local_bytes(&self) -> u64 {
        self.service.status().1
    }

    /// Files and folders with a pin of their own.
    #[zbus(property)]
    async fn pinned_count(&self) -> u32 {
        self.service.pinned_count()
    }

    #[zbus(property)]
    async fn ignore_patterns(&self) -> Vec<String> {
        self.service.ignore_patterns()
    }

    #[zbus(property)]
    async fn paused(&self) -> bool {
        self.service.state().get().paused_until.is_some()
    }

    /// Unix seconds; 0 while paused until resumed, and while not paused.
    #[zbus(property)]
    async fn paused_until(&self) -> i64 {
        self.service.state().get().paused_until.unwrap_or(0)
    }
}

#[interface(name = "org.konedrive.Transfers")]
impl Transfers {
    #[zbus(property)]
    async fn downloads(&self) -> Vec<(String, u64, u64)> {
        self.service.transfers()
    }

    /// Uploads under way, shaped as `Downloads`.
    #[zbus(property)]
    async fn uploads(&self) -> Vec<(String, u64, u64)> {
        self.service.state().get().uploads
    }

    /// Bytes a second downloaded, the average of the last 3 s.
    #[zbus(property)]
    async fn download_speed(&self) -> u64 {
        self.service.state().get().throughput.down_speed
    }

    /// Bytes a second uploaded, the average of the last 3 s.
    #[zbus(property)]
    async fn upload_speed(&self) -> u64 {
        self.service.state().get().throughput.up_speed
    }

    /// Files downloading now: the entries of `Downloads`, each file once however many
    /// streams it runs (issue #50).
    #[zbus(property)]
    async fn active_downloads(&self) -> u32 {
        u32::try_from(self.service.transfers().len()).unwrap_or(u32::MAX)
    }

    /// Files uploading now: the entries of `Uploads`.
    #[zbus(property)]
    async fn active_uploads(&self) -> u32 {
        u32::try_from(self.service.state().get().uploads.len()).unwrap_or(u32::MAX)
    }

    /// Every slot of the pool held now, all four classes, the opens' reserve included: may be
    /// above `PoolSize` (issue #50).
    #[zbus(property)]
    async fn pool_in_use(&self) -> u32 {
        self.service.state().get().throughput.in_use
    }

    /// The large files (100 MiB and up) the sync moves now, each once however many streams it
    /// runs; files being opened left out (issue #50).
    #[zbus(property)]
    async fn large_files(&self) -> u32 {
        self.service.large_files()
    }

    /// The size of the account's transfer pool now.
    #[zbus(property)]
    async fn pool_size(&self) -> u32 {
        self.service.state().get().throughput.size
    }

    /// Its ceiling (`[transfers] max` in `config.toml`).
    #[zbus(property)]
    async fn pool_ceiling(&self) -> u32 {
        self.service.state().get().throughput.ceiling
    }

    /// The streams of large sync transfers (100 MiB and up) under way now; a file being opened
    /// is never one.
    #[zbus(property)]
    async fn large_streams(&self) -> u32 {
        self.service.state().get().throughput.large
    }

    /// How many streams of large sync transfers may run at once (`[transfers] large` in
    /// `config.toml`).
    #[zbus(property)]
    async fn large_stream_limit(&self) -> u32 {
        self.service.state().get().throughput.large_limit
    }

    /// Seconds left of OneDrive's `Retry-After` wait, during which no transfer starts; 0
    /// when there is none.
    #[zbus(property)]
    async fn retry_after(&self) -> u32 {
        self.service.state().get().throughput.retry_after
    }

    /// Files left to download: the pinned files waiting and every download under way
    /// (issue #16, `sync::totals`).
    #[zbus(property)]
    async fn download_left_count(&self) -> u32 {
        self.service.state().get().queue.down.left_count
    }

    /// Their size, less what the downloads under way have received.
    #[zbus(property)]
    async fn download_left_bytes(&self) -> u64 {
        self.service.state().get().queue.down.left_bytes
    }

    /// Bytes downloaded since nothing was last left to download, or since the daemon started.
    #[zbus(property)]
    async fn download_done_bytes(&self) -> u64 {
        self.service.state().get().queue.down.done_bytes
    }

    /// Seconds the downloads left take at the last 30 s's speed; 0 when unknown.
    #[zbus(property)]
    async fn download_time_left(&self) -> u32 {
        self.service.state().get().queue.down.time_left
    }

    /// Changes left to upload: `PendingCount` less those waiting for space or too big for it.
    #[zbus(property)]
    async fn upload_left_count(&self) -> u32 {
        self.service.state().get().queue.up.left_count
    }

    /// `PendingBytes`, less what the uploads under way have sent.
    #[zbus(property)]
    async fn upload_left_bytes(&self) -> u64 {
        self.service.state().get().queue.up.left_bytes
    }

    /// Bytes uploaded since nothing was last left to upload, or since the daemon started.
    #[zbus(property)]
    async fn upload_done_bytes(&self) -> u64 {
        self.service.state().get().queue.up.done_bytes
    }

    /// Seconds the uploads left take at the last 30 s's speed; 0 when unknown, and while paused.
    #[zbus(property)]
    async fn upload_time_left(&self) -> u32 {
        self.service.state().get().queue.up.time_left
    }
}

#[interface(name = "org.konedrive.UploadQueue")]
impl UploadQueue {
    /// The changes waiting to be uploaded, oldest first, at most `limit` (0 for
    /// all): (seq, kind, full path, state, bytes sent, bytes in all, reason,
    /// next try).
    async fn changes(&self, limit: u32) -> Result<Vec<(u64, String, String, String, u64, u64, String, i64)>> {
        self.service.outbox(limit).await.map_err(to_fault)
    }

    /// The removals the mass-delete guard held go ahead; how many.
    async fn confirm_deletes(&self) -> Result<u32> {
        self.service.confirm_deletes().await.map_err(to_fault)
    }

    /// The removals the mass-delete guard held are dropped, and their items
    /// placed again; how many.
    async fn restore_deletes(&self) -> Result<u32> {
        self.service.restore_deletes().await.map_err(to_fault)
    }

    /// What stays on this computer and why: (full path, reason).
    async fn not_uploaded(&self) -> Result<Vec<(String, String)>> {
        self.service.not_uploaded().await.map_err(to_fault)
    }

    /// What is kept back, one row per reason: (group, reason, count, bytes).
    async fn not_uploaded_summary(&self) -> Result<Vec<(String, String, u32, u64)>> {
        self.service.not_uploaded_summary().await.map_err(to_fault)
    }

    /// The files kept back for one reason, at most `limit` (0 for all), and how many there are.
    #[zbus(out_args("items", "total"))]
    async fn not_uploaded_files(&self, reason: String, limit: u32) -> Result<(Vec<(String, String)>, u32)> {
        self.service.not_uploaded_files(reason, limit).await.map_err(to_fault)
    }

    /// Changes waiting to be uploaded (not blocked, not held).
    #[zbus(property)]
    async fn pending_count(&self) -> u32 {
        self.service.state().get().pending_count
    }

    /// The size of the files those changes send.
    #[zbus(property)]
    async fn pending_bytes(&self) -> u64 {
        self.service.state().get().pending_bytes
    }

    /// Changes that need the user to go up.
    #[zbus(property)]
    async fn blocked_count(&self) -> u32 {
        self.service.state().get().blocked_count
    }

    /// Removals the mass-delete guard holds for `ConfirmDeletes` or
    /// `RestoreDeletes`.
    #[zbus(property)]
    async fn held_count(&self) -> u32 {
        self.service.state().get().held_count
    }

    /// OneDrive is full: no content goes up (issue #2).
    #[zbus(property)]
    async fn quota_full(&self) -> bool {
        self.service.state().get().quota_full
    }

    #[zbus(property)]
    async fn quota_waiting_count(&self) -> u32 {
        self.service.state().get().space_waiting_count
    }

    #[zbus(property)]
    async fn quota_waiting_bytes(&self) -> u64 {
        self.service.state().get().space_waiting_bytes
    }

    #[zbus(property)]
    async fn too_big_count(&self) -> u32 {
        self.service.state().get().too_big_count
    }
}

#[interface(name = "org.konedrive.Conflicts")]
impl Conflicts {
    /// (unix time, original full path, full path of the kept version, how it
    /// was kept: `rescued` or `copy`), newest first; one whose kept file is
    /// gone is dropped.
    async fn list(&self) -> Result<Vec<(i64, String, String, String)>> {
        let rows = self.service.conflicts().await.map_err(to_fault)?;
        Ok(rows.into_iter().map(|c| (c.at, c.original, c.rescued, c.kind.as_str().to_owned())).collect())
    }

    async fn dismiss(&self, rescued_path: &str) -> Result<()> {
        self.service.dismiss_conflict(rescued_path).await.map_err(to_fault)
    }

    #[zbus(property)]
    async fn count(&self) -> u32 {
        self.service.status().2
    }

    #[zbus(property)]
    async fn machine_name(&self) -> String {
        self.service.machine_name()
    }
}

#[interface(name = "org.konedrive.LocalScan")]
impl LocalScan {
    /// The Full local scan (issue #8): `running`, `idle`, or `none` for a read-only folder.
    #[zbus(property)]
    async fn state(&self) -> String {
        self.service.state().get().scan.state.as_str().to_owned()
    }

    /// Why the running (or the last) scan runs: start, read-write, helper-back, overflow,
    /// ignore-list, periodic.
    #[zbus(property)]
    async fn reason(&self) -> String {
        self.service.state().get().scan.reason
    }

    /// Unix seconds when it started; 0 before the first.
    #[zbus(property)]
    async fn started(&self) -> i64 {
        self.service.state().get().scan.started
    }

    /// Directories it has seen so far.
    #[zbus(property)]
    async fn directories(&self) -> u64 {
        self.service.state().get().scan.directories
    }

    /// Files (and other entries that are not directories) it has seen so far.
    #[zbus(property)]
    async fn files(&self) -> u64 {
        self.service.state().get().scan.files
    }

    /// About how many items it will see: the items the base had placed when it started.
    #[zbus(property)]
    async fn expected(&self) -> u64 {
        self.service.state().get().scan.expected
    }

    /// Unix seconds when the last scan finished; 0 for none since the daemon started.
    #[zbus(property)]
    async fn finished(&self) -> i64 {
        self.service.state().get().scan.finished
    }

    /// How long the last finished scan took, in seconds.
    #[zbus(property)]
    async fn took(&self) -> u32 {
        self.service.state().get().scan.took
    }
}

#[interface(name = "org.konedrive.ActivityLog")]
impl ActivityLog {
    /// The newest `limit` events, newest first: (unix time, kind, full path,
    /// detail).
    async fn recent(&self, limit: u32) -> Result<Vec<(i64, String, String, String)>> {
        let events = self.service.recent_activity(limit).await.map_err(to_fault)?;
        Ok(events.into_iter().map(|e| (e.at, e.kind, e.path, e.detail)).collect())
    }

    /// One per event, as it is recorded; the same fields as `Recent`.
    #[zbus(signal)]
    async fn added(emitter: &SignalEmitter<'_>, time: i64, kind: &str, path: &str, detail: &str) -> zbus::Result<()>;
}

/// Every refusal keeps its own name; only the ones with nothing a caller
/// could act on differently fall through to `Failed` (Ruling: fail loudly
/// rather than report success this component cannot back up).
pub(crate) fn to_fault(error: SyncError) -> SyncFault {
    let message = error.to_string();
    match error {
        SyncError::Overlaps(_) => SyncFault::Overlaps(message),
        SyncError::NotEmpty | SyncError::ForeignFolder => SyncFault::NotEmpty(message),
        SyncError::Unsupported(_) => SyncFault::Unsupported(message),
        SyncError::InUse => SyncFault::InUse(message),
        SyncError::NoRoot => SyncFault::NoRoot(message),
        SyncError::NoHelper => SyncFault::NoHelper(message),
        SyncError::NotManaged => SyncFault::NotManaged(message),
        SyncError::NotHydrated => SyncFault::NotHydrated(message),
        SyncError::ModifiedLocally => SyncFault::ModifiedLocally(message),
        SyncError::OutsideRoot => SyncFault::OutsideRoot(message),
        SyncError::AlreadyRegistered => SyncFault::AlreadyRegistered(message),
        SyncError::NotSignedIn => SyncFault::NotSignedIn(message),
        SyncError::NoSource => SyncFault::NoSource(message),
        SyncError::NoConflict(_) => SyncFault::NoConflict(message),
        SyncError::NotAllowed(_) => SyncFault::NotAllowed(message),
        SyncError::NotUploaded(_) => SyncFault::NotUploaded(message),
        SyncError::PendingUploads(_) => SyncFault::PendingUploads(message),
        SyncError::InvalidArgs(_) => SyncFault::ZBus(zbus::Error::FDO(Box::new(zbus::fdo::Error::InvalidArgs(message)))),
        SyncError::Io(_) => SyncFault::Failed(message),
    }
}

/// Serves one account's folder — `Folder`, `Transfers`, `UploadQueue`, `Conflicts`,
/// `LocalScan` and `ActivityLog` — at `path` and starts their signals; the tasks
/// that send them, to stop when the account goes.
///
/// At startup this runs before the bus name is claimed
/// (`crate::accounts::serve`): `main` used to claim the name, then connect
/// to the helper (up to 30 s), then attach the folder's interface, so a
/// D-Bus-activated client calling it in that window got
/// `UnknownInterface` from a daemon that was already on the bus. Nothing
/// that can fail, and nothing that can be slow, is left between the
/// interfaces and the name.
pub async fn export(
    connection: &Connection,
    path: &ObjectPath<'_>,
    service: Arc<SyncService>,
) -> zbus::Result<Vec<JoinHandle<()>>> {
    let server = connection.object_server();
    server.at(path, Folder::new(Arc::clone(&service))).await?;
    server.at(path, Transfers::new(Arc::clone(&service))).await?;
    server.at(path, UploadQueue::new(Arc::clone(&service))).await?;
    server.at(path, Conflicts::new(Arc::clone(&service))).await?;
    server.at(path, LocalScan::new(Arc::clone(&service))).await?;
    server.at(path, ActivityLog::new(Arc::clone(&service))).await?;
    start_signals(connection, path, service).await
}

/// Takes one account's folder off the bus (`Accounts.Remove`).
pub async fn unexport(connection: &Connection, path: &ObjectPath<'_>) -> zbus::Result<()> {
    let server = connection.object_server();
    server.remove::<ActivityLog, _>(path).await?;
    server.remove::<LocalScan, _>(path).await?;
    server.remove::<Conflicts, _>(path).await?;
    server.remove::<UploadQueue, _>(path).await?;
    server.remove::<Transfers, _>(path).await?;
    server.remove::<Folder, _>(path).await.map(drop)
}

/// Turns `SyncService`'s state changes into `PropertiesChanged`, the same
/// `StateHandle` → `PropertiesChanged` mechanism `crate::dbus::export` uses
/// for `Account`; each under the interface that holds the property.
async fn start_signals(
    connection: &Connection,
    path: &ObjectPath<'_>,
    service: Arc<SyncService>,
) -> zbus::Result<Vec<JoinHandle<()>>> {
    let server = connection.object_server();
    let folder = server.interface::<_, Folder>(path).await?;
    let queue = server.interface::<_, UploadQueue>(path).await?;
    // Captured before spawning (not inside the task): otherwise a state
    // change landing between attaching the interfaces and the task's first
    // poll would be absorbed into this baseline instead of being emitted as
    // a PropertiesChanged signal — see `crate::dbus::serve`'s identical
    // comment for `Account`.
    let mut changes = service.state().subscribe();
    let mut previous = changes.borrow_and_update().clone();
    // A second subscription so the counters — and the status properties
    // of — can be coalesced on their own schedule: a listing
    // changes the counters with every page, and a download its transfer with
    // every read, far more often than `State`, `Path` and
    // `LastError` change, and neither may hold those up.
    let mut counters = service.state().subscribe();
    let mut transfers = service.report().transfers.subscribe();
    let shown = Coalesced::of(&counters.borrow_and_update(), &transfers.borrow_and_update());
    // Every interface of the folder is on the same object: one emitter sends for all.
    let counters_emitter = folder.signal_emitter().to_owned();
    // Taken here too, for the same reason as `changes`: an event recorded
    // between here and the task's first poll is still sent.
    let mut added = service.report().activity.subscribe();
    let activity_emitter = folder.signal_emitter().to_owned();
    // The queue totals, counted from the rest into the state (issue #16).
    let totals = tokio::spawn(super::totals::run(service.state().clone(), service.report().transfers.clone()));
    let states = tokio::spawn(async move {
        while changes.changed().await.is_ok() {
            let current = changes.borrow_and_update().clone();
            if let Err(e) = emit_changes(&folder, &queue, &previous, &current).await {
                tracing::warn!("cannot emit PropertiesChanged for the folder: {e}");
            }
            previous = current;
        }
    });
    let coalesced = tokio::spawn(coalesce(counters, transfers, shown, move |old, new| {
        let emitter = counters_emitter.clone();
        async move {
            if let Err(e) = emit_coalesced(&emitter, &old, &new).await {
                tracing::warn!("cannot emit PropertiesChanged for the sync counters: {e}");
            }
        }
    }));
    let activity = tokio::spawn(async move {
        loop {
            match added.recv().await {
                Ok(e) => {
                    if let Err(err) = ActivityLog::added(&activity_emitter, e.at, &e.kind, &e.path, &e.detail).await {
                        tracing::warn!("cannot emit ActivityLog.Added: {err}");
                    }
                }
                // `Recent` still has them; only the live signal is lost.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!("{missed} ActivityLog.Added signal(s) were not sent: too many events at once");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    Ok(vec![states, coalesced, activity, totals])
}

/// What travels in the coalesced `PropertiesChanged`: the counters (spec
/// §3.1) and the status properties, as last sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Coalesced {
    items_listed: u64,
    items_placed: u64,
    skipped_count: u64,
    last_checked: i64,
    local_bytes: u64,
    conflict_count: u32,
    pinned_count: u32,
    downloads: Vec<(String, u64, u64)>,
    pending_count: u32,
    pending_bytes: u64,
    blocked_count: u32,
    held_count: u32,
    uploads: Vec<(String, u64, u64)>,
    /// Files moving each way now, and the large files among them the sync moves (issue #50).
    active_downloads: u32,
    active_uploads: u32,
    large_files: u32,
    space_waiting_count: u32,
    space_waiting_bytes: u64,
    too_big_count: u32,
    throughput: crate::pool::Throughput,
    queue: super::totals::QueueTotals,
    scan: super::local_scan::LocalScan,
}

/// The properties that changed, by interface, then by name, with their values now.
pub(crate) type Changed = BTreeMap<&'static str, HashMap<&'static str, zbus::zvariant::Value<'static>>>;

impl Coalesced {
    fn of(s: &SyncSnapshot, transfers: &BTreeMap<u64, Transfer>) -> Self {
        Self {
            items_listed: s.items_listed,
            items_placed: s.items_placed,
            skipped_count: s.skipped_count,
            last_checked: s.last_checked,
            local_bytes: s.local_bytes,
            conflict_count: s.conflict_count,
            pinned_count: s.pinned_count,
            downloads: transfers.values().map(|t| (t.path.clone(), t.done, t.total)).collect(),
            pending_count: s.pending_count,
            pending_bytes: s.pending_bytes,
            blocked_count: s.blocked_count,
            held_count: s.held_count,
            uploads: s.uploads.clone(),
            active_downloads: u32::try_from(transfers.len()).unwrap_or(u32::MAX),
            active_uploads: u32::try_from(s.uploads.len()).unwrap_or(u32::MAX),
            large_files: super::activity::large_files(transfers, &s.uploads),
            space_waiting_count: s.space_waiting_count,
            space_waiting_bytes: s.space_waiting_bytes,
            too_big_count: s.too_big_count,
            throughput: s.throughput,
            queue: s.queue,
            scan: s.scan.clone(),
        }
    }

    /// The properties that differ from `old`, under the interface that holds each, with
    /// their values now. An interface with nothing changed is not there.
    fn changed_since(&self, old: &Self) -> Changed {
        let mut changed = Changed::new();
        let mut put = |interface: &'static str, name: &'static str, value: zbus::zvariant::Value<'static>| {
            changed.entry(interface).or_default().insert(name, value);
        };
        let folder = FOLDER_INTERFACE_NAME;
        if old.items_listed != self.items_listed {
            put(folder, "ItemsListed", self.items_listed.into());
        }
        if old.items_placed != self.items_placed {
            put(folder, "ItemsPlaced", self.items_placed.into());
        }
        if old.skipped_count != self.skipped_count {
            put(folder, "SkippedCount", self.skipped_count.into());
        }
        if old.last_checked != self.last_checked {
            put(folder, "LastChecked", self.last_checked.into());
        }
        if old.local_bytes != self.local_bytes {
            put(folder, "LocalBytes", self.local_bytes.into());
        }
        if old.pinned_count != self.pinned_count {
            put(folder, "PinnedCount", self.pinned_count.into());
        }
        if old.conflict_count != self.conflict_count {
            put(CONFLICTS_INTERFACE_NAME, "Count", self.conflict_count.into());
        }
        let queue = UPLOAD_QUEUE_INTERFACE_NAME;
        if old.pending_count != self.pending_count {
            put(queue, "PendingCount", self.pending_count.into());
        }
        if old.pending_bytes != self.pending_bytes {
            put(queue, "PendingBytes", self.pending_bytes.into());
        }
        if old.blocked_count != self.blocked_count {
            put(queue, "BlockedCount", self.blocked_count.into());
        }
        if old.held_count != self.held_count {
            put(queue, "HeldCount", self.held_count.into());
        }
        if old.space_waiting_count != self.space_waiting_count {
            put(queue, "QuotaWaitingCount", self.space_waiting_count.into());
        }
        if old.space_waiting_bytes != self.space_waiting_bytes {
            put(queue, "QuotaWaitingBytes", self.space_waiting_bytes.into());
        }
        if old.too_big_count != self.too_big_count {
            put(queue, "TooBigCount", self.too_big_count.into());
        }
        let moving = TRANSFERS_INTERFACE_NAME;
        if old.downloads != self.downloads {
            put(moving, "Downloads", self.downloads.clone().into());
        }
        if old.uploads != self.uploads {
            put(moving, "Uploads", self.uploads.clone().into());
        }
        let (was, now) = (old.throughput, self.throughput);
        for (name, before, after) in [("DownloadSpeed", was.down_speed, now.down_speed), ("UploadSpeed", was.up_speed, now.up_speed)] {
            if before != after {
                put(moving, name, after.into());
            }
        }
        for (name, before, after) in [
            ("ActiveDownloads", old.active_downloads, self.active_downloads),
            ("ActiveUploads", old.active_uploads, self.active_uploads),
            ("LargeFiles", old.large_files, self.large_files),
            ("PoolInUse", was.in_use, now.in_use),
            ("PoolSize", was.size, now.size),
            ("PoolCeiling", was.ceiling, now.ceiling),
            ("LargeStreams", was.large, now.large),
            ("LargeStreamLimit", was.large_limit, now.large_limit),
            ("RetryAfter", was.retry_after, now.retry_after),
        ] {
            if before != after {
                put(moving, name, after.into());
            }
        }
        let (was, now) = (old.queue, self.queue);
        for (name, before, after) in [
            ("DownloadLeftBytes", was.down.left_bytes, now.down.left_bytes),
            ("DownloadDoneBytes", was.down.done_bytes, now.down.done_bytes),
            ("UploadLeftBytes", was.up.left_bytes, now.up.left_bytes),
            ("UploadDoneBytes", was.up.done_bytes, now.up.done_bytes),
        ] {
            if before != after {
                put(moving, name, after.into());
            }
        }
        for (name, before, after) in [
            ("DownloadLeftCount", was.down.left_count, now.down.left_count),
            ("DownloadTimeLeft", was.down.time_left, now.down.time_left),
            ("UploadLeftCount", was.up.left_count, now.up.left_count),
            ("UploadTimeLeft", was.up.time_left, now.up.time_left),
        ] {
            if before != after {
                put(moving, name, after.into());
            }
        }
        let scan = LOCAL_SCAN_INTERFACE_NAME;
        let (was, now) = (&old.scan, &self.scan);
        if was.state != now.state {
            put(scan, "State", now.state.as_str().to_owned().into());
        }
        if was.reason != now.reason {
            put(scan, "Reason", now.reason.clone().into());
        }
        if was.started != now.started {
            put(scan, "Started", now.started.into());
        }
        if was.directories != now.directories {
            put(scan, "Directories", now.directories.into());
        }
        if was.files != now.files {
            put(scan, "Files", now.files.into());
        }
        if was.expected != now.expected {
            put(scan, "Expected", now.expected.into());
        }
        if was.finished != now.finished {
            put(scan, "Finished", now.finished.into());
        }
        if was.took != now.took {
            put(scan, "Took", now.took.into());
        }
        changed
    }
}

/// Hands `emit` what changed — the value last sent and the one now — at most
/// once per [`COALESCE`]: a change during the wait is sent when it is over,
/// together with every other, as one message per interface. Nothing is sent
/// for a change that leaves all of it as it was (a `State` change, say).
/// Returns when either side goes away.
pub(crate) async fn coalesce<F, Fut>(
    mut state: watch::Receiver<SyncSnapshot>,
    mut transfers: watch::Receiver<BTreeMap<u64, Transfer>>,
    mut shown: Coalesced,
    mut emit: F,
) where
    F: FnMut(Coalesced, Coalesced) -> Fut,
    Fut: Future<Output = ()>,
{
    loop {
        tokio::select! {
            changed = state.changed() => if changed.is_err() { return },
            changed = transfers.changed() => if changed.is_err() { return },
        }
        let now = Coalesced::of(&state.borrow_and_update(), &transfers.borrow_and_update());
        if now != shown {
            emit(shown, now.clone()).await;
            shown = now;
            tokio::time::sleep(COALESCE).await;
        }
    }
}

async fn emit_changes(
    folder: &InterfaceRef<Folder>,
    queue: &InterfaceRef<UploadQueue>,
    old: &SyncSnapshot,
    new: &SyncSnapshot,
) -> zbus::Result<()> {
    let emitter = folder.signal_emitter();
    let folder = folder.get().await;
    if old.root_path != new.root_path {
        folder.path_changed(emitter).await?;
        // The source is decided when a folder is registered and
        // kept with it for good, so it only ever changes alongside the path.
        folder.source_changed(emitter).await?;
    }
    // What is published is computed from the registration and the sync
    // together, so that is what is compared.
    if published_state(old) != published_state(new) {
        folder.state_changed(emitter).await?;
    }
    if published_error(old) != published_error(new) {
        folder.last_error_changed(emitter).await?;
    }
    // Not coalesced: a pause and a resume within one coalescing window would
    // leave a client that read in between with the pause for good.
    if old.paused_until != new.paused_until {
        folder.paused_changed(emitter).await?;
        folder.paused_until_changed(emitter).await?;
    }
    // Not coalesced either: the tray says once that OneDrive is full.
    if old.quota_full != new.quota_full {
        queue.get().await.quota_full_changed(queue.signal_emitter()).await?;
    }
    // `HelperState` itself is `Accounts`'s; a change of it shows here
    // only as the `LastError` it changes (the comparison above).
    Ok(())
}

/// As [`emit_changes`], for the counters (`ItemsListed`, `ItemsPlaced`,
/// `SkippedCount`), the status properties (`LastChecked`, `LocalBytes`,
/// `Conflicts.Count`, `PinnedCount`), the transfers and the queue — kept separate so their own
/// coalescing ([`coalesce`]: at most four `PropertiesChanged` a second per interface,
/// since a listing changes the counters with every page and a download its
/// transfer with every read) never holds up `State`, `Path` or
/// `LastError`.
///
/// Sent as one `PropertiesChanged` signal per interface carrying every property of it that
/// changed since the last tick, through `fdo::Properties::properties_changed`
/// directly rather than the per-property `*_changed` helpers each
/// property's own `#[zbus(property)]` generates: calling those separately
/// would put up to eight signals on the bus per tick — eight times the ≤4-a-
/// second asks for, not one within it.
async fn emit_coalesced(emitter: &SignalEmitter<'_>, old: &Coalesced, new: &Coalesced) -> zbus::Result<()> {
    for (interface, changed) in new.changed_since(old) {
        zbus::fdo::Properties::properties_changed(
            emitter,
            zbus::names::InterfaceName::from_static_str(interface).expect("a valid interface name"),
            changed,
            std::borrow::Cow::Borrowed(&[]),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::sync::activity::Transfers as Downloads;
    use crate::sync::SyncStateHandle;

    /// A download moves its `Downloads` entry on with every
    /// read, and a listing the counters with every page, but what goes on
    /// the bus is at most four messages a second — each carrying everything
    /// that changed — and the last value always arrives. A hundred changes
    /// in one second, counted on the paused clock.
    #[tokio::test(start_paused = true)]
    async fn a_hundred_changes_in_a_second_are_at_most_five_messages() {
        let state = SyncStateHandle::new(SyncSnapshot::default());
        let transfers = Downloads::default();
        let sent: Arc<Mutex<Vec<Coalesced>>> = Arc::default();
        let log = Arc::clone(&sent);
        tokio::spawn(coalesce(state.subscribe(), transfers.subscribe(), Coalesced::default(), move |_, new| {
            log.lock().unwrap().push(new);
            std::future::ready(())
        }));

        let entry = transfers.start("/r/f.bin".into(), 1000);
        for step in 1..=100u64 {
            entry.progress(step * 10, 1000);
            if step % 10 == 0 {
                state.update(|s| s.items_listed = step);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let messages = sent.lock().unwrap().len();
        assert!((2..=5).contains(&messages), "{messages} messages for 100 changes in one second");

        tokio::time::sleep(COALESCE * 2).await;
        let last = sent.lock().unwrap().last().cloned().unwrap();
        assert_eq!((last.downloads, last.items_listed), (vec![("/r/f.bin".to_owned(), 1000, 1000)], 100));

        drop(entry);
        tokio::time::sleep(COALESCE * 2).await;
        assert_eq!(sent.lock().unwrap().last().unwrap().downloads, Vec::new(), "an ended download leaves the list");
    }
}
