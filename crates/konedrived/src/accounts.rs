//! The accounts of one daemon (design §2, §4): the manager at `/org/konedrive/Accounts` —
//! `org.konedrive.Accounts1`, `org.konedrive.Files1` and the `ObjectManager` — and, for each
//! account, its `Account1`, `Sync1` and `Dev1` at `/org/konedrive/Accounts/<id>`.
//!
//! [`start`] is the daemon's startup, in the order of design §2.2: `config.toml` loaded and
//! migrated, the files of a migrated account moved, every account brought up to where no
//! helper is needed, every object exported, and only then the bus name claimed.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use konedrive_dbus::{account_path, ACCOUNTS_PATH, SERVICE_NAME};
use tokio::task::JoinHandle;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::{fdo, interface, Connection, DBusError};

use crate::account::{AccountService, Siblings};
use crate::config::{is_valid_client_id, AccountConfig, AccountPaths, ConfigError, ConfigStore, Paths};
use crate::oauth::Endpoints;
use crate::secret::{AccountSecrets, Slot, Wallet};
use crate::state::SignInState;
use crate::sync::baloo::Baloo;
use crate::sync::dbus::{to_fault, SyncFault};
use crate::sync::hub::HelperHub;
use crate::sync::{Persist, SyncError, SyncPaths, SyncService};

/// How long the migration waits for the wallet to say whether version 1's refresh token is
/// there; a wallet that does not answer counts as "maybe".
const WALLET_CHECK: Duration = Duration::from_secs(10);

/// What the daemon's accounts are made with. `main` gives Microsoft, the Secret Service, the
/// real `balooctl6` and the freedesktop thumbnail cache; a test gives wiremock, a
/// [`crate::secret::MemoryWallet`], [`Baloo::disabled`] and no thumbnails.
pub struct Options {
    pub endpoints: Endpoints,
    pub wallet: Arc<dyn Wallet>,
    pub sign_in_timeout: Duration,
    /// Each account's Baloo.
    pub baloo: fn() -> Baloo,
    /// The freedesktop thumbnail cache, shared by every account; `None` fills none.
    pub thumbnails: Option<PathBuf>,
    /// Whether a folder registered while signed in shows the account's OneDrive: always in
    /// the daemon. A test of local folders turns it off, and every folder is then filled
    /// with `PopulateFromDirectory`, as a service with no drive always was.
    pub onedrive: bool,
}

/// One account: its sign-in, its folder, and the tasks that turn their state into
/// `PropertiesChanged`.
pub struct Account {
    pub id: String,
    pub path: OwnedObjectPath,
    pub account: Arc<AccountService>,
    pub sync: Arc<SyncService>,
    paths: AccountPaths,
    signals: Mutex<Vec<JoinHandle<()>>>,
}

/// Why the manager refused.
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    /// `InvalidArgs`: a label or a client id the rules refuse.
    #[error("{0}")]
    InvalidArgs(String),
    /// `Failed`: `config.toml` cannot be read or written, or the call is not possible now.
    #[error("{0}")]
    Failed(String),
    #[error("there is no account {0}")]
    NoAccount(String),
    /// What the account's folder refused, under `Sync1`'s names (`NoHelper`, …).
    #[error(transparent)]
    Sync(#[from] SyncError),
}

impl From<ConfigError> for ManagerError {
    fn from(error: ConfigError) -> Self {
        match error {
            ConfigError::InvalidLabel(_) | ConfigError::InvalidClientId => ManagerError::InvalidArgs(error.to_string()),
            ConfigError::NoAccount(id) => ManagerError::NoAccount(id),
            other => ManagerError::Failed(other.to_string()),
        }
    }
}

/// The accounts, in the order they were added, and what they share: `config.toml`, the
/// helper hub, and the identity guard's view of every account.
pub struct AccountManager {
    config: Arc<ConfigStore>,
    paths: Paths,
    options: Options,
    hub: Arc<HelperHub>,
    siblings: Arc<Siblings>,
    accounts: Mutex<Vec<Arc<Account>>>,
    /// `Add`, `Remove` and `SetClientId`, one at a time.
    changing: tokio::sync::Mutex<()>,
}

