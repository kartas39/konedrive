//! `org.konedrive.Sync1`, on the same object as `Account1`
//! (definition: `dbus/org.konedrive.Sync1.xml`).
//!
//! Follows the same split as `crate::dbus`/`crate::account`: this module is
//! the thin zbus wrapper, and `SyncService` (in `sync/mod.rs`) does the
//! actual work, so the latter can be exercised without a bus at all.

use std::path::Path;
use std::sync::Arc;

use konedrive_dbus::{OBJECT_PATH, SYNC_INTERFACE_NAME};
use zbus::object_server::InterfaceRef;
use zbus::{interface, Connection, DBusError};

use super::{SyncError, SyncService, SyncSnapshot};

pub struct Sync1 {
    service: Arc<SyncService>,
}

impl Sync1 {
    pub fn new(service: Arc<SyncService>) -> Self {
        Self { service }
    }
}

/// Every way `Sync1` can refuse, as a D-Bus error *name* rather than a
/// sentence (spec §3.1 asks for named errors on refusals the user can act
/// on).
///
/// All of these used to collapse into `org.freedesktop.DBus.Error.Failed`
/// with the reason in the message, which leaves a client — the CLI, the
/// window, a script — nothing to branch on but English prose. "The file was
/// modified locally" and "the file is not downloaded" are exactly the two
/// refusals §3.1 has in mind: the user can do something about each, and they
/// are not the same something.
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
    /// Everything with no name of its own: an I/O failure, mostly.
    Failed(String),
}

type Result<T> = std::result::Result<T, SyncFault>;

#[interface(name = "org.konedrive.Sync1")]
impl Sync1 {
    async fn register_root(&self, path: &str) -> Result<()> {
        self.service.register_root(Path::new(path)).await.map_err(to_fault)
    }

    /// Binds a folder with **nothing intercepting opens inside it** (Ruling
    /// H105). Separate from `RegisterRoot`, rather than a flag on it, so
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

    #[zbus(property)]
    async fn root_path(&self) -> String {
        self.service.root().map(|root| root.path.display().to_string()).unwrap_or_default()
    }

    #[zbus(property)]
    async fn root_state(&self) -> String {
        self.service.root_state()
    }

    #[zbus(property)]
    async fn last_error(&self) -> String {
        self.service.last_error()
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
        SyncError::Io(_) => SyncFault::Failed(message),
    }
}

/// Serves `Sync1` through a connection *builder* — before the bus name is
/// claimed (Ruling H104).
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
    tokio::spawn(async move {
        while changes.changed().await.is_ok() {
            let current = changes.borrow_and_update().clone();
            if let Err(e) = emit_changes(&iface, &previous, &current).await {
                tracing::warn!("cannot emit PropertiesChanged for {SYNC_INTERFACE_NAME}: {e}");
            }
            previous = current;
        }
    });
    Ok(())
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
    }
    if old.root_state != new.root_state {
        sync1.root_state_changed(emitter).await?;
    }
    if old.last_error != new.last_error {
        sync1.last_error_changed(emitter).await?;
    }
    Ok(())
}
