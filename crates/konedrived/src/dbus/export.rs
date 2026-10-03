use std::sync::Arc;

use tokio::task::JoinHandle;
use zbus::zvariant::ObjectPath;
use zbus::Connection;

use crate::sync::SyncService;
use crate::dbus::{ActivityLog, Conflicts, Folder, LocalScan, Transfers, UploadQueue};
use crate::dbus::signals::start_signals;

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
