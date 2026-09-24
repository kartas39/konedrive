//! `org.konedrive.Sync1`, on the same object as `Account1`
//! (definition: `dbus/org.konedrive.Sync1.xml`).
//!
//! Follows the same split as `crate::dbus`/`crate::account`: this module is
//! the thin zbus wrapper, and `SyncService` (in `sync/mod.rs`) does the
//! actual work, so the latter can be exercised without a bus at all.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use konedrive_dbus::{OBJECT_PATH, SYNC_INTERFACE_NAME};
use tokio::sync::{broadcast, watch};
use zbus::object_server::{InterfaceRef, SignalEmitter};
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

    async fn hydrate(&self, path: &str) -> Result<()> {
        self.service.hydrate_now(Path::new(path)).await.map_err(to_fault)
    }

    async fn dehydrate(&self, path: &str) -> Result<()> {
        self.service.dehydrate(Path::new(path)).await.map_err(to_fault)
    }

    async fn item_state(&self, path: &str) -> String {
        self.service.item_state(Path::new(path)).await
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

    /// (unix time, original full path, full path it was moved to), newest
    /// first; one whose moved file is gone is dropped.
    async fn conflicts(&self) -> Result<Vec<(i64, String, String)>> {
        let rows = self.service.conflicts().await.map_err(to_fault)?;
        Ok(rows.into_iter().map(|c| (c.at, c.original, c.rescued)).collect())
    }

    async fn dismiss_conflict(&self, rescued_path: &str) -> Result<()> {
        self.service.dismiss_conflict(rescued_path).await.map_err(to_fault)
    }

    /// "Always keep on this device" for each path; how many files were
    /// queued for download.
    #[zbus(out_args("queued"))]
    async fn pin(&self, paths: Vec<String>) -> Result<u32> {
        let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
        self.service.pin(&paths).await.map_err(to_fault)
    }

    /// Unchecking "Always keep on this device": each path's own pin comes
    /// off, and its files stay; how many pins came off.
    #[zbus(out_args("unpinned"))]
    async fn unpin(&self, paths: Vec<String>) -> Result<u32> {
        let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
        self.service.unpin(&paths).await.map_err(to_fault)
    }

    /// "Free up space" for each path, taking its own pin off first. `busy`
    /// counts the files kept because they were in use or changed here.
    #[zbus(out_args("files", "bytes", "busy", "skipped_pinned"))]
    async fn free_up(&self, paths: Vec<String>) -> Result<(u32, u64, u32, u32)> {
        let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
        let freed = self.service.free_up(&paths).await.map_err(to_fault)?;
        Ok((freed.files, freed.bytes, freed.busy + freed.modified, freed.pinned))
    }

    #[zbus(out_args("files", "bytes", "busy"))]
    async fn free_up_space(&self) -> Result<(u32, u64, u32)> {
        let freed = self.service.free_up_space().await.map_err(to_fault)?;
        Ok((freed.files, freed.bytes, freed.busy))
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

    /// The privileged helper as the daemon sees it (HS1).
    #[zbus(property)]
    async fn helper_state(&self) -> String {
        self.service.helper_state()
    }
}

/// Every refusal keeps its own name; only the ones with nothing a caller
/// could act on differently fall through to `Failed` (Ruling: fail loudly
/// rather than report success this component cannot back up).
fn to_fault(error: SyncError) -> SyncFault {
    let message = error.to_string();
    match error {
        SyncError::NotEmpty => SyncFault::NotEmpty(message),
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
        SyncError::Io(_) => SyncFault::Failed(message),
    }
}

/// Serves `Sync1` through a connection *builder* — before the bus name is
/// claimed.
///
/// `main` used to claim the name, then connect to the helper (up to 30 s),
/// then attach this interface, so a D-Bus-activated client calling `Sync1`
/// in that window got `UnknownInterface` from a daemon that was already on
/// the bus. `Account1` had the opposite discipline spelled out three lines
/// above it — "before the bus name is claimed, so a D-Bus-activated client's
/// first call is never answered from stale state" — and this follows it.
/// Nothing that can fail, and nothing that can be slow, is left between the
/// interface and the name.
pub fn add_to_builder<'a>(
    builder: zbus::connection::Builder<'a>,
    service: Arc<SyncService>,
) -> zbus::Result<zbus::connection::Builder<'a>> {
    builder.serve_at(OBJECT_PATH, Sync1::new(service))
}

/// Turns `SyncService`'s state changes into `PropertiesChanged`, the same
/// `StateHandle` → `PropertiesChanged` mechanism `crate::dbus::serve` uses
/// for `Account1`.
pub async fn start_signals(
    connection: &Connection,
    service: Arc<SyncService>,
) -> zbus::Result<()> {
    let iface = connection.object_server().interface::<_, Sync1>(OBJECT_PATH).await?;
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
    tokio::spawn(async move {
        while changes.changed().await.is_ok() {
            let current = changes.borrow_and_update().clone();
            if let Err(e) = emit_changes(&iface, &previous, &current).await {
                tracing::warn!("cannot emit PropertiesChanged for {SYNC_INTERFACE_NAME}: {e}");
            }
            previous = current;
        }
    });
    tokio::spawn(coalesce(counters, transfers, shown, move |old, new| {
        let iface = counters_iface.clone();
        async move {
            if let Err(e) = emit_coalesced(&iface, &old, &new).await {
                tracing::warn!("cannot emit PropertiesChanged for the sync counters: {e}");
            }
        }
    }));
    tokio::spawn(async move {
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
    Ok(())
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

/// Adds `Sync1` to an already-built connection and starts its signals. The
/// daemon itself goes through [`add_to_builder`] instead, so that the
/// interface is in place before the name is; this is for a connection that
/// already exists.
pub async fn attach(connection: &Connection, service: Arc<SyncService>) -> zbus::Result<()> {
    connection.object_server().at(OBJECT_PATH, Sync1::new(Arc::clone(&service))).await?;
    start_signals(connection, service).await
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
    if old.helper_state != new.helper_state {
        sync1.helper_state_changed(emitter).await?;
    }
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
