use std::os::unix::fs::DirBuilderExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use konedrive_dbus::account_path;
use tokio::task::JoinHandle;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::Connection;
use konedrive_graph::oauth::Endpoints;

use crate::account::{AccountService, Siblings};
use crate::config::{is_valid_client_id, AccountConfig, AccountPaths, ConfigError, ConfigStore, OnBattery, Paths};
use crate::account::secret::{AccountSecrets, Wallet};
use crate::account::state::SignInState;
use crate::desktop::baloo::Baloo;
use crate::dbus::fault::SyncFault;
use crate::sync::hub::HelperHub;
use crate::conditions::running::HoldSettings;
use crate::sync::{Persist, SyncError, SyncPaths, SyncService};
use crate::dbus::files::outside;

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
    /// What the account's folder refused, under the folder's names (`NoHelper`, …).
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
    pub(crate) config: Arc<ConfigStore>,
    paths: Paths,
    options: Options,
    pub(crate) hub: Arc<HelperHub>,
    siblings: Arc<Siblings>,
    accounts: Mutex<Vec<Arc<Account>>>,
    /// `Add`, `Remove`, `SetClientId`, `SetPauseOnMetered` and `SetOnBattery`, one at a
    /// time.
    changing: tokio::sync::Mutex<()>,
}

impl AccountManager {
    /// The hub takes the hold's settings from `config` now, before any account joins it.
    pub fn new(config: Arc<ConfigStore>, paths: Paths, options: Options, hub: Arc<HelperHub>) -> Arc<Self> {
        hub.set_hold_settings(HoldSettings::of(&config.snapshot()));
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

    /// `Accounts.List`.
    pub fn paths(&self) -> Vec<OwnedObjectPath> {
        self.accounts().iter().map(|a| a.path.clone()).collect()
    }

    /// Brings up every account `config.toml` holds, in file order (design §2.2, step 3):
    /// the session restored from the wallet and the cache, and an intercepted folder held
    /// until the helper is back. An account that repeats an earlier one's id, label, drive
    /// or folder is loaded but held back (§3.1); one whose id cannot name an object or a
    /// directory, or repeats an id, is not loaded at all, and `Accounts.LastError` says so.
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
                    // The folder starts in the mode the account does, before it is restored.
                    follow_mode(&account);
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
        // One quota for the account, whoever reads it (issue #78).
        sync.set_quota(account.quota().clone());
        let config = self.config.snapshot();
        sync.set_transfer_limits(config.transfer_ceiling(), config.transfer_large());
        // A switch to read-only asks the folder what waits to be uploaded (`docs/design/writes.md` §2).
        let uploads: std::sync::Weak<SyncService> = Arc::downgrade(&sync);
        account.set_uploads(uploads);
        // The folder's outbox worker, finding the write gate closed, has the mode worked out
        // again.
        let checked = Arc::downgrade(&account);
        sync.set_mode_check(Arc::new(move || {
            if let Some(account) = checked.upgrade() {
                account.recheck_mode();
            }
        }));
        // A cycle that finds the token reaching another drive than the folder's tells the
        // account, which records it and works its mode out again.
        let seen = Arc::downgrade(&account);
        sync.set_drive_seen(Arc::new(move |drive| {
            if let Some(account) = seen.upgrade() {
                account.drive_seen(drive);
            }
        }));
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
    pub(crate) async fn export(&self, connection: &Connection, account: &Account) -> zbus::Result<()> {
        let path = account.path.as_ref();
        let mut signals = vec![crate::dbus::account::export(connection, &path, Arc::clone(&account.account)).await?];
        signals.extend(crate::dbus::export::export(connection, &path, Arc::clone(&account.sync)).await?);
        account.signals.lock().unwrap().extend(signals);
        Ok(())
    }

    /// Takes `account`'s objects off the bus.
    async fn unexport(&self, connection: &Connection, account: &Account) {
        let path = account.path.as_ref();
        for signals in account.signals.lock().unwrap().drain(..) {
            signals.abort();
        }
        if let Err(e) = crate::dbus::export::unexport(connection, &path).await {
            tracing::warn!("cannot take {path} off the bus: {e}");
        }
        if let Err(e) = crate::dbus::account::unexport(connection, &path).await {
            tracing::warn!("cannot take {path} off the bus: {e}");
        }
    }

    /// `Accounts.Add`: a signed-out, read-only account with no folder, after every other,
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
        follow_mode(&account);
        self.export(connection, &account).await.map_err(|e| ManagerError::Failed(e.to_string()))?;
        self.accounts.lock().unwrap().push(Arc::clone(&account));
        tracing::info!("added the account {:?} ({})", entry.label, entry.id);
        Ok(account)
    }

