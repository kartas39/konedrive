//! Sign-in state machine behind the D-Bus interface: one Microsoft account.
//!
//! # The order of the locks
//!
//! An [`AccountService`] works under these locks, and takes them only in this order, outer
//! first. Whoever holds a later one never asks for an earlier one.
//!
//! 1. **`session`** (async): one sign-in attempt, cancel, sign-out, commit or account-info
//!    write at a time. Held across the wallet (`commit_sign_in` and `commit_read_write`
//!    store the refresh token under it, which can show the wallet's unlock prompt, and
//!    `sign_out` deletes it): on purpose, so that an attempt is checked and stored in one
//!    step. So nothing that must answer at once waits for it: `begin_sign_in` and the
//!    switch to read-write refuse what they can before they ask for it.
//! 2. **The token manager's refresh lock** (async, `konedrive_graph::token`): taken under
//!    `session` by `forget`, `seed_as` and `commit_as`, and alone by every refresh. What a
//!    refresh calls back (`note_granted`) and what `commit_as` runs under it take only the
//!    locks below, never `session`.
//!    - The account's folder is asked under `session` too (`PendingUploads::quota_read`, from
//!      `refresh_account_info`): what answers there never calls back into what takes
//!      `session` (a sign-out, a switch of the mode).
//!    - Between accounts: no account's `session` is held while another account is asked.
//!      The identity guard asks the other accounts for their drives (`settle_siblings`: their
//!      refresh lock, then `ConfigStore`'s) before it takes its own `session`.
//! 3. **The leaves**, none held while another is asked for, none held across an `await`:
//!    - `ConfigStore`'s lock, across one read or one read-modify-write of `config.toml`; the
//!      change it runs touches the configuration and nothing else;
//!    - `cache_lock`, across one read-modify-write of `account.json` (`save_cache`, the
//!      quota's `keep_quota`);
//!    - the state (`StateHandle::update`): the change it runs only assigns, and what it
//!      needs from `config.toml` or `account.json` is read before or written after;
//!    - `uploads` and `siblings`, each for one read or one assignment;
//!    - the token manager's own (`oauth`, the cached tokens, the grant's hook), each for one
//!      read or one assignment, under either of the two above (`install_oauth`).
//!
//! The file writes of the leaves are blocking and `fsync`ed, also where they run under the
//! refresh lock on a runtime thread (limitations log F231, F249).

pub mod cache;
mod mode;
pub mod quota;
pub mod secret;
mod sign_in;
pub mod state;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{oneshot, Mutex};

use crate::account::cache::AccountInfo;
use crate::account::quota::Quota;
use crate::config::{AccountId, AccountPaths, Config, ConfigError, ConfigStore, DriveId, Mode, WriteStanding};
use konedrive_graph::drive::{DriveClient, Status};
use konedrive_graph::loopback::{Callback, LoopbackError, LoopbackListener};
use konedrive_graph::oauth::{grants_writes, is_read_only, scopes_for, Endpoints, OAuthClient, TokenResponse};
use konedrive_graph::pkce::{random_token, Pkce};
use crate::account::secret::SecretStore;
use crate::account::state::{AccountSnapshot, ModeNote, SignInState, StateHandle};
use konedrive_graph::token::{AuthError, TokenManager};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccountError {
    #[error("invalid client ID: expected a GUID like 00000000-0000-0000-0000-000000000000")]
    InvalidClientId,
    /// A label `config::check_label` refuses, with the reason.
    #[error("{0}")]
    InvalidLabel(String),
    #[error("not possible right now: finish or cancel the current sign-in, or sign out first")]
    Busy,
    #[error("set a client ID first")]
    NoClientId,
    #[error("{0}")]
    Failed(String),
}

impl From<ConfigError> for AccountError {
    fn from(error: ConfigError) -> Self {
        match error {
            ConfigError::InvalidClientId => AccountError::InvalidClientId,
            ConfigError::InvalidLabel(why) => AccountError::InvalidLabel(why),
            other => AccountError::Failed(format!("cannot save the configuration: {other}")),
        }
    }
}