impl AccountManager {
    pub fn new(config: Arc<ConfigStore>, paths: Paths, options: Options, hub: Arc<HelperHub>) -> Arc<Self> {
        Arc::new(Self {
            config,
            paths,
            options,
            hub,
            siblings: Arc::new(Siblings::default()),
            accounts: Mutex::new(Vec::new()),
            changing: tokio::sync::Mutex::new(()),
        })
    }

    pub fn config(&self) -> &Arc<ConfigStore> {
        &self.config
    }

    /// The link to the helper every account shares.
    pub fn hub(&self) -> &Arc<HelperHub> {
        &self.hub
    }

    /// Every account, in the order it was added.
    pub fn accounts(&self) -> Vec<Arc<Account>> {
        self.accounts.lock().unwrap().clone()
    }

    /// The account at `path`, if one is.
    pub fn account(&self, path: &ObjectPath<'_>) -> Option<Arc<Account>> {
        self.accounts().into_iter().find(|a| a.path.as_str() == path.as_str())
    }

    /// `Accounts1.Accounts`.
    pub fn paths(&self) -> Vec<OwnedObjectPath> {
        self.accounts().iter().map(|a| a.path.clone()).collect()
    }

    /// Brings up every account `config.toml` holds, in file order (design §2.2, step 3):
    /// the session restored from the wallet and the cache, and an intercepted folder held
    /// until the helper is back. An account that repeats an earlier one's id, label, drive
    /// or folder is loaded but held back (§3.1); one whose id cannot name an object or a
    /// directory, or repeats an id, is not loaded at all, and `Accounts1.LastError` says so.
    pub async fn load(&self) {
        let config = self.config.snapshot();
        for (entry, held) in config.accounts.iter().zip(config.holds()) {
            let taken = self.accounts().iter().any(|a| a.id == entry.id);
            if taken || account_path(&entry.id).is_none() || self.paths.account(&entry.id).is_none() {
                let why = held.unwrap_or_else(|| "its id repeats an earlier account's".into());
                let message = format!("the account {:?} is not loaded: {why}", entry.label);
                tracing::error!("{message}");
                self.config.note_error(message);
                continue;
            }
            match self.build(entry, held.as_deref()) {
                Ok(account) => {
                    account.account.startup().await;
                    account.sync.restore().await;
                    self.accounts.lock().unwrap().push(account);
                }
                Err(e) => {
                    let message = format!("the account {:?} cannot be loaded: {e}", entry.label);
                    tracing::error!("{message}");
                    self.config.note_error(message);
                }
            }
        }
    }