    /// `Accounts.Remove` (design §4.2): the folder forgotten exactly as
    /// `Folder.Unregister` forgets it — refused, before anything changes, under the same
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

    /// `Accounts.SetClientId`: refused while any account is signing in or signed in.
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

    /// The hold's settings every account runs on (`Accounts.PauseOnMetered`, `OnBattery`).
    pub fn hold_settings(&self) -> HoldSettings {
        self.hub.hold_settings()
    }

    /// `Accounts.SetPauseOnMetered`: written to `config.toml`, then taken by every account
    /// at once (issue #95).
    pub async fn set_pause_on_metered(&self, on: bool) -> Result<(), ManagerError> {
        self.change_hold_settings(|config| config.set_pause_on_metered(on)).await
    }

    /// `Accounts.SetOnBattery`: `sync`, `power-saver` or `pause`, refused `InvalidArgs`
    /// otherwise; written to `config.toml`, then taken by every account at once.
    pub async fn set_on_battery(&self, choice: &str) -> Result<(), ManagerError> {
        let Some(choice) = OnBattery::parse(choice) else {
            return Err(ManagerError::InvalidArgs(format!("{choice:?}: not sync, power-saver or pause")));
        };
        self.change_hold_settings(|config| config.set_on_battery(choice)).await
    }

    /// One change of the hold's settings: one at a time, so that the file and what every
    /// account runs on end up the same.
    async fn change_hold_settings(&self, write: impl FnOnce(&ConfigStore) -> Result<(), ConfigError>) -> Result<(), ManagerError> {
        let _changing = self.changing.lock().await;
        write(&self.config)?;
        let hold = HoldSettings::of(&self.config.snapshot());
        tracing::info!(
            "every account now {} on a metered connection, and on battery: {}",
            if hold.pause_on_metered { "pauses" } else { "syncs" },
            hold.on_battery.as_str()
        );
        self.hub.set_hold_settings(hold);
        Ok(())
    }

    /// The account whose folder holds `path` (design §2.5), for `Files`: the one whose
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

    /// The account whose folder `path` itself is, for `Files.WebUrl`: the parent resolved
    /// and the name kept, as [`route`](Self::route) and `SyncRoot::open_item` take a path.
    /// `route` never answers for such a path: its parent is in no account's folder.
    pub(crate) async fn folder_itself(&self, path: &Path) -> Option<Arc<Account>> {
        let accounts = self.accounts();
        let given = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let resolved = resolve_parent(&given)?;
            accounts
                .iter()
                .find(|a| {
                    a.sync.root().is_some_and(|root| {
                        resolved == root.path || std::fs::canonicalize(&root.path).is_ok_and(|real| real == resolved)
                    })
                })
                .cloned()
        })
        .await
        .ok()
        .flatten()
    }

    /// Every path routed to its account, grouped by account in the order the accounts are
    /// first named; `OutsideRoot` for the first path in no account's folder, before
    /// anything is done.
    pub(crate) async fn route_all(&self, paths: &[String]) -> Result<Vec<(Arc<Account>, Vec<PathBuf>)>, SyncFault> {
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

/// Makes `account`'s folder follow the mode the account runs in (`docs/design/writes.md` §2): it starts
/// in the account's mode now, and a task switches it whenever `Account.Mode` changes. The
/// task goes with the account's other tasks when it is removed.
fn follow_mode(account: &Account) {
    let changes = account.account.state().subscribe();
    account.sync.start_in_mode(changes.borrow().mode);
    let follower = tokio::spawn(crate::sync::write_mode::follow(changes, Arc::downgrade(&account.sync)));
    account.signals.lock().unwrap().push(follower);
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
