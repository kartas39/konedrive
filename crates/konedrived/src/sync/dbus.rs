//! `org.konedrive.Sync1`, one per account on the account's object
//! `/org/konedrive/Accounts/<id>`, beside its `Account1`
//! (definition: `dbus/org.konedrive.Sync1.xml`). The per-file calls are
//! `org.konedrive.Files1`'s, routed by path (`crate::accounts`).
//!
//! Follows the same split as `crate::dbus`/`crate::account`: this module is
//! the thin zbus wrapper, and `SyncService` (in `sync/mod.rs`) does the
//! actual work, so the latter can be exercised without a bus at all.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use konedrive_dbus::SYNC_INTERFACE_NAME;
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

pub struct Sync1 {
    service: Arc<SyncService>,
}

impl Sync1 {
    pub fn new(service: Arc<SyncService>) -> Self {
        Self { service }
    }
}

/// Every way `Sync1` can refuse, as a D-Bus error *name* rather than a
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
    /// `DismissConflict` of a path that names no conflict; the message
    /// names the path.
    NoConflict(String),
    /// A free-up of something "Always keep on this device" keeps here:
    /// `FreeUp` of a path a folder above it pins, or `Dehydrate` of a pinned
    /// file. The message is "<path> is pinned by <folder>: unpin it first".
    NotAllowed(String),
    /// `RegisterRoot` or `RegisterRootWithoutInterception` of a folder that
    /// is, is inside, or contains another account's folder; the message
    /// names that account's label.
    Overlaps(String),
    /// `Accounts1.Remove` of a path that names no account.
    NoAccount(String),
    /// A free-up of a file whose change waits to be uploaded (write design
    /// §3.8): freeing it up would lose that change. The message names it.
    NotUploaded(String),
    /// `UnregisterRoot`, or `Accounts1.Remove`, while changes wait to be uploaded: the folder's
    /// record holding them would go. The message says how many.
    PendingUploads(String),
    /// Everything with no name of its own: an I/O failure, mostly.
    Failed(String),
}

type Result<T> = std::result::Result<T, SyncFault>;

#[interface(name = "org.konedrive.Sync1")]
impl Sync1 {
    async fn register_root(&self, path: &str) -> Result<()> {
        self.service.register_root(Path::new(path)).await.map_err(to_fault)
    }

    /// Binds a folder with **nothing intercepting opens inside it**.
    /// Separate from `RegisterRoot`, rather than a flag on it, so
    /// that nobody enters this mode without naming it: a placeholder nobody
    /// intercepts reads as zeros, which `RootState = no-interception` and
    /// `LastError` then say in as many words.
    async fn register_root_without_interception(&self, path: &str) -> Result<()> {
        self.service
            .register_root_without_interception(Path::new(path))
            .await
            .map_err(to_fault)
    }