    /// One account's services, wired as the daemon wires them, and not yet on the bus.
    fn build(&self, entry: &AccountConfig, held: Option<&str>) -> anyhow::Result<Arc<Account>> {
        let path = account_path(&entry.id).ok_or_else(|| anyhow::anyhow!("{:?} cannot name an object", entry.id))?;
        let paths = self.paths.account(&entry.id).ok_or_else(|| anyhow::anyhow!("{:?} is not an account id", entry.id))?;
        // Everything in it goes with the account, and is nobody else's.
        if let Err(e) = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&paths.dir) {
            tracing::warn!("cannot create {}: {e}", paths.dir.display());
        }
        let secrets = Arc::new(AccountSecrets::new(Arc::clone(&self.options.wallet), Arc::clone(&self.config), &entry.id));
        let account = AccountService::new(
            Arc::clone(&self.config),
            &entry.id,
            paths.clone(),
            self.options.endpoints.clone(),
            secrets,
            self.options.sign_in_timeout,
        )?;
        self.siblings.add(&account);
        let persist = Persist { store: Arc::clone(&self.config), account: entry.id.clone() };
        let sync = SyncService::on_hub(&self.hub, Some(account.state().clone()), Some(persist));
        // Before anything is restored or registered: a folder registered while signed in
        // shows OneDrive only with a drive to show, and a restored one starts syncing as it
        // is brought up.
        if self.options.onedrive {
            sync.set_drive(account.drive()?);
            sync.set_sync_paths(SyncPaths {
                tree_db: paths.tree_db.clone(),
                rescue_dir: paths.rescue_dir.clone(),
                thumbnails: self.options.thumbnails.clone(),
            });
        }
        sync.set_baloo((self.options.baloo)());
        if let Some(why) = held {
            sync.hold_back(why);
        }
        Ok(Arc::new(Account { id: entry.id.clone(), path, account, sync, paths, signals: Mutex::new(Vec::new()) }))
    }

    /// Puts `account`'s objects on the bus.
    async fn export(&self, connection: &Connection, account: &Account) -> zbus::Result<()> {
        let path = account.path.as_ref();
        let mut signals = vec![crate::dbus::export(connection, &path, Arc::clone(&account.account)).await?];
        signals.extend(crate::sync::dbus::export(connection, &path, Arc::clone(&account.sync)).await?);
        account.signals.lock().unwrap().extend(signals);
        Ok(())
    }

    /// Takes `account`'s objects off the bus.
    async fn unexport(&self, connection: &Connection, account: &Account) {
        let path = account.path.as_ref();
        for signals in account.signals.lock().unwrap().drain(..) {
            signals.abort();
        }
        if let Err(e) = crate::sync::dbus::unexport(connection, &path).await {
            tracing::warn!("cannot take {path} off the bus: {e}");
        }
        if let Err(e) = crate::dbus::unexport(connection, &path).await {
            tracing::warn!("cannot take {path} off the bus: {e}");
        }
    }

    /// `Accounts1.Add`: a signed-out, read-only account with no folder, after every other,
    /// on the bus from the moment it is listed.
    pub async fn add(&self, label: &str, connection: &Connection) -> Result<Arc<Account>, ManagerError> {
        let _changing = self.changing.lock().await;
        let entry = self.config.add_account(label)?;
        let account = match self.build(&entry, None) {
            Ok(account) => account,
            Err(e) => {
                if let Err(e) = self.config.remove_account(&entry.id) {
                    tracing::warn!("cannot take the account {:?} out of config.toml again: {e}", entry.label);
                }
                return Err(ManagerError::Failed(e.to_string()));
            }
        };
        account.account.startup().await;
        self.export(connection, &account).await.map_err(|e| ManagerError::Failed(e.to_string()))?;
        self.accounts.lock().unwrap().push(Arc::clone(&account));
        tracing::info!("added the account {:?} ({})", entry.label, entry.id);
        Ok(account)
    }

    /// `Accounts1.Remove` (design §4.2): the folder forgotten exactly as
    /// `Sync1.UnregisterRoot` forgets it — refused, before anything changes, under the same
    /// rule (`NoHelper` for an intercepted folder with no helper) — then a sign-in under way
    /// cancelled, the refresh token, the cached name and quota and the tree store deleted,
    /// the account taken out of `config.toml`, and its object off the bus. The folder's
    /// files and the rescued files are kept.
    pub async fn remove(&self, path: &ObjectPath<'_>, connection: &Connection) -> Result<(), ManagerError> {
        let _changing = self.changing.lock().await;
        let account = self.account(path).ok_or_else(|| ManagerError::NoAccount(path.to_string()))?;
        if self.config.is_poisoned() {
            return Err(ManagerError::Failed(self.config.last_error()));
        }
        // Retired first, each under its own lock, so that no call on the account's own
        // objects can register a folder or store a sign-in in between: the folder
        // forgotten (a held account's through the helper too), then the sign-in.
        account.sync.retire().await?;
        account.account.retire().await.map_err(|e| ManagerError::Failed(format!("cannot delete the sign-in: {e}")))?;
        self.config.remove_account(&account.id)?;
        if let Err(e) = std::fs::remove_dir_all(&account.paths.dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("cannot remove {}: {e}", account.paths.dir.display());
            }
        }
        self.unexport(connection, &account).await;
        self.accounts.lock().unwrap().retain(|a| a.id != account.id);
        self.hub.leave(&account.sync);
        self.siblings.remove(&account.id);
        tracing::info!("removed the account {} ({path})", account.account.state().get().label);
        Ok(())
    }

    /// `Accounts1.SetClientId`: refused while any account is signing in or signed in.
    pub async fn set_client_id(&self, id: &str) -> Result<(), ManagerError> {
        let _changing = self.changing.lock().await;
        let id = id.trim();
        if !is_valid_client_id(id) {
            return Err(ConfigError::InvalidClientId.into());
        }
        if self.accounts().iter().any(|a| a.account.state().get().state != SignInState::SignedOut) {
            return Err(ManagerError::Failed(
                "not possible while an account is signing in or signed in: sign out first".into(),
            ));
        }
        self.config.set_client_id(id)?;
        for account in self.accounts() {
            account.account.use_client_id(id);
        }
        Ok(())
    }

    /// The account whose folder holds `path` (design §2.5), for `Files1`: the one whose
    /// folder is a component prefix of it, taken as given, or else with its directory part
    /// resolved — a folder reached through a link (`/home` → `/var/home`). The file itself
    /// is never opened.
    pub async fn route(&self, path: &Path) -> Option<Arc<Account>> {
        let accounts = self.accounts();
        let holds = |path: &Path| {
            accounts.iter().find(|a| a.sync.root().is_some_and(|root| path.starts_with(&root.path))).cloned()
        };
        // The directory part resolved first: `..`, and a link to a directory in another
        // account's folder, lead where the file really is. Taken as given only when it
        // cannot be resolved (the file cannot be there then), and never with a `.` or `..`.
        let given = path.to_path_buf();
        match tokio::task::spawn_blocking(move || resolve_parent(&given)).await.ok().flatten() {
            Some(resolved) => holds(&resolved),
            None if path.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_))) => holds(path),
            None => None,
        }
    }

    /// Every path routed to its account, grouped by account in the order the accounts are
    /// first named; `OutsideRoot` for the first path in no account's folder, before
    /// anything is done.
    async fn route_all(&self, paths: &[String]) -> Result<Vec<(Arc<Account>, Vec<PathBuf>)>, SyncFault> {
        let mut groups: Vec<(Arc<Account>, Vec<PathBuf>)> = Vec::new();
        for path in paths {
            let account = self.route(Path::new(path)).await.ok_or_else(|| outside(path))?;
            match groups.iter_mut().find(|(a, _)| Arc::ptr_eq(a, &account)) {
                Some((_, group)) => group.push(PathBuf::from(path)),
                None => groups.push((account, vec![PathBuf::from(path)])),
            }
        }
        Ok(groups)
    }

    /// Brings up every folder that needs no helper (design §2.2, step 5); an intercepted one
    /// waits for the hub's first connection.
    pub async fn resume_all(&self) {
        for account in self.accounts() {
            account.sync.resume().await;
        }
    }
}

