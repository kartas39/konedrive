use std::sync::Arc;

use async_trait::async_trait;
use konedrive_dbus::ACCOUNTS_PATH;
use tokio::task::JoinHandle;
use zbus::object_server::InterfaceRef;
use zbus::zvariant::ObjectPath;
use zbus::Connection;

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
/// `partly`: see [`take_off`].
pub async fn unexport(connection: &Connection, path: &ObjectPath<'_>, partly: bool) -> zbus::Result<()> {
    all_taken_off([
        take_off::<ActivityLog>(connection, path, partly).await,
        take_off::<LocalScan>(connection, path, partly).await,
        take_off::<Conflicts>(connection, path, partly).await,
        take_off::<UploadQueue>(connection, path, partly).await,
        take_off::<Transfers>(connection, path, partly).await,
        take_off::<Folder>(connection, path, partly).await,
    ])
}

/// Takes the interface `I` off `path`. One that is not there is off, and no failure; it is
/// worth a warning unless `partly`, which the cleanup of an `Accounts.Add` that failed part
/// of the way gives: on a removal every interface is there.
pub(crate) async fn take_off<I: zbus::object_server::Interface>(connection: &Connection, path: &ObjectPath<'_>, partly: bool) -> zbus::Result<()> {
    match connection.object_server().remove::<I, _>(path).await {
        Ok(_) => Ok(()),
        Err(zbus::Error::InterfaceNotFound) => {
            if !partly {
                tracing::warn!("{} was not on the bus at {path}", I::name());
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// What taking several interfaces off the bus comes to: the first failure, after every one
/// was tried.
pub(crate) fn all_taken_off(taken: impl IntoIterator<Item = zbus::Result<()>>) -> zbus::Result<()> {
    taken.into_iter().find(Result::is_err).unwrap_or(Ok(()))
}

/// The daemon's objects on a bus: what the account manager and the startup are given
/// (`daemon::manager::Options::bus`).
pub struct OnBus;

#[async_trait]
impl Bus for OnBus {
    async fn serve(&self, connection: &Connection, manager: &Arc<AccountManager>) -> zbus::Result<()> {
        // The `ObjectManager` is there already: the connection is built with it
        // (`crate::daemon::startup::connect`).
        let server = connection.object_server();
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

    async fn unexport_folder(&self, connection: &Connection, path: &ObjectPath<'_>, partly: bool) -> zbus::Result<()> {
        unexport(connection, path, partly).await
    }

    async fn unexport_account(&self, connection: &Connection, path: &ObjectPath<'_>, partly: bool) -> zbus::Result<()> {
        crate::dbus::account::unexport(connection, path, partly).await
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