/// Why `Account.SetMode` or a `TokenExport` token was refused (`docs/design/writes.md` §11), each under its own
/// D-Bus error name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModeError {
    /// Not `read-only` or `read-write` (`InvalidArgs`).
    #[error("{0}")]
    InvalidMode(String),
    /// The development gate: the account's drive is not in `write_test_drive_ids`.
    #[error("{0}")]
    WritesNotAllowed(String),
    /// The account's token does not carry `Files.ReadWrite`.
    #[error("{0}")]
    ModeNotGranted(String),
    /// Changes wait to be uploaded, and the switch to read-only was not forced.
    #[error("{0}")]
    PendingUploads(String),
    #[error("{0}")]
    NotSignedIn(String),
    #[error("{0}")]
    Failed(String),
}

/// What `SetMode` refuses for the development gate (`docs/design/writes.md` §2.3).
pub const WRITES_NOT_ALLOWED: &str = "while uploads are being developed, only the test accounts \
     listed in write_test_drive_ids in config.toml can be read-write, and this account's drive is not \
     one of them; it stays read-only";

/// `LastError` of an account `config.toml` sets to read-write whose drive the gate does not let
/// through: a hand edit. It runs read-only.
pub const GATE_KEEPS_READ_ONLY: &str = "config.toml sets this account to read-write, but while uploads \
     are being developed only the test accounts in write_test_drive_ids can be; it runs read-only";

/// `LastError` of a read-write account whose token does not carry `Files.ReadWrite` (write
/// design §7). It runs read-only until it signs in with that permission.
pub const SIGN_IN_TO_WRITE: &str = "Changes made here are not uploaded: this account's sign-in does \
     not allow konedrive to change files in OneDrive. Switch it to read-write again to sign in with \
     that permission.";

/// `LastError` of a read-write account whose token has not been seen to reach its recorded
/// drive since `account.json` was written without it. It runs read-only until it is.
pub const DRIVE_NOT_SEEN: &str = "config.toml sets this account to read-write, but which OneDrive \
     its sign-in reaches has not been checked yet; it runs read-only until it is";

/// `LastError` of a read-write account while `config.toml` cannot be read: the
/// mode and the gate are read from the file each time, and fail closed.
pub const CONFIG_UNREADABLE: &str = "config.toml cannot be read now, so this account runs read-only \
     until it can";

/// The start of `LastError` for a sign-in that reaches another drive than the one
/// `config.toml` records ([`ModeNote::DriveMismatch`]).
const DRIVE_MISMATCH: &str = "this account's sign-in reaches the OneDrive drive";

/// The start of `LastError` for a read-only request answered with more
/// ([`ModeNote::WiderGrant`]).
const WIDER_GRANT: &str = "Microsoft answered a request for read-only access with a token that can";

/// What the switch between the modes asks of the account's folder (`docs/design/writes.md` §2): how many
/// changes wait to be uploaded, and dropping them when a switch to read-only is forced — the
/// files stay, as ordinary local changes. The account's `SyncService` answers
/// (`crate::sync::write_mode`); until the outbox exists (the examination and the outbox worker), nothing waits.
#[async_trait::async_trait]
pub trait PendingUploads: Send + Sync {
    async fn pending_uploads(&self) -> u64;
    async fn drop_pending_uploads(&self);
    /// The account's quota was just read (`RefreshInfo`): a full OneDrive, or a file
    /// too big for what was left, is decided again by it (issue #2).
    fn quota_read(&self, _quota: &konedrive_graph::drive::DriveQuota) {}
}

