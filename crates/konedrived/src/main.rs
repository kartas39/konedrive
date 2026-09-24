use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use konedrived::account::AccountService;
use konedrived::config::Paths;
use konedrived::dbus;
use konedrived::oauth::Endpoints;
use konedrived::secret::SecretServiceStore;
use konedrived::sync::{self, SyncService};
use tracing_subscriber::EnvFilter;

const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// The first delay before a helper reconnect is retried; it doubles up to
/// `sync::MAX_HELPER_BACKOFF`.
const HELPER_BACKOFF: Duration = Duration::from_secs(1);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let paths = Paths::from_xdg()?;
    let service = AccountService::new(
        paths.clone(),
        Endpoints::microsoft(),
        Arc::new(SecretServiceStore::new()),
        SIGN_IN_TIMEOUT,
    )?;
    // Restores the session (its own Secret Service connection, no D-Bus needed) before the
    // bus name is claimed, so a D-Bus-activated client's first call is never answered from
    // stale, pre-restore state.
    service.startup().await;

    // Sync sub-project. The service exists before the connection does and
    // starts with no helper link: `Sync1` is served through the same builder
    // as `Account1`, so both interfaces are on the object before the bus
    // name is claimed. Everything that can be slow — the
    // helper connection, which can take up to 30 s against a helper that
    // accepts and then says nothing, and the root's registration and
    // recovery walk — happens afterwards, in the supervisor below, where it
    // delays nobody's first call.
    let sync_service = SyncService::new(
        None,
        Some(service.state().clone()),
        Some(paths.config_file.clone()),
    );
    // Before anything is restored or registered: a folder registered while
    // signed in shows OneDrive only with a drive to show, and a
    // restored one starts syncing as it is brought up.
    sync_service.set_drive(service.drive()?);
    sync_service.set_sync_paths(sync::SyncPaths {
        tree_db: paths.tree_db.clone(),
        rescue_dir: paths.rescue_dir.clone(),
        thumbnails: Some(paths.thumbnails.clone()),
    });
    // Keep KDE's Baloo indexer out of a fresh OneDrive folder.
    // `SyncService` starts with a `Baloo` that runs nothing; this is the one
    // place that installs the real `balooctl6`.
    sync_service.set_baloo(sync::baloo::Baloo::default());
    // HS1: with no link, `HelperState` says what systemd says of the
    // helper's unit (read-only, on the system bus). The one place that asks
    // the real systemd.
    sync_service.set_helper_unit(Arc::new(sync::helper_status::Systemd::default()));
    // An intercepted root recorded in config.toml is held before the name is
    // claimed, so the first thing a client reads is that folder, waiting for
    // the helper, rather than `none`. It reads config.toml and at most one
    // xattr and asks nothing of the helper, so it delays no one; every call
    // that changes the registration runs the same step first anyway.
    sync_service.restore().await;
    // Held for the life of the process: dropping the connection would drop
    // the bus name and both interfaces with it.
    let _connection = dbus::serve(
        zbus::connection::Builder::session()?,
        Arc::clone(&service),
        Some(Arc::clone(&sync_service)),
    )
    .await?;

    // A root persisted without interception needs no helper
    // and comes up right away; an ordinary one waits for the supervisor's
    // first successful connection, which calls `resume` again.
    sync_service.resume().await;
    tokio::spawn(sync::supervise_helper(
        Arc::clone(&sync_service),
        PathBuf::from(konedrive_proto::SOCKET_PATH),
        HELPER_BACKOFF,
    ));
    // `HelperState` follows the link, and systemd every 30 s without one.
    tokio::spawn(sync::watch_helper(Arc::clone(&sync_service)));
    // A OneDrive folder is brought up to date the moment the network is back.
    tokio::spawn(sync::network::watch(Arc::clone(&sync_service)));

    tracing::info!("konedrived ready");
    std::future::pending::<()>().await;
    Ok(())
}