    async fn unregister_root(&self) -> Result<()> {
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

    /// The newest `limit` events, newest first: (unix time, kind, full path,
    /// detail).
    async fn recent_activity(&self, limit: u32) -> Result<Vec<(i64, String, String, String)>> {
        let events = self.service.recent_activity(limit).await.map_err(to_fault)?;
        Ok(events.into_iter().map(|e| (e.at, e.kind, e.path, e.detail)).collect())
    }

    /// One per event, as it is recorded; the same fields as
    /// `RecentActivity`.
    #[zbus(signal)]
    async fn activity_added(
        emitter: &SignalEmitter<'_>,
        time: i64,
        kind: &str,
        path: &str,
        detail: &str,
    ) -> zbus::Result<()>;

    /// (unix time, original full path, full path of the kept version, how it
    /// was kept: `rescued` or `copy`), newest first; one whose kept file is
    /// gone is dropped.
    async fn conflicts(&self) -> Result<Vec<(i64, String, String, String)>> {
        let rows = self.service.conflicts().await.map_err(to_fault)?;
        Ok(rows.into_iter().map(|c| (c.at, c.original, c.rescued, c.kind.as_str().to_owned())).collect())
    }

    async fn dismiss_conflict(&self, rescued_path: &str) -> Result<()> {
        self.service.dismiss_conflict(rescued_path).await.map_err(to_fault)
    }

    #[zbus(out_args("files", "bytes", "busy"))]
    async fn free_up_space(&self) -> Result<(u32, u64, u32)> {
        let freed = self.service.free_up_space().await.map_err(to_fault)?;
        Ok((freed.files, freed.bytes, freed.busy))
    }

    /// The changes waiting to be uploaded, oldest first, at most `limit` (0 for
    /// all): (seq, kind, full path, state, bytes sent, bytes in all, reason,
    /// next try).
    async fn outbox(&self, limit: u32) -> Result<Vec<(u64, String, String, String, u64, u64, String, i64)>> {
        self.service.outbox(limit).await.map_err(to_fault)
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

    #[zbus(property)]
    /// From the published state, as `RootState` and `LastError` are: a
    /// folder that could not be brought up reads
    /// `error` and still says which folder it is.
    async fn root_path(&self) -> String {
        self.service.state().get().root_path
    }

    #[zbus(property)]
    async fn root_state(&self) -> String {
        self.service.root_state()
    }

    #[zbus(property)]
    async fn last_error(&self) -> String {
        self.service.last_error()
    }

    #[zbus(property)]
    async fn root_source(&self) -> String {
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

    #[zbus(property)]
    async fn conflict_count(&self) -> u32 {
        self.service.status().2
    }

    /// Files and folders with a pin of their own.
    #[zbus(property)]
    async fn pinned_count(&self) -> u32 {
        self.service.pinned_count()
    }

    #[zbus(property)]
    async fn transfers(&self) -> Vec<(String, u64, u64)> {
        self.service.transfers()
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

    /// Uploads under way, shaped as `Transfers`.
    #[zbus(property)]
    async fn uploads(&self) -> Vec<(String, u64, u64)> {
        self.service.state().get().uploads
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

    #[zbus(property)]
    async fn ignore_patterns(&self) -> Vec<String> {
        self.service.ignore_patterns()
    }

    #[zbus(property)]
    async fn machine_name(&self) -> String {
        self.service.machine_name()
    }
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

/// Serves one account's `Sync1` at `path` and starts its signals; the tasks
/// that send them, to stop when the account goes.
///
/// At startup this runs before the bus name is claimed
/// (`crate::accounts::serve`): `main` used to claim the name, then connect
/// to the helper (up to 30 s), then attach this interface, so a
/// D-Bus-activated client calling `Sync1` in that window got
/// `UnknownInterface` from a daemon that was already on the bus. Nothing
/// that can fail, and nothing that can be slow, is left between the
/// interface and the name.
pub async fn export(
    connection: &Connection,
    path: &ObjectPath<'_>,
    service: Arc<SyncService>,
) -> zbus::Result<Vec<JoinHandle<()>>> {
    connection.object_server().at(path, Sync1::new(Arc::clone(&service))).await?;
    start_signals(connection, path, service).await
}

/// Takes one account's `Sync1` off the bus (`Accounts1.Remove`).
pub async fn unexport(connection: &Connection, path: &ObjectPath<'_>) -> zbus::Result<()> {
    connection.object_server().remove::<Sync1, _>(path).await.map(drop)
}

/// Turns `SyncService`'s state changes into `PropertiesChanged`, the same
/// `StateHandle` → `PropertiesChanged` mechanism `crate::dbus::export` uses
/// for `Account1`.
async fn start_signals(
    connection: &Connection,
    path: &ObjectPath<'_>,
    service: Arc<SyncService>,
) -> zbus::Result<Vec<JoinHandle<()>>> {
    let iface = connection.object_server().interface::<_, Sync1>(path).await?;
    // Captured before spawning (not inside the task): otherwise a state
    // change landing between attaching the interface and the task's first
    // poll would be absorbed into this baseline instead of being emitted as
    // a PropertiesChanged signal — see `crate::dbus::serve`'s identical
    // comment for `Account1`.
    let mut changes = service.state().subscribe();
    let mut previous = changes.borrow_and_update().clone();
    // A second subscription (`InterfaceRef` is `Clone`; taken before the
    // first loop moves `iface`) so the counters — and the status properties
    // of — can be coalesced on their own schedule: a listing
    // changes the counters with every page, and a download its transfer with
    // every read, far more often than `RootState`, `RootPath` and
    // `LastError` change, and neither may hold those up.
    let mut counters = service.state().subscribe();
    let mut transfers = service.report().transfers.subscribe();
    let shown = Coalesced::of(&counters.borrow_and_update(), &transfers.borrow_and_update());
    let counters_iface = iface.clone();
    // Taken here too, for the same reason as `changes`: an event recorded
    // between here and the task's first poll is still sent.
    let mut added = service.report().activity.subscribe();
    let activity_iface = iface.clone();
    let states = tokio::spawn(async move {
        while changes.changed().await.is_ok() {
            let current = changes.borrow_and_update().clone();
            if let Err(e) = emit_changes(&iface, &previous, &current).await {
                tracing::warn!("cannot emit PropertiesChanged for {SYNC_INTERFACE_NAME}: {e}");
            }
            previous = current;
        }
    });
    let coalesced = tokio::spawn(coalesce(counters, transfers, shown, move |old, new| {
        let iface = counters_iface.clone();
        async move {
            if let Err(e) = emit_coalesced(&iface, &old, &new).await {
                tracing::warn!("cannot emit PropertiesChanged for the sync counters: {e}");
            }
        }
    }));
    let activity = tokio::spawn(async move {
        loop {
            match added.recv().await {
                Ok(e) => {
                    let emitter = activity_iface.signal_emitter();
                    if let Err(err) = Sync1::activity_added(emitter, e.at, &e.kind, &e.path, &e.detail).await {
                        tracing::warn!("cannot emit ActivityAdded: {err}");
                    }
                }
                // `RecentActivity` still has them; only the live signal is lost.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!("{missed} ActivityAdded signal(s) were not sent: too many events at once");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    Ok(vec![states, coalesced, activity])
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
    transfers: Vec<(String, u64, u64)>,
    pending_count: u32,
    pending_bytes: u64,
    blocked_count: u32,
    held_count: u32,
    uploads: Vec<(String, u64, u64)>,
}

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
            transfers: transfers.values().map(|t| (t.path.clone(), t.done, t.total)).collect(),
            pending_count: s.pending_count,
            pending_bytes: s.pending_bytes,
            blocked_count: s.blocked_count,
            held_count: s.held_count,
            uploads: s.uploads.clone(),
        }
    }

    /// The properties that differ from `old`, by name, with their values now.
    fn changed_since(&self, old: &Self) -> HashMap<&'static str, zbus::zvariant::Value<'static>> {
        let mut changed: HashMap<&'static str, zbus::zvariant::Value<'static>> = HashMap::new();
        if old.items_listed != self.items_listed {
            changed.insert("ItemsListed", self.items_listed.into());
        }
        if old.items_placed != self.items_placed {
            changed.insert("ItemsPlaced", self.items_placed.into());
        }
        if old.skipped_count != self.skipped_count {
            changed.insert("SkippedCount", self.skipped_count.into());
        }
        if old.last_checked != self.last_checked {
            changed.insert("LastChecked", self.last_checked.into());
        }
        if old.local_bytes != self.local_bytes {
            changed.insert("LocalBytes", self.local_bytes.into());
        }
        if old.conflict_count != self.conflict_count {
            changed.insert("ConflictCount", self.conflict_count.into());
        }
        if old.pinned_count != self.pinned_count {
            changed.insert("PinnedCount", self.pinned_count.into());
        }
        if old.transfers != self.transfers {
            changed.insert("Transfers", self.transfers.clone().into());
        }
        if old.pending_count != self.pending_count {
            changed.insert("PendingCount", self.pending_count.into());
        }
        if old.pending_bytes != self.pending_bytes {
            changed.insert("PendingBytes", self.pending_bytes.into());
        }
        if old.blocked_count != self.blocked_count {
            changed.insert("BlockedCount", self.blocked_count.into());
        }
        if old.held_count != self.held_count {
            changed.insert("HeldCount", self.held_count.into());
        }
        if old.uploads != self.uploads {
            changed.insert("Uploads", self.uploads.clone().into());
        }
        changed
    }
}

/// Hands `emit` what changed — the value last sent and the one now — at most
/// once per [`COALESCE`]: a change during the wait is sent when it is over,
/// together with every other, as one message. Nothing is sent for a change
/// that leaves all of it as it was (a `RootState` change, say). Returns when
/// either side goes away.
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
    iface: &InterfaceRef<Sync1>,
    old: &SyncSnapshot,
    new: &SyncSnapshot,
) -> zbus::Result<()> {
    let emitter = iface.signal_emitter();
    let sync1 = iface.get().await;
    if old.root_path != new.root_path {
        sync1.root_path_changed(emitter).await?;
        // The source is decided when a folder is registered and
        // kept with it for good, so it only ever changes alongside the path.
        sync1.root_source_changed(emitter).await?;
    }
    // What is published is computed from the registration and the sync
    // together, so that is what is compared.
    if published_state(old) != published_state(new) {
        sync1.root_state_changed(emitter).await?;
    }
    if published_error(old) != published_error(new) {
        sync1.last_error_changed(emitter).await?;
    }
    // Not coalesced: a pause and a resume within one coalescing window would
    // leave a client that read in between with the pause for good.
    if old.paused_until != new.paused_until {
        sync1.paused_changed(emitter).await?;
        sync1.paused_until_changed(emitter).await?;
    }
    // `HelperState` itself is `Accounts1`'s now; a change of it shows here
    // only as the `LastError` it changes (the comparison above).
    Ok(())
}

/// As [`emit_changes`], for the counters (`ItemsListed`, `ItemsPlaced`,
/// `SkippedCount`) and the status properties (`LastChecked`, `LocalBytes`,
/// `ConflictCount`, `PinnedCount`, `Transfers`) only — kept separate so their own
/// coalescing ([`coalesce`]: at most four `PropertiesChanged` a second,
/// since a listing changes the counters with every page and a download its
/// transfer with every read) never holds up `RootState`, `RootPath` or
/// `LastError`.
///
/// Sent as a single `PropertiesChanged` signal carrying every property that
/// changed since the last tick, through `fdo::Properties::properties_changed`
/// directly rather than the per-property `*_changed` helpers each
/// property's own `#[zbus(property)]` generates: calling those separately
/// would put up to eight signals on the bus per tick — eight times the ≤4-a-
/// second asks for, not one within it.
async fn emit_coalesced(iface: &InterfaceRef<Sync1>, old: &Coalesced, new: &Coalesced) -> zbus::Result<()> {
    let changed = new.changed_since(old);
    if changed.is_empty() {
        return Ok(());
    }
    zbus::fdo::Properties::properties_changed(
        iface.signal_emitter(),
        zbus::names::InterfaceName::from_static_str(SYNC_INTERFACE_NAME)
            .expect("SYNC_INTERFACE_NAME is a valid interface name"),
        changed,
        std::borrow::Cow::Borrowed(&[]),
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::sync::activity::Transfers;
    use crate::sync::SyncStateHandle;

    /// A download moves its `Transfers` entry on with every
    /// read, and a listing the counters with every page, but what goes on
    /// the bus is at most four messages a second — each carrying everything
    /// that changed — and the last value always arrives. A hundred changes
    /// in one second, counted on the paused clock.
    #[tokio::test(start_paused = true)]
    async fn a_hundred_changes_in_a_second_are_at_most_five_messages() {
        let state = SyncStateHandle::new(SyncSnapshot::default());
        let transfers = Transfers::default();
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
        assert_eq!((last.transfers, last.items_listed), (vec![("/r/f.bin".to_owned(), 1000, 1000)], 100));

        drop(entry);
        tokio::time::sleep(COALESCE * 2).await;
        assert_eq!(sent.lock().unwrap().last().unwrap().transfers, Vec::new(), "an ended download leaves the list");
    }
}