/// What an account's folder asks of its account (`crate::sync`): the one thing the folder
/// is made with for all of it. The daemon's is the [`AccountService`].
pub trait FolderAccount: Send + Sync {
    /// The account's state as published: whether somebody is signed in, the mode it runs
    /// in, what its sign-in grants, and the drive its token was last seen to reach.
    fn snapshot(&self) -> state::AccountSnapshot;
    /// The state as it changes: the folder asks OneDrive again at a sign-in.
    fn changes(&self) -> tokio::sync::watch::Receiver<state::AccountSnapshot>;
    /// The account's one quota, which the uploads' space check reads and adjusts.
    fn quota(&self) -> Quota;
    /// Works the account's mode out again: the write gate closed under the folder's outbox
    /// worker, before anything on the account's side saw why.
    fn recheck_mode(&self);
    /// The folder's cycle saw the account's token reach `drive`, which is not the drive
    /// the folder was listed from.
    fn drive_seen(&self, drive: &str);
}

impl FolderAccount for AccountService {
    fn snapshot(&self) -> state::AccountSnapshot {
        self.state.get()
    }

    fn changes(&self) -> tokio::sync::watch::Receiver<state::AccountSnapshot> {
        self.state.subscribe()
    }

    fn quota(&self) -> Quota {
        self.quota.clone()
    }

    fn recheck_mode(&self) {
        self.recompute_mode();
    }

    fn drive_seen(&self, drive: &str) {
        AccountService::drive_seen(self, drive);
    }
}

/// Serializes sign-in commits, cancellation and sign-out against each other. `generation`
/// identifies the current sign-in attempt (if any): a spawned attempt only writes state
/// while holding this lock and only if the generation it was started with is still
/// current, so a superseded attempt (cancelled, signed out, or replaced by a newer
/// `begin_sign_in`) can never resurrect state after the fact. An attempt also starts under this
/// lock, in one step with the state it starts from (`begin_sign_in`, the switch to
/// read-write), and commits only while the account still shows that attempt's state.
struct Session {
    generation: u64,
    /// Interrupts a pending `listener.wait()`. Only ever `Some` for the current attempt.
    cancel: Option<oneshot::Sender<()>>,
}

/// One account: its sign-in, its tokens, its mode and its cached name and quota. Its entry in
/// `config.toml` (label, mode, drive) is read and written through the daemon's one
/// [`ConfigStore`].
pub struct AccountService {
    id: AccountId,
    config: Arc<ConfigStore>,
    state: StateHandle,
    /// `account.json`: the cached name and quota, and what the last token was valid for.
    cache: PathBuf,
    /// Held across every read-modify-write of `account.json`: a refresh's granted scopes,
    /// `refresh_account_info`'s name and every read of the quota each keep the others'.
    cache_lock: Arc<std::sync::Mutex<()>>,
    /// The account's one quota (`crate::account::quota`), in `state`, kept in `account.json` at every
    /// read, whoever reads it.
    quota: Quota,
    /// The account's folder, as the mode switch asks it about waiting uploads. Empty until
    /// the accounts manager wires the folder up, and in tests without one.
    uploads: std::sync::Mutex<Option<Weak<dyn PendingUploads>>>,
    endpoints: Endpoints,
    http: reqwest::Client,
    /// Graph, for [`Self::graph`]. Its own transfer pool, which nothing else uses.
    graph: DriveClient,
    secrets: Arc<dyn SecretStore>,
    tokens: Arc<TokenManager>,
    sign_in_timeout: Duration,
    session: Mutex<Session>,
    /// The daemon's other accounts, for the identity guard (§8.2). Empty until a
    /// [`Siblings`] adds this account.
    siblings: std::sync::Mutex<Option<Arc<Siblings>>>,
    /// Set by `Accounts.Remove`: no sign-in is begun or stored from then on.
    retired: std::sync::atomic::AtomicBool,
}

struct SignInAttempt {
    oauth: OAuthClient,
    listener: LoopbackListener,
    redirect_uri: String,
    pkce: Pkce,
    csrf: String,
    cancel: oneshot::Receiver<()>,
    generation: u64,
}

