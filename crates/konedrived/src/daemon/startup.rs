use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use konedrive_dbus::SERVICE_NAME;
use zbus::{fdo, Connection};

use crate::config::{ConfigStore, Paths};
use crate::account::secret::Slot;
use crate::sync::hub::HelperHub;
use crate::daemon::manager::{AccountManager, Options};

/// How long the migration waits for the wallet to say whether version 1's refresh token is
/// there; a wallet that does not answer counts as "maybe".
const WALLET_CHECK: Duration = Duration::from_secs(10);

/// The daemon's accounts, on the bus.
pub struct Daemon {
    /// Held for the life of the process: dropping it drops the bus name and every object.
    pub connection: Connection,
    pub manager: Arc<AccountManager>,
    /// `config.toml.lock`, held for the life of the process: one daemon per configuration.
    _lock: nix::fcntl::Flock<std::fs::File>,
}

/// Takes `config.toml.lock` beside `config_file`, or refuses: another konedrived runs on this
/// configuration. Two daemons started at once — one by hand beside the D-Bus-activated one —
/// could otherwise both migrate version 1, each under an account id of its own, and the one
/// that loses the file would move the wallet's token into an account that is not there.
fn lock_config(config_file: &Path) -> anyhow::Result<nix::fcntl::Flock<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = config_file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut name = config_file.as_os_str().to_owned();
    name.push(".lock");
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(&name)?;
    nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock).map_err(|(_, e)| match e {
        nix::errno::Errno::EWOULDBLOCK => anyhow::anyhow!("another konedrived is running on {}", config_file.display()),
        other => anyhow::anyhow!("cannot lock {}: {other}", PathBuf::from(&name).display()),
    })
}

/// Starts the daemon's accounts on `builder`'s bus (design §2.2):
///
/// 1. `config.toml` loaded, a version-1 file migrated (§7) — refused while another
///    konedrived holds the bus name, since it could write version 1 over the result;
/// 2. the files of a migrated account moved, before anything opens them;
/// 3. every account brought up: its session restored, an intercepted folder held until
///    the helper is back;
/// 4. `/org/konedrive/Accounts` and every account's object exported, and only then
///    `org.konedrive.Daemon` claimed;
/// 5. every folder that needs no helper brought up.
///
/// The caller starts the hub's supervisor ([`crate::sync::hub::supervise`]) and watchers.
pub async fn start(builder: zbus::connection::Builder<'_>, paths: Paths, options: Options) -> anyhow::Result<Daemon> {
    start_on(builder, paths, options, HelperHub::new()).await
}

/// [`start`], on a `hub` the caller has set up already: a test's points at a helper socket
/// of its own, so that no startup looks at a helper running on the machine.
pub async fn start_on(
    builder: zbus::connection::Builder<'_>,
    paths: Paths,
    options: Options,
    hub: Arc<HelperHub>,
) -> anyhow::Result<Daemon> {
    let connection = builder.build().await?;
    if fdo::DBusProxy::new(&connection).await?.name_has_owner(SERVICE_NAME.try_into()?).await? {
        anyhow::bail!("{SERVICE_NAME} is running already");
    }
    let lock = lock_config(&paths.config_file)?;
    let wallet = Arc::clone(&options.wallet);
    let legacy_token = async move {
        // A wallet that does not answer, or answers with an error, counts as "maybe".
        !matches!(tokio::time::timeout(WALLET_CHECK, wallet.exists(&Slot::V1)).await, Ok(Ok(false)))
    };
    let config = Arc::new(ConfigStore::open(&paths, legacy_token).await);
    crate::config::migrate::finish_file_moves(&config, &paths);
    crate::config::migrate::move_hold_settings(&config);
    let manager = AccountManager::new(config, paths, options, hub);
    manager.load().await;
    serve(&connection, &manager).await?;
    manager.resume_all().await;
    Ok(Daemon { connection, manager, _lock: lock })
}

/// Exports the manager and every account on `connection`, then claims `org.konedrive.Daemon`:
/// every object a client may call is there before the name is.
async fn serve(connection: &Connection, manager: &Arc<AccountManager>) -> zbus::Result<()> {
    let bus = manager.bus();
    bus.serve(connection, manager).await?;
    for account in manager.accounts() {
        manager.export(connection, &account).await?;
    }
    // `HelperState` is the hub's: every change of it is `Accounts`'s to announce.
    let mut helper = manager.hub.subscribe();
    helper.borrow_and_update();
    let on = connection.clone();
    tokio::spawn(async move {
        while helper.changed().await.is_ok() {
            helper.borrow_and_update();
            if let Err(e) = bus.helper_state_changed(&on).await {
                tracing::warn!("cannot emit PropertiesChanged for HelperState: {e}");
            }
        }
    });
    connection.request_name(SERVICE_NAME).await
}
