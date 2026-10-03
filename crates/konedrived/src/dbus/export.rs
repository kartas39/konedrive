use std::sync::Arc;

use async_trait::async_trait;
use konedrive_dbus::ACCOUNTS_PATH;
use tokio::task::JoinHandle;
use zbus::object_server::InterfaceRef;
use zbus::zvariant::ObjectPath;
use zbus::{fdo, Connection};

use crate::account::AccountService;
use crate::daemon::manager::{AccountManager, Bus, HelperStateSignal};
use crate::dbus::accounts::Accounts;
use crate::dbus::files::Files;
use crate::sync::SyncService;
use crate::dbus::{ActivityLog, Conflicts, Folder, LocalScan, Transfers, UploadQueue};
use crate::dbus::signals::start_signals;

/// Serves one account's folder — `Folder`, `Transfers`, `UploadQueue`, `Conflicts`,
/// `LocalScan` and `ActivityLog` — at `path` and starts their signals; the tasks
/// that send them, to stop when the account goes.
///
/// At startup this runs before the bus name is claimed
/// (`crate::daemon::startup::serve`): `main` used to claim the name, then connect
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

/// Takes one account's folder off the bus (`Accounts.Remove`, and an `Accounts.Add` that
/// failed while putting it there): every interface, whatever the one before answered.
pub async fn unexport(connection: &Connection, path: &ObjectPath<'_>) -> zbus::Result<()> {
    let server = connection.object_server();
    all_taken_off([
        server.remove::<ActivityLog, _>(path).await,
        server.remove::<LocalScan, _>(path).await,
        server.remove::<Conflicts, _>(path).await,
        server.remove::<UploadQueue, _>(path).await,
        server.remove::<Transfers, _>(path).await,
        server.remove::<Folder, _>(path).await,
    ])
}

/// What taking several interfaces off the bus comes to: an interface that was not there is
/// off, and the first other failure is the answer.
pub(crate) fn all_taken_off(taken: impl IntoIterator<Item = zbus::Result<bool>>) -> zbus::Result<()> {
    let mut failures = taken.into_iter().filter_map(|taken| match taken {
        Ok(_) | Err(zbus::Error::InterfaceNotFound) => None,
        Err(e) => Some(e),
    });
    failures.next().map_or(Ok(()), Err)
}

/// The daemon's objects on a bus: what the account manager and the startup are given
/// (`daemon::manager::Options::bus`).
pub struct OnBus;

#[async_trait]
impl Bus for OnBus {
    async fn serve(&self, connection: &Connection, manager: &Arc<AccountManager>) -> zbus::Result<()> {
        let server = connection.object_server();
        server.at(ACCOUNTS_PATH, fdo::ObjectManager).await?;
        server.at(ACCOUNTS_PATH, Accounts { manager: Arc::clone(manager) }).await?;
        server.at(ACCOUNTS_PATH, Files { manager: Arc::clone(manager) }).await?;
        Ok(())
    }

    async fn helper_state(&self, connection: &Connection) -> zbus::Result<Box<dyn HelperStateSignal>> {
        let iface = connection.object_server().interface::<_, Accounts>(ACCOUNTS_PATH).await?;
        Ok(Box::new(AccountsOnBus(iface)))
    }

    async fn export_account(&self, connection: &Connection, path: &ObjectPath<'_>, account: Arc<AccountService>) -> zbus::Result<JoinHandle<()>> {
        crate::dbus::account::export(connection, path, account).await
    }

    async fn export_folder(&self, connection: &Connection, path: &ObjectPath<'_>, sync: Arc<SyncService>) -> zbus::Result<Vec<JoinHandle<()>>> {
        export(connection, path, sync).await
    }

    async fn unexport_folder(&self, connection: &Connection, path: &ObjectPath<'_>) -> zbus::Result<()> {
        unexport(connection, path).await
    }

    async fn unexport_account(&self, connection: &Connection, path: &ObjectPath<'_>) -> zbus::Result<()> {
        crate::dbus::account::unexport(connection, path).await
    }
}

/// `Accounts` as it is on the bus, for announcing `HelperState`.
struct AccountsOnBus(InterfaceRef<Accounts>);

#[async_trait]
impl HelperStateSignal for AccountsOnBus {
    async fn changed(&self) -> zbus::Result<()> {
        self.0.get().await.helper_state_changed(self.0.signal_emitter()).await
    }
}