/// Who a sign-in turned out to be (design §8.2): the drive, and the email when `/me`
/// answered.
struct Identity {
    drive: DriveId,
    email: Option<String>,
}

impl AccountService {
    /// Account `id` of `config`, with its files at `paths` and its refresh token in
    /// `secrets`. Its label and mode are what `config.toml` says now; the client id is the
    /// one every account shares.
    pub fn new(
        config: Arc<ConfigStore>,
        id: &AccountId,
        paths: AccountPaths,
        endpoints: Endpoints,
        secrets: Arc<dyn SecretStore>,
        sign_in_timeout: Duration,
    ) -> anyhow::Result<Arc<Self>> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("konedrive/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .build()?;
        let label = config.account(id).map(|a| a.label).unwrap_or_default();
        let client_id = config.client_id();
        // Read-only until startup has read what the last token was valid for.
        let state = StateHandle::new(AccountSnapshot {
            client_id: client_id.clone(),
            label,
            ..AccountSnapshot::default()
        });
        let tokens = Arc::new(TokenManager::new(secrets.clone(), state.clone()));
        let graph = DriveClient::new(endpoints.graph.clone(), Arc::clone(&tokens) as Arc<dyn konedrive_graph::token::TokenSource>)?;
        let cache_lock = Arc::new(std::sync::Mutex::new(()));
        let quota = Quota::new(state.clone(), Some(keep_quota(paths.account_cache.clone(), Arc::clone(&cache_lock))));
        let service = Arc::new(Self {
            id: id.clone(),
            config,
            state,
            cache: paths.account_cache,
            cache_lock,
            quota,
            uploads: std::sync::Mutex::new(None),
            endpoints,
            http,
            graph,
            secrets,
            tokens,
            sign_in_timeout,
            session: Mutex::new(Session { generation: 0, cancel: None }),
            siblings: std::sync::Mutex::new(None),
            retired: std::sync::atomic::AtomicBool::new(false),
        });
        service.install_oauth(&client_id);
        // Every refresh says what its token is valid for (`docs/design/writes.md` §2). Weak: the token
        // manager is the service's own.
        let me = Arc::downgrade(&service);
        service.tokens.set_on_granted(Arc::new(move |asked: &'static str, granted: &str| {
            if let Some(service) = me.upgrade() {
                service.note_granted(asked, granted);
            }
        }));
        Ok(service)
    }

    pub fn id(&self) -> &AccountId {
        &self.id
    }

    /// `Account.Mode`: the mode the account runs in (`docs/design/writes.md` §2). Read-write only while
    /// `config.toml` says so, the gate lets its drive through, and its last token carried
    /// `Files.ReadWrite`; [`recompute_mode`](Self::recompute_mode) keeps it.
    pub fn mode(&self) -> Mode {
        self.state.get().mode
    }

    /// The mode `config.toml` gives the account now — read again, as the gate is:
    /// the user's choice, which it runs in only when the gate and its token allow
    /// ([`mode`](Self::mode)). A file that cannot be read now reads as read-only.
    pub fn configured_mode(&self) -> Mode {
        self.config.write_standing(&self.id).map(|standing| standing.mode).unwrap_or_default()
    }

    /// What a sign-in asks for: read-write when `config.toml` says so and the gate lets the
    /// drive through, both from one reading of the file, so that signing in again keeps a
    /// read-write account read-write.
    fn sign_in_mode(&self) -> Mode {
        match self.config.write_standing(&self.id) {
            Some(standing) if standing.allows_writes() => Mode::ReadWrite,
            _ => Mode::ReadOnly,
        }
    }

    /// The account's folder, for the mode switch's question about waiting uploads.
    pub fn set_uploads(&self, uploads: Weak<dyn PendingUploads>) {
        *self.uploads.lock().unwrap() = Some(uploads);
    }

    pub fn config(&self) -> &Arc<ConfigStore> {
        &self.config
    }

    pub fn state(&self) -> &StateHandle {
        &self.state
    }

    /// The account's one quota, which its folder's uploads read and adjust too.
    pub fn quota(&self) -> &Quota {
        &self.quota
    }

    /// Access tokens for Graph callers.
    pub fn tokens(&self) -> &Arc<TokenManager> {
        &self.tokens
    }

    /// A read-only Graph drive client for the sync side, on this account's
    /// tokens.
    pub fn drive(&self) -> anyhow::Result<konedrive_graph::drive::DriveClient> {
        konedrive_graph::drive::DriveClient::new(self.endpoints.graph.clone(), Arc::clone(&self.tokens) as Arc<dyn konedrive_graph::token::TokenSource>)
    }

    /// The client the account's own questions go through (who is signed in, which drive it
    /// is): each is asked with the token its caller holds.
    fn graph(&self) -> &DriveClient {
        &self.graph
    }

    /// A client asking for `mode`'s scope, for the authorization, the code exchange or a
    /// refresh.
    fn oauth_client(&self, client_id: &str, mode: Mode) -> OAuthClient {
        OAuthClient::new(self.http.clone(), self.endpoints.clone(), client_id.to_owned()).with_scope(scopes_for(mode == Mode::ReadWrite))
    }

    /// Every refresh asks for the scope of the mode the account runs in. A read-write account
    /// whose token lost `Files.ReadWrite` asks for `Files.Read`: a subset of what it was
    /// granted, which Microsoft never refuses, where asking for more than the grant would
    /// fail as `invalid_grant` and sign the account out.
    fn install_oauth(&self, client_id: &str) {
        self.tokens
            .set_oauth((!client_id.is_empty()).then(|| self.oauth_client(client_id, self.mode())));
    }

    /// Works out the mode the account runs in (`docs/design/writes.md` §2); publishes it, says in
    /// `LastError` why an account runs read-only against `config.toml` — or holds more than
    /// it asked for — and makes every refresh ask for that mode's scope. Read-write needs
    /// all of:
    ///
    /// - `config.toml` saying read-write;
    /// - the gate letting the recorded drive through, as the file says now;
    /// - the drive the account's token was last seen to reach being that very drive (review
    ///   I2: a recorded drive is never trusted on its own);
    /// - its last token carrying `Files.ReadWrite`.
    ///
    /// Called whenever one of them may have changed, the folder's outbox worker finding the
    /// write gate closed included ([`FolderAccount::recheck_mode`]). The folder follows
    /// `Mode` (`crate::sync::write_mode::follow`).
    fn recompute_mode(&self) {
        // The mode and the list it is gated by, from one reading of the file. A
        // file that cannot be read now fails closed: read-only, and `LastError` says why when
        // the file last read said read-write.
        let standing = self.config.write_standing(&self.id);
        let unreadable = standing.is_none() && self.config.account(&self.id).is_some_and(|a| a.mode == Mode::ReadWrite);
        let WriteStanding { mode: configured, writable_drive: allowed } =
            standing.unwrap_or(WriteStanding { mode: Mode::ReadOnly, writable_drive: None });
        self.state.update(|s| {
            let granted = grants_writes(&s.granted_scopes);
            let same_drive = allowed.as_ref().is_some_and(|drive| *drive == s.live_drive);
            s.mode = if configured == Mode::ReadWrite && same_drive && granted { Mode::ReadWrite } else { Mode::ReadOnly };
            let signed_in = s.state == SignInState::SignedIn;
            let why = if signed_in && !s.wider_grant.is_empty() {
                Some(ModeNote::WiderGrant(s.wider_grant.clone()))
            } else if signed_in && unreadable {
                Some(ModeNote::ConfigUnreadable)
            } else if signed_in && configured == Mode::ReadWrite {
                match &allowed {
                    None => Some(ModeNote::GateKeepsReadOnly),
                    // Another drive seen comes first: signing in again cannot cure it.
                    Some(drive) if !s.live_drive.is_empty() && !same_drive => {
                        Some(ModeNote::DriveMismatch { live: s.live_drive.clone(), recorded: drive.to_string() })
                    }
                    Some(_) if !granted => Some(ModeNote::SignInToWrite),
                    Some(_) if s.live_drive.is_empty() => Some(ModeNote::DriveNotSeen),
                    Some(_) => None,
                }
            } else {
                None
            };
            // The note takes the place of what `LastError` said; with no reason left, only
            // the note goes, and an error said since stays.
            s.set_mode_note(why);
        });
        self.install_oauth(&self.state.get().client_id);
    }

    /// A token asked for with `asked` turned out valid for `granted` (`docs/design/writes.md` §2):
    /// recorded, and the mode worked out again. The token manager calls this after every
    /// refresh, with its refresh lock held.
    fn note_granted(&self, asked: &str, granted: &str) {
        self.record_granted(asked, granted);
        self.recompute_mode();
    }

    /// Keeps what a token may be used for in the state and in `account.json`: what it was
    /// granted, but never more than was asked for. A read-only request answered
    /// with a token that can write — consent Microsoft still holds — is logged, shown in
    /// `LastError`, and used to read only; `TokenExport` hands such a token to nobody.
    fn record_granted(&self, asked: &str, granted: &str) {
        let wider = is_read_only(asked) && !is_read_only(granted);
        let usable = if wider { asked } else { granted };
        if wider {
            tracing::warn!(
                "account {:?}: a request for {asked:?} was answered with a token valid for {granted:?}; it is used to read only",
                self.id
            );
        }
        let mut changed = false;
        self.state.update(|s| {
            s.wider_grant = if wider { granted.to_owned() } else { String::new() };
            if s.granted_scopes != usable {
                s.granted_scopes = usable.to_owned();
                changed = true;
            }
        });
        if changed {
            self.save_cache(|info| info.granted_scopes = usable.to_owned());
        }
    }

    /// A folder's cycle saw the account's token reach `drive`, which is not the drive the folder
    /// was listed from: recorded, and the mode worked out again, so that a
    /// read-write account turns read-only rather than writing on as before.
    pub fn drive_seen(&self, drive: &str) {
        self.record_live_drive(drive);
        self.recompute_mode();
    }

    /// The account's token was seen to reach `drive` (`GET /me/drive`): kept in the state and
    /// in `account.json`. The mode is the caller's to work out again.
    fn record_live_drive(&self, drive: &str) {
        let mut changed = false;
        self.state.update(|s| {
            if s.live_drive != drive {
                s.live_drive = drive.to_owned();
                changed = true;
            }
        });
        if changed {
            self.save_cache(|info| info.drive_id = drive.to_owned());
        }
    }

    /// One read-modify-write of `account.json`, under its lock.
    fn save_cache(&self, change: impl FnOnce(&mut AccountInfo)) {
        let _cache = self.cache_lock.lock().unwrap();
        let mut info = crate::account::cache::load(&self.cache).unwrap_or_default();
        change(&mut info);
        if let Err(e) = crate::account::cache::save(&self.cache, &info) {
            tracing::warn!("cannot write the account cache: {e}");
        }
    }

    fn siblings(&self) -> Option<Arc<Siblings>> {
        self.siblings.lock().unwrap().clone()
    }

    /// Restores the session from the wallet (existence check only) and the account cache.
    pub async fn startup(self: &Arc<Self>) {
        match self.secrets.exists().await {
            Ok(true) => {
                let cached = crate::account::cache::load(&self.cache);
                if let Some(info) = &cached {
                    self.secrets.describe(&info.email);
                }
                // Conditional: a sign-in started in the window between claiming the bus
                // name and this call (D-Bus activation) must not be clobbered back to
                // `SignedIn` by a wallet check that predates it. The cached info is still
                // published either way, atomically with the (possibly no-op) transition.
                self.state.update(|s| {
                    if s.state == SignInState::SignedOut {
                        s.state = SignInState::SignedIn;
                    }
                    if let Some(info) = &cached {
                        apply_info(s, info);
                        // What the last token was valid for decides whether a read-write
                        // account's first refresh may ask for Files.ReadWrite again (§7). No
                        // cache, or one from before it was kept: nothing granted, read-only.
                        s.granted_scopes = info.granted_scopes.clone();
                        // And the drive the token reached, which must be the one
                        // config.toml records.
                        s.live_drive = info.drive_id.clone();
                    }
                });
                self.recompute_mode();
                let this = Arc::clone(self);
                tokio::spawn(async move { this.refresh_account_info().await });
            }
            Ok(false) => crate::account::cache::remove(&self.cache),
            Err(e) => self.state.update(|s| s.set_error(e.to_string())),
        }
    }

    /// Signs in with `id` from now on (already validated and saved).
    pub fn use_client_id(&self, id: &str) {
        self.install_oauth(id);
        self.state.update(|s| s.client_id = id.to_owned());
    }

    /// `Account.SetLabel`: the rules of `config::check_label`, `InvalidLabel` otherwise.
    pub fn set_label(&self, label: &str) -> Result<(), AccountError> {
        let label = self.config.set_label(&self.id, label)?;
        self.state.update(|s| s.label = label);
        Ok(())
    }

    /// The account as a refusal names it: its email, or its label while there is none.
    fn who(&self) -> String {
        let s = self.state.get();
        if s.email.is_empty() {
            format!("'{}'", s.label)
        } else {
            s.email
        }
    }
}