/// `path` with its directory part resolved and its last component kept as it is — the file
/// itself is never followed — or the whole path resolved when it ends in `..`. `None` when
/// it cannot be resolved. Blocking.
fn resolve_parent(path: &Path) -> Option<PathBuf> {
    match path.file_name() {
        Some(name) => {
            let parent = match path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent,
                _ => Path::new("."),
            };
            Some(std::fs::canonicalize(parent).ok()?.join(name))
        }
        None => std::fs::canonicalize(path).ok(),
    }
}

fn outside(path: &str) -> SyncFault {
    SyncFault::OutsideRoot(format!("{path} is in no account's folder"))
}

/// How `Accounts1` refuses: under `Sync1`'s names for what an account's folder refused
/// (`NoHelper`, …) and `NoAccount`, and under the bus's own `InvalidArgs` and `Failed` for a
/// label, a client id or `config.toml`.
#[derive(Debug)]
pub enum ManagerFault {
    Sync(SyncFault),
    Fdo(fdo::Error),
}

impl DBusError for ManagerFault {
    fn create_reply(&self, call: &zbus::message::Header<'_>) -> zbus::Result<zbus::message::Message> {
        match self {
            ManagerFault::Sync(fault) => fault.create_reply(call),
            ManagerFault::Fdo(error) => error.create_reply(call),
        }
    }

