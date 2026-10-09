use std::sync::Arc;
use std::time::Duration;

use konedrived::daemon::manager::Options;
use konedrived::config::Paths;
use konedrive_graph::oauth::Endpoints;
use konedrived::account::secret::SecretServiceWallet;
use konedrived::daemon::stop;
use konedrived::desktop::baloo::Baloo;
use futures_util::FutureExt;
use tracing_subscriber::EnvFilter;

const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The only argument there is; everything else is ignored, as it always was.
    if std::env::args().nth(1).is_some_and(|arg| arg == "--version" || arg == "-V") {
        use konedrive_dbus::version::{line, COMMIT, VERSION};
        println!("{}", line("konedrived", VERSION, COMMIT));
        return Ok(());
    }
    // What the unit runs after the daemon's end, however it ended: a folder that shows OneDrive
    // loses the mount the daemon left on it, and is locked.
    if std::env::args().nth(1).is_some_and(|arg| arg == "--tidy") {
        let folders = konedrived::fuse::tidy_served_folders(&Paths::from_xdg()?.config_file);
        // A file manager that showed the mount reads the folder on disk now.
        let directories = folders.iter().flat_map(|folder| konedrived::fuse::list_directories(folder)).collect();
        konedrived::desktop::dirnotify::read_again(directories).await;
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    // Before anything is sent: from now on SIGTERM and SIGINT stop the
    // daemon through `stop`, not by themselves.
    let mut signals = stop::Signals::install()?;
    let paths = Paths::from_xdg()?;
    let options = Options {
        endpoints: Endpoints::microsoft(),
        wallet: Arc::new(SecretServiceWallet::new()),
        sign_in_timeout: SIGN_IN_TIMEOUT,
        // Keep KDE's Baloo indexer out of a fresh OneDrive folder: the one
        // place that installs the real `balooctl6`.
        baloo: Baloo::default,
        thumbnails: Some(paths.thumbnails.clone()),
        drive: konedrived::daemon::manager::own_drive(),
        bus: Arc::new(konedrived::dbus::export::OnBus),
    };
    // `config.toml` migrated, every account's folder taken up, every object exported, and
    // only then the bus name claimed: a D-Bus-activated client's first call is never answered from
    // stale state (`docs/design/accounts.md` §3.2). Held for the life of the process: dropping
    // the connection would drop the bus name and every object with it.
    // A stop before the daemon is up has nothing in flight to wait for.
    let daemon = tokio::select! {
        daemon = konedrived::daemon::startup::start(zbus::connection::Builder::session()?, paths, options) => daemon?,
        _ = signals.next() => {
            tracing::info!("stopped before the daemon was up");
            std::process::exit(0)
        }
    };
    let registry = Arc::clone(daemon.manager.registry());
    // Kept, each of them: one that is gone ends the daemon (quality finding `SY11`).
    let mut tasks = stop::Tasks::default();
    // Every account holds back by itself on a metered connection or on battery, as its
    // settings say (`conditions`). With no system bus it has nothing to follow, and ends.
    tasks.spawn("the watcher of power and metering", stop::Need::WhileItCan, konedrived::conditions::watch(Arc::clone(&registry)));
    // Every OneDrive folder is brought up to date the moment the network is back.
    tasks.spawn("the network's watcher", stop::Need::WhileItCan, konedrived::conditions::network::watch(Arc::clone(&registry)));

    tracing::info!("konedrived ready");
    let _connection = daemon.connection;
    // A signal stops the daemon; so does the end of a task it cannot work without, and then
    // it leaves with a failure, for systemd to start it again (`Restart=on-failure`).
    let code = match stop::stopped(signals.next(), &mut tasks).await {
        None => 0,
        Some(died) => {
            tracing::error!("{died}; stopping, to be started again");
            1
        }
    };
    // The stop: nothing new is sent, and the requests in flight
    // get a bounded time to return and be persisted; a second signal ends it.
    tracing::info!("stopping: the uploads in flight get up to {} s", stop::STOP_BOUND.as_secs());
    let closing: Vec<_> = registry.accounts().iter().filter_map(|sync| sync.close_outbox()).collect();
    match stop::wind_down(futures_util::future::join_all(closing).map(|_| ()), stop::STOP_BOUND, signals.next()).await {
        stop::Ended::Finished => tracing::info!("stopped"),
        stop::Ended::Bound => tracing::warn!("stopped: requests still in flight are cut"),
        stop::Ended::Again => tracing::warn!("stopped at once by a second signal"),
    }
    // Last: a folder that shows OneDrive loses its mount, is locked, and whoever showed the
    // mount is told to read the folder on disk. The unit's tidy step does the same for a daemon
    // that could not.
    for sync in registry.accounts().iter() {
        if let Some(folder) = sync.unmount().await {
            konedrived::desktop::dirnotify::read_again(konedrived::fuse::list_directories(&folder)).await;
        }
    }
    // Not through the runtime's drop, which would wait for blocking tasks.
    std::process::exit(code)
}