/// The accounts of one daemon, as the identity guard sees them (design §8.2): a sign-in
/// compares its drive with every other account's.
#[derive(Default)]
pub struct Siblings(std::sync::Mutex<Vec<Weak<AccountService>>>);

impl Siblings {
    /// `account` is one of them from now on.
    pub fn add(self: &Arc<Self>, account: &Arc<AccountService>) {
        self.0.lock().unwrap().push(Arc::downgrade(account));
        *account.siblings.lock().unwrap() = Some(Arc::clone(self));
    }

    /// Account `id` is not one of them any more.
    pub fn remove(&self, id: &AccountId) {
        self.0.lock().unwrap().retain(|a| a.upgrade().is_some_and(|a| a.id != *id));
    }

    fn others(&self, id: &AccountId) -> Vec<Arc<AccountService>> {
        self.0.lock().unwrap().iter().filter_map(Weak::upgrade).filter(|a| a.id != *id).collect()
    }
}

fn apply_info(s: &mut AccountSnapshot, info: &AccountInfo) {
    s.display_name = info.display_name.clone();
    s.email = info.email.clone();
    s.quota = info.quota.clone();
}

/// What keeps the account's quota across restarts: at every read, its figures into `account.json` (`cache`, under `lock`), while the account is signed in — a read
/// that lands after a sign-out, which deletes the file, writes nothing.
fn keep_quota(cache: PathBuf, lock: Arc<std::sync::Mutex<()>>) -> crate::account::quota::Keep {
    Arc::new(move |s: &AccountSnapshot| {
        if s.state != SignInState::SignedIn {
            return;
        }
        let _cache = lock.lock().unwrap();
        let mut info = crate::account::cache::load(&cache).unwrap_or_default();
        info.quota = s.quota.clone();
        if let Err(e) = crate::account::cache::save(&cache, &info) {
            tracing::warn!("cannot write the account cache: {e}");
        }
    })
}

/// What a sign-in is refused with when Graph names no drive.
const NO_DRIVE: &str = "Microsoft Graph did not say which drive this is";

/// What a retired account answers a sign-in.
const RETIRED: &str = "this account is being removed";

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