    fn name(&self) -> zbus::names::ErrorName<'_> {
        match self {
            ManagerFault::Sync(fault) => fault.name(),
            ManagerFault::Fdo(error) => error.name(),
        }
    }

    fn description(&self) -> Option<&str> {
        match self {
            ManagerFault::Sync(fault) => fault.description(),
            ManagerFault::Fdo(error) => error.description(),
        }
    }
}

impl std::fmt::Display for ManagerFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.name(), self.description().unwrap_or(""))
    }
}

impl std::error::Error for ManagerFault {}

impl From<zbus::Error> for ManagerFault {
    fn from(error: zbus::Error) -> Self {
        ManagerFault::Sync(SyncFault::ZBus(error))
    }
}

impl From<ManagerError> for ManagerFault {
    fn from(error: ManagerError) -> Self {
        match error {
            ManagerError::InvalidArgs(why) => ManagerFault::Fdo(fdo::Error::InvalidArgs(why)),
            ManagerError::Failed(why) => ManagerFault::Fdo(fdo::Error::Failed(why)),
            ManagerError::NoAccount(path) => ManagerFault::Sync(SyncFault::NoAccount(format!("there is no account {path}"))),
            ManagerError::Sync(error) => ManagerFault::Sync(to_fault(error)),
        }
    }
}

/// `org.konedrive.Accounts1` (`dbus/org.konedrive.Accounts1.xml`).
pub struct Accounts1 {
    manager: Arc<AccountManager>,
}

#[interface(name = "org.konedrive.Accounts1")]
impl Accounts1 {
    async fn add(
        &self,
        label: &str,
        #[zbus(connection)] connection: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<OwnedObjectPath, ManagerFault> {
        let account = self.manager.add(label, connection).await?;
        self.accounts_changed(&emitter).await?;
        Ok(account.path.clone())
    }

    async fn remove(
        &self,
        account: ObjectPath<'_>,
        #[zbus(connection)] connection: &Connection,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> Result<(), ManagerFault> {
        self.manager.remove(&account, connection).await?;
        self.accounts_changed(&emitter).await?;
        Ok(())
    }

    async fn set_client_id(&self, id: &str, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> Result<(), ManagerFault> {
        self.manager.set_client_id(id).await?;
        self.client_id_changed(&emitter).await?;
        Ok(())
    }

    #[zbus(property)]
    async fn accounts(&self) -> Vec<OwnedObjectPath> {
        self.manager.paths()
    }

    #[zbus(property)]
    async fn client_id(&self) -> String {
        self.manager.config.client_id()
    }

    #[zbus(property)]
    async fn helper_state(&self) -> String {
        self.manager.hub.state().as_str().to_owned()
    }

    #[zbus(property)]
    async fn last_error(&self) -> String {
        self.manager.config.last_error()
    }
}

/// `org.konedrive.Files1` (`dbus/org.konedrive.Files1.xml`): the per-file calls, each
/// routed by path to the account whose folder holds it.
pub struct Files1 {
    manager: Arc<AccountManager>,
}


#[interface(name = "org.konedrive.Files1")]
impl Files1 {
    async fn hydrate(&self, path: &str) -> Result<(), SyncFault> {
        let account = self.manager.route(Path::new(path)).await.ok_or_else(|| outside(path))?;
        account.sync.hydrate_now(Path::new(path)).await.map_err(to_fault)
    }

    async fn dehydrate(&self, path: &str) -> Result<(), SyncFault> {
        let account = self.manager.route(Path::new(path)).await.ok_or_else(|| outside(path))?;
        account.sync.dehydrate(Path::new(path)).await.map_err(to_fault)
    }

