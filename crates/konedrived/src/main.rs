use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use konedrived::accounts::{self, Options};
use konedrived::config::Paths;
use konedrived::oauth::Endpoints;
use konedrived::secret::SecretServiceWallet;
use konedrived::sync::{self, baloo::Baloo};
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
    let options = Options {
        endpoints: Endpoints::microsoft(),
        wallet: Arc::new(SecretServiceWallet::new()),
        sign_in_timeout: SIGN_IN_TIMEOUT,
        // Keep KDE's Baloo indexer out of a fresh OneDrive folder: the one
        // place that installs the real `balooctl6`.
        baloo: Baloo::default,
        thumbnails: Some(paths.thumbnails.clone()),
        onedrive: true,
    };
    // `config.toml` migrated, every account brought up as far as it can be
    // without the helper, every object exported, and only then the bus name
    // claimed: a D-Bus-activated client's first call is never answered from
    // stale state (design §2.2). Held for the life of the process: dropping
    // the connection would drop the bus name and every object with it.
    let daemon = accounts::start(zbus::connection::Builder::session()?, paths, options).await?;
    let hub = Arc::clone(daemon.manager.hub());
    // HS1: with no link, `HelperState` says what systemd says of the
    // helper's unit (read-only, on the system bus). The one place that asks
    // the real systemd.
    hub.set_unit(Arc::new(sync::helper_status::Systemd::default()));
    // Everything that can be slow — the helper connection, which can take up
    // to 30 s against a helper that accepts and then says nothing, and each
    // folder's registration and recovery walk — happens in the supervisor,
    // where it delays nobody's first call. On every connect it brings every
    // account's folder up, one after another, before it serves a fill.
    tokio::spawn(sync::hub::supervise(
        Arc::clone(&hub),
        PathBuf::from(konedrive_proto::SOCKET_PATH),
        HELPER_BACKOFF,
    ));
    // `HelperState` follows the link, and systemd every 30 s without one.
    tokio::spawn(sync::hub::watch(Arc::clone(&hub)));
    // Every OneDrive folder is brought up to date the moment the network is back.
    tokio::spawn(sync::network::watch(hub));

    tracing::info!("konedrived ready");
    let _connection = daemon.connection;
    std::future::pending::<()>().await;
    Ok(())
}
