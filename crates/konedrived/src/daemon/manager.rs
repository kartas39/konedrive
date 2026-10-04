use std::os::unix::fs::DirBuilderExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use konedrive_dbus::account_path;
use tokio::task::JoinHandle;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};
use zbus::Connection;
use konedrive_graph::oauth::Endpoints;

use crate::account::{AccountService, Siblings};
use crate::config::{is_valid_client_id, AccountConfig, AccountId, AccountPaths, ConfigError, ConfigStore, OnBattery, Paths};
use crate::account::secret::{AccountSecrets, Wallet};
use crate::account::state::SignInState;
use crate::desktop::baloo::Baloo;
use crate::helper::hub::HelperHub;
use crate::sync::registry::Registry;
use crate::conditions::running::HoldSettings;
use crate::sync::{OneDrive, Persist, SyncError, SyncPaths, SyncService, Transfers, Wiring};

/// The drive an account's folder shows, asked once as the account is made
/// ([`Options::drive`]).
pub type DriveOf = Arc<dyn Fn(&AccountService) -> anyhow::Result<Option<konedrive_graph::drive::DriveClient>> + Send + Sync>;

/// The account's own drive, in Graph, through its tokens: what `main` gives.
pub fn own_drive() -> DriveOf {
    Arc::new(|account| account.drive().map(Some))
}

/// No drive: every folder is a local one, filled with `PopulateFromDirectory`.
pub fn no_drive() -> DriveOf {
    Arc::new(|_| Ok(None))
}

/// What the daemon's accounts are made with. `main` gives Microsoft, the Secret Service, the
/// real `balooctl6`, the freedesktop thumbnail cache and each account's own drive; a test
/// gives wiremock, a wallet in memory (`account::testing::MemoryWallet`), [`Baloo::disabled`], no
/// thumbnails, and the drive its folders show, or none.
pub struct Options {
    pub endpoints: Endpoints,
    pub wallet: Arc<dyn Wallet>,
    pub sign_in_timeout: Duration,
    /// Each account's Baloo.
    pub baloo: fn() -> Baloo,
    /// The freedesktop thumbnail cache, shared by every account; `None` fills none.
    pub thumbnails: Option<PathBuf>,
    /// The drive a folder registered while signed in shows ([`own_drive`], [`no_drive`]).
    pub drive: DriveOf,
    /// How the objects get on the bus: `dbus::export::OnBus`, which `main` and every test
    /// give alike.
    pub bus: Arc<dyn Bus>,
}