    async fn item_state(&self, path: &str) -> String {
        match self.manager.route(Path::new(path)).await {
            Some(account) => account.sync.item_state(Path::new(path)).await,
            None => "not-managed".into(),
        }
    }

    /// "Always keep on this device" for each path; how many files were queued for
    /// download, over every account.
    #[zbus(out_args("queued"))]
    async fn pin(&self, paths: Vec<String>) -> Result<u32, SyncFault> {
        let groups = self.manager.route_all(&paths).await?;
        for (account, paths) in &groups {
            account.sync.check_pinnable(paths).await.map_err(to_fault)?;
        }
        let mut queued = 0;
        for (account, paths) in groups {
            queued += account.sync.pin(&paths).await.map_err(to_fault)?;
        }
        Ok(queued)
    }

    /// Unchecking "Always keep on this device": each path's own pin comes off, and its
    /// files stay; how many pins came off. Every account's paths are checked first.
    #[zbus(out_args("unpinned"))]
    async fn unpin(&self, paths: Vec<String>) -> Result<u32, SyncFault> {
        let groups = self.manager.route_all(&paths).await?;
        for (account, paths) in &groups {
            account.sync.check_unpinnable(paths).await.map_err(to_fault)?;
        }
        let mut unpinned = 0;
        for (account, paths) in groups {
            unpinned += account.sync.unpin(&paths).await.map_err(to_fault)?;
        }
        Ok(unpinned)
    }

    /// "Free up space" for each path, taking its own pin off first. `busy` counts the
    /// files kept because they were in use or changed here. Every account's paths are
    /// checked first.
    #[zbus(out_args("files", "bytes", "busy", "skipped_pinned"))]
    async fn free_up(&self, paths: Vec<String>) -> Result<(u32, u64, u32, u32), SyncFault> {
        let groups = self.manager.route_all(&paths).await?;
        for (account, paths) in &groups {
            account.sync.check_free_up(paths).await.map_err(to_fault)?;
        }
        let mut total = (0, 0, 0, 0);
        for (account, paths) in groups {
            let freed = account.sync.free_up(&paths).await.map_err(to_fault)?;
            total.0 += freed.files;
            total.1 += freed.bytes;
            total.2 += freed.busy + freed.modified;
            total.3 += freed.pinned;
        }
        Ok(total)
    }
}

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
    crate::migrate::finish_file_moves(&config, &paths);
    let manager = AccountManager::new(config, paths, options, hub);
    manager.load().await;
    serve(&connection, &manager).await?;
    manager.resume_all().await;
    Ok(Daemon { connection, manager, _lock: lock })
}

/// Exports the manager and every account on `connection`, then claims `org.konedrive.Daemon`:
/// every object a client may call is there before the name is.
async fn serve(connection: &Connection, manager: &Arc<AccountManager>) -> zbus::Result<()> {
    let server = connection.object_server();
    server.at(ACCOUNTS_PATH, fdo::ObjectManager).await?;
    server.at(ACCOUNTS_PATH, Accounts1 { manager: Arc::clone(manager) }).await?;
    server.at(ACCOUNTS_PATH, Files1 { manager: Arc::clone(manager) }).await?;
    for account in manager.accounts() {
        manager.export(connection, &account).await?;
    }
    // `HelperState` is the hub's: every change of it is `Accounts1`'s to announce.
    let iface = server.interface::<_, Accounts1>(ACCOUNTS_PATH).await?;
    let mut helper = manager.hub.subscribe();
    helper.borrow_and_update();
    tokio::spawn(async move {
        while helper.changed().await.is_ok() {
            helper.borrow_and_update();
            if let Err(e) = iface.get().await.helper_state_changed(iface.signal_emitter()).await {
                tracing::warn!("cannot emit PropertiesChanged for HelperState: {e}");
            }
        }
    });
    connection.request_name(SERVICE_NAME).await
}