/// The bus, as the daemon uses it: the manager's own objects and each account's are put on
/// it and taken off it by `dbus/`, whose `dbus::export::OnBus` is the one implementation.
#[async_trait]
pub trait Bus: Send + Sync {
    /// Puts `org.konedrive.Accounts` and `org.konedrive.Files` at `/org/konedrive/Accounts`,
    /// over `manager`, beside the `ObjectManager` the connection was built with
    /// (`daemon::startup`).
    async fn serve(&self, connection: &Connection, manager: &Arc<AccountManager>) -> zbus::Result<()>;
    /// What announces a change of `Accounts.HelperState`: the `Accounts` interface
    /// [`serve`](Self::serve) put on the bus, looked up once, at startup.
    async fn helper_state(&self, connection: &Connection) -> zbus::Result<Box<dyn HelperStateSignal>>;
    /// Puts an account's `Account` at `path`; the task that sends its signals.
    async fn export_account(&self, connection: &Connection, path: &ObjectPath<'_>, account: Arc<AccountService>) -> zbus::Result<JoinHandle<()>>;
    /// Puts the interfaces of an account's folder at `path`; the tasks that send their
    /// signals.
    async fn export_folder(&self, connection: &Connection, path: &ObjectPath<'_>, sync: Arc<SyncService>) -> zbus::Result<Vec<JoinHandle<()>>>;
    /// Takes the interfaces of an account's folder off the bus, every one whatever the one
    /// before answered. `partly`: some may not be there (an `Add` that failed while putting
    /// them), which is then not worth a warning.
    async fn unexport_folder(&self, connection: &Connection, path: &ObjectPath<'_>, partly: bool) -> zbus::Result<()>;
    /// Takes an account's `Account` off the bus, as [`unexport_folder`](Self::unexport_folder)
    /// takes the folder.
    async fn unexport_account(&self, connection: &Connection, path: &ObjectPath<'_>, partly: bool) -> zbus::Result<()>;
}

/// Announces changes of `Accounts.HelperState` ([`Bus::helper_state`]).
#[async_trait]
pub trait HelperStateSignal: Send + Sync {
    /// `PropertiesChanged` for `HelperState`.
    async fn changed(&self) -> zbus::Result<()>;
}

/// A path that is in no account's folder ([`AccountManager::route_all`]).
#[derive(Debug)]
pub struct Outside(pub String);

/// One account: its sign-in, its folder, and the tasks that turn their state into
/// `PropertiesChanged`.
pub struct Account {
    pub id: AccountId,
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
            ConfigError::NoAccount(id) => ManagerError::NoAccount(id.to_string()),
            other => ManagerError::Failed(other.to_string()),
        }
    }
}

/// The accounts, in the order they were added, and what they share: `config.toml`, the
/// registry of their folders with the helper hub, and the identity guard's view of every
/// account.
pub struct AccountManager {
    pub(crate) config: Arc<ConfigStore>,
    paths: Paths,
    options: Options,
    /// The accounts as their folders see each other. It is written only here, by
    /// [`list`](Self::list) and [`unlist`](Self::unlist), with `accounts`.
    registry: Arc<Registry>,
    siblings: Arc<Siblings>,
    accounts: Mutex<Vec<Arc<Account>>>,
    /// `Add`, `Remove`, `SetClientId`, `SetPauseOnMetered` and `SetOnBattery`, one at a
    /// time.
    changing: tokio::sync::Mutex<()>,
}

impl AccountManager {
    /// The registry takes the hold's settings from `config` now, before any account is
    /// listed.
    pub fn new(config: Arc<ConfigStore>, paths: Paths, options: Options, registry: Arc<Registry>) -> Arc<Self> {
        registry.set_hold_settings(HoldSettings::of(&config.snapshot()));
        Arc::new(Self {
            config,
            paths,
            options,
            registry,
            siblings: Arc::new(Siblings::default()),
            accounts: Mutex::new(Vec::new()),
            changing: tokio::sync::Mutex::new(()),
        })
    }

    pub fn config(&self) -> &Arc<ConfigStore> {
        &self.config
    }

    /// How the objects get on the bus.
    pub(crate) fn bus(&self) -> Arc<dyn Bus> {
        Arc::clone(&self.options.bus)
    }

    /// The link to the helper every account shares.
    pub fn hub(&self) -> &Arc<HelperHub> {
        self.registry.hub()
    }

    /// The accounts as their folders see each other: the same accounts as
    /// [`accounts`](Self::accounts), in the same order.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Makes `account` one of the daemon's, after every other: in the manager's list and
    /// in the registry, which nothing else writes.
    fn list(&self, account: Arc<Account>) {
        let mut accounts = self.accounts.lock().unwrap();
        self.registry.add(&account.sync);
        accounts.push(account);
    }

    /// Takes `account` out of both lists.
    fn unlist(&self, account: &Account) {
        let mut accounts = self.accounts.lock().unwrap();
        accounts.retain(|a| a.id != account.id);
        self.registry.remove(&account.id);
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
            match self.build(entry) {
                Ok(account) => {
                    // Listed before its folder is restored: the folder starts on the hold's
                    // settings every account runs on.
                    self.list(Arc::clone(&account));
                    account.account.startup().await;
                    // The folder starts in the mode the account does, before it is restored.
                    follow_mode(&account).await;
                    if let Some(why) = &held {
                        account.sync.hold_back(why).await;
                    }
                    account.sync.restore().await;
                }
                Err(e) => {
                    let message = format!("the account {:?} cannot be loaded: {e}", entry.label);
                    tracing::error!("{message}");
                    self.config.note_error(message);
                }
            }
        }
    }

    /// One account's services, wired as the daemon wires them, not yet listed and not yet
    /// on the bus.
    fn build(&self, entry: &AccountConfig) -> anyhow::Result<Arc<Account>> {
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
        let config = self.config.snapshot();
        // The folder asks its account for the sign-in, the one quota (issue #78), the mode
        // worked out again when its outbox worker finds the write gate closed, and tells it
        // of a drive that is not the folder's. A folder registered while signed in shows
        // OneDrive only with a drive to show, and a restored one starts syncing as it is
        // brought up.
        let onedrive = (self.options.drive)(&account)?.map(|drive| OneDrive {
            drive,
            paths: SyncPaths {
                tree_db: paths.tree_db.clone(),
                rescue_dir: paths.rescue_dir.clone(),
                thumbnails: self.options.thumbnails.clone(),
            },
        });
        let sync = SyncService::new(Wiring {
            onedrive,
            baloo: (self.options.baloo)(),
            transfers: Transfers { ceiling: config.transfer_ceiling(), large: config.transfer_large() },
            ..Wiring::new(Arc::clone(&self.registry), Arc::clone(&account) as Arc<dyn crate::account::FolderAccount>, persist)
        });
        // A switch to read-only asks the folder what waits to be uploaded (`docs/design/writes.md` §2).
        let uploads: std::sync::Weak<SyncService> = Arc::downgrade(&sync);
        account.set_uploads(uploads);
        Ok(Arc::new(Account { id: entry.id.clone(), path, account, sync, paths, signals: Mutex::new(Vec::new()) }))
    }

    /// Puts `account`'s objects on the bus.
    pub(crate) async fn export(&self, connection: &Connection, account: &Account) -> zbus::Result<()> {
        let path = account.path.as_ref();
        // Kept at once: when the folder cannot be put on the bus, whoever takes the account
        // off again stops this task with the others.
        let signals = self.options.bus.export_account(connection, &path, Arc::clone(&account.account)).await?;
        account.signals.lock().unwrap().push(signals);
        let signals = self.options.bus.export_folder(connection, &path, Arc::clone(&account.sync)).await?;
        account.signals.lock().unwrap().extend(signals);
        Ok(())
    }

    /// Takes `account`'s objects off the bus; `partly` when not all of them may be there.
    async fn unexport(&self, connection: &Connection, account: &Account, partly: bool) {
        let path = account.path.as_ref();
        for signals in account.signals.lock().unwrap().drain(..) {
            signals.abort();
        }
        if let Err(e) = self.options.bus.unexport_folder(connection, &path, partly).await {
            tracing::warn!("cannot take {path} off the bus: {e}");
        }
        if let Err(e) = self.options.bus.unexport_account(connection, &path, partly).await {
            tracing::warn!("cannot take {path} off the bus: {e}");
        }
    }

    /// `Accounts.Add`: a signed-out, read-only account with no folder, after every other,
    /// on the bus from the moment it is listed.
    pub async fn add(&self, label: &str, connection: &Connection) -> Result<Arc<Account>, ManagerError> {
        let _changing = self.changing.lock().await;
        let entry = self.config.add_account(label)?;
        let account = match self.build(&entry) {
            Ok(account) => account,
            Err(e) => {
                if let Err(e) = self.config.remove_account(&entry.id) {
                    tracing::warn!("cannot take the account {:?} out of config.toml again: {e}", entry.label);
                }
                return Err(ManagerError::Failed(e.to_string()));
            }
        };
        account.account.startup().await;
        follow_mode(&account).await;
        if let Err(e) = self.export(connection, &account).await {
            // Nothing of the account is to be left: not half of its objects on the bus, and
            // not an entry in `config.toml` that would come up as an account at the next
            // start (which stays all the same if the file cannot be written now: F205).
            self.unexport(connection, &account, true).await;
            self.siblings.remove(&account.id);
            if let Err(e) = self.config.remove_account(&entry.id) {
                tracing::warn!("cannot take the account {:?} out of config.toml again: {e}", entry.label);
            }
            remove_account_dir(&account.paths.dir);
            return Err(ManagerError::Failed(format!("cannot put the account on the bus: {e}")));
        }
        self.list(Arc::clone(&account));
        tracing::info!("added the account {:?} ({})", entry.label, entry.id);
        Ok(account)
    }

    /// `Accounts.Remove` (design §4.2): the folder forgotten exactly as
    /// `Folder.Unregister` forgets it — refused, before anything changes, under the same
    /// rule (`NoHelper` for an intercepted folder with no helper) — then a sign-in under way
    /// cancelled, the refresh token, the cached name and quota and the tree store deleted,
    /// the account taken out of `config.toml`, and its object off the bus. The folder's
    /// files and the rescued files are kept.
    ///
    /// A removal that fails after the account was retired — the sign-in cannot be deleted,
    /// or the account cannot be taken out of `config.toml` — leaves the account as an
    /// account: listed, taking a folder, a sign-in and a mode again, and removable again.
    /// What the steps before did is not taken back (a folder forgotten is forgotten at the
    /// helper, and a OneDrive folder's tree store is gone), and the refusal, under the name
    /// the failure always had, says what was done.
    pub async fn remove(&self, path: &ObjectPath<'_>, connection: &Connection) -> Result<(), ManagerError> {
        let _changing = self.changing.lock().await;
        let account = self.account(path).ok_or_else(|| ManagerError::NoAccount(path.to_string()))?;
        if self.config.is_poisoned() {
            return Err(ManagerError::Failed(self.config.last_error()));
        }
        // Retired first, each under its own lock, so that no call on the account's own
        // objects can register a folder or store a sign-in in between: the folder
        // forgotten (a held account's through the helper too), then the sign-in.
        let forgotten = account.sync.retire().await?;
        let failed = match account.account.retire().await {
            Err(e) => Some((ManagerError::Failed(format!("cannot delete the sign-in: {e}")), false)),
            Ok(()) => self.config.remove_account(&account.id).err().map(|e| (ManagerError::from(e), true)),
        };
        if let Some((error, signed_out)) = failed {
            account.account.unretire();
            account.sync.unretire().await;
            let left = HalfRemoved {
                forgotten,
                still_recorded: self.config.account(&account.id).is_some_and(|a| a.root.is_some()),
                signed_out,
            };
            let error = left.refusal(&account.account.state().get().label, error);
            tracing::error!("{error}");
            return Err(error);
        }
        remove_account_dir(&account.paths.dir);
        self.unexport(connection, &account, false).await;
        self.unlist(&account);
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
        self.registry.hold_settings()
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
        self.registry.set_hold_settings(hold);
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
    /// first named; the first path in no account's folder (`OutsideRoot` on the bus), before
    /// anything is done.
    pub(crate) async fn route_all(&self, paths: &[String]) -> Result<Vec<(Arc<Account>, Vec<PathBuf>)>, Outside> {
        let mut groups: Vec<(Arc<Account>, Vec<PathBuf>)> = Vec::new();
        for path in paths {
            let account = self.route(Path::new(path)).await.ok_or_else(|| Outside(path.clone()))?;
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

/// What an `Accounts.Remove` that failed after the account was retired had done by then.
struct HalfRemoved {
    /// The folder that was forgotten, if one was.
    forgotten: Option<PathBuf>,
    /// Whether `config.toml`, as the daemon holds it, still records a folder for the account.
    still_recorded: bool,
    /// Whether the sign-in was deleted.
    signed_out: bool,
}

impl HalfRemoved {
    /// `error`, which is what failed, under the same name and saying what the removal had
    /// done by then.
    fn refusal(&self, label: &str, error: ManagerError) -> ManagerError {
        match error {
            ManagerError::InvalidArgs(why) => ManagerError::InvalidArgs(self.text(label, &why)),
            ManagerError::Failed(why) => ManagerError::Failed(self.text(label, &why)),
            ManagerError::NoAccount(id) => ManagerError::NoAccount(self.gone_from_config(label, &id)),
            sync @ ManagerError::Sync(_) => sync,
        }
    }

    /// What follows "there is no account " when `config.toml` no longer holds the account
    /// (taken out by hand while the daemon runs): the third step's failure, so the sign-in is
    /// deleted by then. Neither another `Remove` nor going on with the account works, and
    /// what the daemon's copy of the file records is not what the file says, so neither is
    /// said.
    fn gone_from_config(&self, label: &str, id: &str) -> String {
        let folder = match &self.forgotten {
            Some(folder) => format!(" and its folder {} forgotten (the files in it are kept)", folder.display()),
            None => String::new(),
        };
        format!(
            "{id:?} in config.toml any more, so the account {label:?} cannot be taken out of it; its sign-in is \
             deleted{folder}. Start konedrived again and the account is gone"
        )
    }

    /// What the removal answers, `why` being what failed, while `config.toml` still holds
    /// the account.
    fn text(&self, label: &str, why: &str) -> String {
        let mut left = String::from("the account stays");
        if self.signed_out {
            left.push_str(", signed out");
        }
        match (&self.forgotten, self.still_recorded) {
            (Some(folder), false) => {
                left.push_str(&format!(", and its folder {} is no longer registered (the files in it are kept)", folder.display()));
            }
            (Some(folder), true) => left.push_str(&format!(
                ", and its folder {} was forgotten (the files in it are kept), but config.toml still records it, so it \
                 may be taken for registered again",
                folder.display()
            )),
            (None, true) => left.push_str(", and its folder is left as it was"),
            (None, false) => {}
        }
        format!("the account {label:?} is not removed: {why}; {left}. Remove it again, or go on using it")
    }
}

/// Removes an account's own directory, with everything the daemon kept in it.
fn remove_account_dir(dir: &Path) {
    if let Err(e) = std::fs::remove_dir_all(dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("cannot remove {}: {e}", dir.display());
        }
    }
}

/// Makes `account`'s folder follow the mode the account runs in (`docs/design/writes.md` §2): it starts
/// in the account's mode now, and a task switches it whenever `Account.Mode` changes. The
/// task goes with the account's other tasks when it is removed.
async fn follow_mode(account: &Account) {
    let changes = account.account.state().subscribe();
    let mode = changes.borrow().mode;
    account.sync.follow_mode(mode).await;
    let follower = tokio::spawn(crate::sync::mode::follow(changes, Arc::downgrade(&account.sync)));
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

#[cfg(test)]
mod tests;
