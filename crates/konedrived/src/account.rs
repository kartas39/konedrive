//! Sign-in state machine behind the D-Bus interface: one Microsoft account.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{oneshot, Mutex};

use crate::account_cache::{self, AccountInfo};
use crate::config::{is_valid_client_id, AccountPaths, Config, ConfigError, ConfigStore, Mode, Paths, MIGRATED_LABEL};
use crate::graph::{GraphClient, GraphError};
use crate::loopback::{Callback, LoopbackError, LoopbackListener};
use crate::oauth::{grants_writes, is_read_only, scopes_for, Endpoints, OAuthClient, TokenResponse};
use crate::pkce::{random_token, Pkce};
use crate::secret::SecretStore;
use crate::state::{AccountSnapshot, SignInState, StateHandle};
use crate::token::{AuthError, TokenManager};

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

/// Why `Account1.SetMode` or a `Dev1` token was refused (`docs/design/writes.md` §11), each under its own
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
/// `config.toml` records.
const DRIVE_MISMATCH: &str = "this account's sign-in reaches the OneDrive drive";

fn drive_mismatch_message(live: &str, recorded: &str) -> String {
    format!(
        "{DRIVE_MISMATCH} {live}, but config.toml records drive {recorded} for it; it runs read-only \
         until the two agree"
    )
}

/// The start of `LastError` for a read-only request answered with more.
const WIDER_GRANT: &str = "Microsoft answered a request for read-only access with a token that can";

fn wider_grant_message(granted: &str) -> String {
    format!(
        "{WIDER_GRANT} also change files ({granted}); konedrive uses it to read only. The consent \
         stays with Microsoft until it is revoked at https://account.live.com/consent/Manage"
    )
}

/// Whether `text` is one of the messages [`AccountService::recompute_mode`] sets, and so
/// takes back when its reason is gone.
fn is_mode_message(text: &str) -> bool {
    [GATE_KEEPS_READ_ONLY, SIGN_IN_TO_WRITE, DRIVE_NOT_SEEN, CONFIG_UNREADABLE].contains(&text)
        || text.starts_with(DRIVE_MISMATCH)
        || text.starts_with(WIDER_GRANT)
}

/// What the switch between the modes asks of the account's folder (`docs/design/writes.md` §2): how many
/// changes wait to be uploaded, and dropping them when a switch to read-only is forced — the
/// files stay, as ordinary local changes. The account's `SyncService` answers
/// (`crate::sync::write_mode`); until the outbox exists (the examination and the outbox worker), nothing waits.
#[async_trait::async_trait]
pub trait PendingUploads: Send + Sync {
    async fn pending_uploads(&self) -> u64;
    async fn drop_pending_uploads(&self);
}

/// Serializes sign-in commits, cancellation and sign-out against each other. `generation`
/// identifies the current sign-in attempt (if any): a spawned attempt only writes state
/// while holding this lock and only if the generation it was started with is still
/// current, so a superseded attempt (cancelled, signed out, or replaced by a newer
/// `begin_sign_in`) can never resurrect state after the fact.
struct Session {
    generation: u64,
    /// Interrupts a pending `listener.wait()`. Only ever `Some` for the current attempt.
    cancel: Option<oneshot::Sender<()>>,
}

/// One account: its sign-in, its tokens, its mode and its cached name and quota. Its entry in
/// `config.toml` (label, mode, drive) is read and written through the daemon's one
/// [`ConfigStore`].
pub struct AccountService {
    id: String,
    config: Arc<ConfigStore>,
    state: StateHandle,
    /// `account.json`: the cached name and quota, and what the last token was valid for.
    cache: PathBuf,
    /// Held across every read-modify-write of `account.json`: a refresh's granted scopes and
    /// `refresh_account_info`'s name and quota each keep the other's.
    cache_lock: std::sync::Mutex<()>,
    /// The account's folder, as the mode switch asks it about waiting uploads. Empty until
    /// the accounts manager wires the folder up, and in tests without one.
    uploads: std::sync::Mutex<Option<Weak<dyn PendingUploads>>>,
    endpoints: Endpoints,
    http: reqwest::Client,
    secrets: Arc<dyn SecretStore>,
    tokens: Arc<TokenManager>,
    sign_in_timeout: Duration,
    session: Mutex<Session>,
    /// The daemon's other accounts, for the identity guard (§8.2). Empty until a
    /// [`Siblings`] adds this account.
    siblings: std::sync::Mutex<Option<Arc<Siblings>>>,
    /// Set by `Accounts1.Remove`: no sign-in is begun or stored from then on.
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
    drive: String,
    email: Option<String>,
}

impl AccountService {
    /// Account `id` of `config`, with its files at `paths` and its refresh token in
    /// `secrets`. Its label and mode are what `config.toml` says now; the client id is the
    /// one every account shares.
    pub fn new(
        config: Arc<ConfigStore>,
        id: &str,
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
        let service = Arc::new(Self {
            id: id.to_owned(),
            config,
            state,
            cache: paths.account_cache,
            cache_lock: std::sync::Mutex::new(()),
            uploads: std::sync::Mutex::new(None),
            endpoints,
            http,
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

    /// One account in a configuration of its own under `dir` (`config.toml`, and the
    /// account's files in `accounts/<id>/`): the configuration's first account, or a new
    /// one called `Personal`. For tests and tools that run no accounts manager.
    pub async fn single(
        dir: &Path,
        endpoints: Endpoints,
        secrets: Arc<dyn SecretStore>,
        sign_in_timeout: Duration,
    ) -> anyhow::Result<Arc<Self>> {
        let paths = Paths::in_dir(dir);
        let config = Arc::new(ConfigStore::open(&paths, async { false }).await);
        let id = match config.snapshot().accounts.first() {
            Some(account) => account.id.clone(),
            None => config.add_account(MIGRATED_LABEL)?.id,
        };
        let account_paths = paths.account(&id).ok_or_else(|| anyhow::anyhow!("{id:?} is not an account id"))?;
        Self::new(config, &id, account_paths, endpoints, secrets, sign_in_timeout)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// `Account1.Mode`: the mode the account runs in (`docs/design/writes.md` §2). Read-write only while
    /// `config.toml` says so, the gate lets its drive through, and its last token carried
    /// `Files.ReadWrite`; [`recompute_mode`](Self::recompute_mode) keeps it.
    pub fn mode(&self) -> Mode {
        self.state.get().mode
    }

    /// The mode `config.toml` gives the account now — read again, as the gate is:
    /// the user's choice, which it runs in only when the gate and its token allow
    /// ([`mode`](Self::mode)). A file that cannot be read now reads as read-only.
    pub fn configured_mode(&self) -> Mode {
        self.config.write_standing(&self.id).map(|(mode, _)| mode).unwrap_or_default()
    }

    /// What a sign-in asks for: read-write when `config.toml` says so and the gate lets the
    /// drive through, both from one reading of the file, so that signing in again keeps a
    /// read-write account read-write.
    fn sign_in_mode(&self) -> Mode {
        match self.config.write_standing(&self.id) {
            Some((Mode::ReadWrite, Some(_))) => Mode::ReadWrite,
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

    /// Access tokens for Graph callers.
    pub fn tokens(&self) -> &Arc<TokenManager> {
        &self.tokens
    }

    /// A read-only Graph drive client for the sync side, on this account's
    /// tokens.
    pub fn drive(&self) -> anyhow::Result<crate::drive::DriveClient> {
        crate::drive::DriveClient::new(self.endpoints.graph.clone(), Arc::clone(&self.tokens) as Arc<dyn crate::token::TokenSource>)
    }

    fn graph(&self) -> GraphClient {
        GraphClient::new(self.http.clone(), self.endpoints.graph.clone())
    }

    /// A client asking for `mode`'s scope, for the authorization, the code exchange or a
    /// refresh.
    fn oauth_client(&self, client_id: &str, mode: Mode) -> OAuthClient {
        OAuthClient::new(self.http.clone(), self.endpoints.clone(), client_id.to_owned()).with_scope(scopes_for(mode))
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
    /// Called whenever one of them may have changed. The folder follows `Mode`
    /// (`crate::sync::write_mode::follow`).
    fn recompute_mode(&self) {
        // The mode and the list it is gated by, from one reading of the file. A
        // file that cannot be read now fails closed: read-only, and `LastError` says why when
        // the file last read said read-write.
        let standing = self.config.write_standing(&self.id);
        let unreadable = standing.is_none() && self.config.account(&self.id).is_some_and(|a| a.mode == Mode::ReadWrite);
        let (configured, allowed) = standing.unwrap_or((Mode::ReadOnly, None));
        self.state.update(|s| {
            let granted = grants_writes(&s.granted_scopes);
            let same_drive = allowed.as_deref().is_some_and(|drive| drive == s.live_drive);
            s.mode = if configured == Mode::ReadWrite && same_drive && granted { Mode::ReadWrite } else { Mode::ReadOnly };
            let signed_in = s.state == SignInState::SignedIn;
            let why = if signed_in && !s.wider_grant.is_empty() {
                Some(wider_grant_message(&s.wider_grant))
            } else if signed_in && unreadable {
                Some(CONFIG_UNREADABLE.to_owned())
            } else if signed_in && configured == Mode::ReadWrite {
                match &allowed {
                    None => Some(GATE_KEEPS_READ_ONLY.to_owned()),
                    // Another drive seen comes first: signing in again cannot cure it.
                    Some(drive) if !s.live_drive.is_empty() && !same_drive => {
                        Some(drive_mismatch_message(&s.live_drive, drive))
                    }
                    Some(_) if !granted => Some(SIGN_IN_TO_WRITE.to_owned()),
                    Some(_) if s.live_drive.is_empty() => Some(DRIVE_NOT_SEEN.to_owned()),
                    Some(_) => None,
                }
            } else {
                None
            };
            match why {
                Some(why) => s.last_error = why,
                None if is_mode_message(&s.last_error) => s.last_error.clear(),
                None => {}
            }
        });
        self.install_oauth(&self.state.get().client_id);
    }

    /// The mode worked out again now: the folder's outbox worker found the
    /// write gate closed — `config.toml` edited, the drive taken off the list — before any
    /// trigger here saw it.
    pub fn recheck_mode(&self) {
        self.recompute_mode();
    }

    /// A token asked for with `asked` turned out valid for `granted` (`docs/design/writes.md` §2):
    /// recorded, and the mode worked out again. The token manager calls this after every
    /// refresh, with its cache locked.
    fn note_granted(&self, asked: &str, granted: &str) {
        self.record_granted(asked, granted);
        self.recompute_mode();
    }

    /// Keeps what a token may be used for in the state and in `account.json`: what it was
    /// granted, but never more than was asked for. A read-only request answered
    /// with a token that can write — consent Microsoft still holds — is logged, shown in
    /// `LastError`, and used to read only; `Dev1` hands such a token to nobody.
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
        let mut info = account_cache::load(&self.cache).unwrap_or_default();
        change(&mut info);
        if let Err(e) = account_cache::save(&self.cache, &info) {
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
                let cached = account_cache::load(&self.cache);
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
            Ok(false) => account_cache::remove(&self.cache),
            Err(e) => self.state.update(|s| s.last_error = e.to_string()),
        }
    }

    /// Sets the client id every account signs in with, when this account is signed out: the
    /// single-account form of `Accounts1.SetClientId`, whose manager checks every account
    /// and then calls [`use_client_id`](Self::use_client_id) on each.
    pub fn set_client_id(&self, id: &str) -> Result<(), AccountError> {
        let id = id.trim();
        if !is_valid_client_id(id) {
            return Err(AccountError::InvalidClientId);
        }
        if self.state.get().state != SignInState::SignedOut {
            return Err(AccountError::Busy);
        }
        // Through the store: `config.toml` also holds every account and its folder, and a
        // file that cannot be read is refused, never written back from defaults.
        self.config.set_client_id(id)?;
        self.use_client_id(id);
        Ok(())
    }

    /// Signs in with `id` from now on (already validated and saved).
    pub fn use_client_id(&self, id: &str) {
        self.install_oauth(id);
        self.state.update(|s| s.client_id = id.to_owned());
    }

    /// `Account1.SetLabel`: the rules of `config::check_label`, `InvalidLabel` otherwise.
    pub fn set_label(&self, label: &str) -> Result<(), AccountError> {
        let label = self.config.set_label(&self.id, label)?;
        self.state.update(|s| s.label = label);
        Ok(())
    }

    /// Starts a sign-in and returns the URL the user must open in a browser.
    pub async fn begin_sign_in(self: &Arc<Self>) -> Result<String, AccountError> {
        if self.is_retired() {
            return Err(AccountError::Failed(RETIRED.into()));
        }
        let client_id = self.state.get().client_id;
        if client_id.is_empty() {
            return Err(AccountError::NoClientId);
        }
        if !self.state.try_transition(SignInState::SignedOut, SignInState::SigningIn) {
            return Err(AccountError::Busy);
        }
        self.state.update(|s| s.last_error.clear());
        let listener = match LoopbackListener::bind().await {
            Ok(listener) => listener,
            Err(e) => {
                let message = format!("cannot listen on localhost: {e}");
                self.state.update(|s| {
                    s.state = SignInState::SignedOut;
                    s.last_error = message.clone();
                });
                return Err(AccountError::Failed(message));
            }
        };
        // A read-write account signing in again asks for Files.ReadWrite again (§7).
        let oauth = self.oauth_client(&client_id, self.sign_in_mode());
        let redirect_uri = listener.redirect_uri();
        let pkce = Pkce::new();
        let csrf = random_token();
        let url = self.authorize_url(&oauth, &redirect_uri, &pkce, &csrf);
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let generation = {
            let mut session = self.session.lock().await;
            session.generation += 1;
            session.cancel = Some(cancel_tx);
            session.generation
        };
        let attempt = SignInAttempt { oauth, listener, redirect_uri, pkce, csrf, cancel: cancel_rx, generation };
        let this = Arc::clone(self);
        tokio::spawn(async move { this.finish_sign_in(attempt).await });
        Ok(url)
    }

    /// The authorization URL of a sign-in with `oauth`: Microsoft's account picker for a
    /// read-only sign-in, so that any account can be chosen; pinned to this account whenever it
    /// asks for `Files.ReadWrite` — its password asked for again, with its email
    /// filled in when it is known: from the state, `account.json`, or — after a sign-out,
    /// which forgets both — the `login_hint` `config.toml` keeps.
    fn authorize_url(&self, oauth: &OAuthClient, redirect_uri: &str, pkce: &Pkce, csrf: &str) -> String {
        if !grants_writes(oauth.scope()) {
            return oauth.picker_authorize_url(redirect_uri, pkce, csrf).to_string();
        }
        let email = [
            Some(self.state.get().email),
            account_cache::load(&self.cache).map(|info| info.email),
            self.config.account(&self.id).map(|account| account.login_hint),
        ]
        .into_iter()
        .flatten()
        .find(|email| !email.is_empty());
        oauth.pinned_authorize_url(redirect_uri, pkce, csrf, email.as_deref()).to_string()
    }

    /// Stops waiting for the browser and returns to `signed-out` with no `LastError`,
    /// whether the browser has answered yet or not. If a callback already arrived and is
    /// being exchanged, that exchange is left to finish but its result is discarded: this
    /// bumps the generation, so the exchange's eventual commit sees itself superseded.
    pub async fn cancel_sign_in(&self) {
        let mut session = self.session.lock().await;
        session.generation += 1;
        if let Some(cancel) = session.cancel.take() {
            let _ = cancel.send(());
        }
        self.state.update(|s| {
            if s.state == SignInState::SigningIn {
                s.state = SignInState::SignedOut;
                s.last_error.clear();
            }
        });
    }

    /// `Accounts1.Remove`'s sign-out: the account retired first, so that no sign-in is
    /// begun or stored from then on — one under way is superseded by the sign-out, and one
    /// whose exchange ends later finds the account retired — then signed out as
    /// [`sign_out`](Self::sign_out) signs out.
    pub async fn retire(&self) -> Result<(), AccountError> {
        self.retired.store(true, std::sync::atomic::Ordering::SeqCst);
        self.sign_out().await
    }

    fn is_retired(&self) -> bool {
        self.retired.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub async fn sign_out(&self) -> Result<(), AccountError> {
        // Held for the whole call: supersedes any in-flight sign-in attempt, and makes
        // this mutually exclusive with `refresh_account_info`'s cache-save-and-apply step,
        // so neither a stale sign-in nor a stale refresh can resurrect state afterwards.
        let mut session = self.session.lock().await;
        session.generation += 1;
        if let Some(cancel) = session.cancel.take() {
            let _ = cancel.send(());
        }
        // Takes TokenManager's own lock, so an in-flight access-token refresh commits its
        // rotated refresh token to the wallet before this deletes it.
        self.tokens.forget().await.map_err(|e| AccountError::Failed(e.to_string()))?;
        account_cache::remove(&self.cache);
        self.state.update(|s| {
            s.state = SignInState::SignedOut;
            s.last_error.clear();
            s.clear_account();
        });
        // Nothing is granted any more: read-only until the next sign-in, which asks for the
        // mode config.toml keeps.
        self.recompute_mode();
        Ok(())
    }

    pub async fn refresh_account_info(&self) {
        if self.state.get().state != SignInState::SignedIn {
            return;
        }
        let Ok(token) = self.token_for_refresh().await else { return };
        let graph = self.graph();
        let mut result = tokio::try_join!(graph.profile(&token), graph.drive(&token));
        if matches!(result, Err(GraphError::Unauthorized)) {
            // The cached access token was rejected (e.g. its lifetime elapsed across a
            // suspend, which `Instant`-based expiry cannot see coming). Invalidate it and
            // retry once with a freshly refreshed one before giving up.
            self.tokens.invalidate().await;
            let Ok(token) = self.token_for_refresh().await else { return };
            result = tokio::try_join!(graph.profile(&token), graph.drive(&token));
        }
        let (profile, drive) = match result {
            Ok(both) => both,
            Err(e) => {
                self.state
                    .update(|s| s.last_error = format!("Could not load account info: {e}"));
                return;
            }
        };
        let info = AccountInfo {
            display_name: profile.display_name,
            email: profile.email,
            quota_used: drive.quota.used,
            quota_total: drive.quota.total,
            fetched_at: unix_now(),
            granted_scopes: String::new(),
            drive_id: String::new(),
        };
        // Same lock `sign_out` holds for its whole call: if a sign-out is in progress (or
        // ran to completion while the Graph calls above were in flight), either this waits
        // until it is done and then sees `SignedOut` below, or it beats a sign-out that is
        // still waiting for this lock.
        let _session = self.session.lock().await;
        if self.state.get().state != SignInState::SignedIn {
            return;
        }
        // The drive comes with the quota (§8.1): an account that has none recorded yet —
        // migrated from a folder that never recorded one — records it now.
        if let Err(e) = self.record_drive(&drive.id) {
            tracing::warn!("cannot record the drive of account {:?}: {e}", self.id);
        }
        self.secrets.describe(&info.email);
        {
            // The granted scopes as the last refresh left them, and the drive the token was
            // just seen to reach, kept beside the name and quota.
            let _cache = self.cache_lock.lock().unwrap();
            let granted_scopes = self.state.get().granted_scopes;
            let info = AccountInfo { granted_scopes, drive_id: drive.id.clone(), ..info.clone() };
            if let Err(e) = account_cache::save(&self.cache, &info) {
                tracing::warn!("cannot write the account cache: {e}");
            }
        }
        self.state.update(|s| {
            if s.state == SignInState::SignedIn {
                apply_info(s, &info);
                s.live_drive = drive.id.clone();
                s.last_error.clear();
            }
        });
        // Says again why a read-write account runs read-only, if it does: the drive may have
        // just been recorded, or be another than config.toml's, and the line above cleared the
        // reason. A drive that is not the recorded one turns a read-write account read-only.
        self.recompute_mode();
    }

    /// The drive recorded for this account, if one is.
    fn recorded_drive(&self) -> Option<String> {
        self.config.account(&self.id).map(|a| a.drive_id).filter(|d| !d.is_empty())
    }

    /// Records `drive` as this account's when it has none yet; the drive recorded.
    fn record_drive(&self, drive: &str) -> Result<String, String> {
        if drive.is_empty() {
            return Err("Microsoft Graph did not say which drive this is".into());
        }
        let recorded = self.config.record_drive(&self.id, drive).map_err(|e| e.to_string())?;
        if recorded != drive {
            tracing::warn!(
                "account {:?} is signed in to drive {drive}, but drive {recorded} is recorded for it",
                self.id
            );
        }
        Ok(recorded)
    }

    /// Asks Graph which drive this account is, and records it if none is recorded yet
    /// (§8.2: asked by another account's sign-in). The drive recorded.
    pub async fn learn_drive(&self) -> Result<String, String> {
        let token = self.tokens.access_token().await.map_err(|e| e.to_string())?;
        let drive = self.graph().drive(&token).await.map_err(|e| e.to_string())?;
        self.record_drive(&drive.id)
    }

    /// Gets an access token for `refresh_account_info`, reconciling the visible state with
    /// what [`TokenManager::access_token`] found. Returns `Err(())` once the state (and, for
    /// a transient failure, `LastError`) has been dealt with, or there is nothing further to
    /// do because the token manager already handled it.
    async fn token_for_refresh(&self) -> Result<String, ()> {
        match self.tokens.access_token().await {
            Ok(token) => Ok(token),
            Err(AuthError::Transient(message)) => {
                self.state
                    .update(|s| s.last_error = format!("Could not load account info: {message}"));
                Err(())
            }
            // Wallet locked: the token manager already set `LastError` and the state is
            // still `SignedIn`, so a later unlock lets a retry succeed without user action.
            Err(AuthError::Locked) => Err(()),
            // `SignedOut` covers three different situations in the token manager:
            //   - invalid_grant: it already moved the state to `SignedOut` with
            //     `SESSION_EXPIRED`. The `still signed-in` guard below then does nothing,
            //     leaving that message alone.
            //   - no refresh token in the wallet, or
            //   - no client ID configured
            // For the latter two the state was never touched, so it is still `SignedIn`
            // here even though there is no way to actually refresh anything; that is the
            // "signed in with a lost wallet item" / "signed in with no client ID" bug this
            // guards against. Tell them apart from the account layer's own client ID.
            Err(AuthError::SignedOut) => {
                self.state.update(|s| {
                    if s.state == SignInState::SignedIn {
                        s.last_error = if s.client_id.is_empty() {
                            "Set a client ID first, then sign in again.".into()
                        } else {
                            "The stored sign-in was lost. Sign in again.".into()
                        };
                        s.state = SignInState::SignedOut;
                        s.clear_account();
                    }
                });
                Err(())
            }
        }
    }

    async fn finish_sign_in(&self, attempt: SignInAttempt) {
        let (generation, asked) = (attempt.generation, attempt.oauth.scope());
        match self.complete_sign_in(attempt).await {
            Ok(tokens) => self.commit_sign_in(generation, asked, tokens).await,
            Err(message) => self.abort_sign_in(generation, message).await,
        }
    }

    /// Waits for the browser and exchanges the code, without touching shared state: the
    /// caller decides whether the result still applies.
    async fn complete_sign_in(&self, attempt: SignInAttempt) -> Result<TokenResponse, String> {
        let SignInAttempt { oauth, listener, redirect_uri, pkce, csrf, cancel, .. } = attempt;
        let callback = tokio::select! {
            result = listener.wait(&csrf, self.sign_in_timeout) => result,
            _ = cancel => return Err(String::new()),
        };
        let code = match callback {
            Ok(Callback::Code(code)) => code,
            Ok(Callback::Error { error, .. }) if error == "access_denied" => {
                return Err("Access was denied in the browser.".into())
            }
            Ok(Callback::Error { error, description }) => {
                return Err(format!("Microsoft reported an error: {error}: {description}"))
            }
            Err(LoopbackError::TimedOut) => {
                return Err("Timed out waiting for the browser. If Microsoft showed an error page, \
                            check the app registration: client ID and redirect URI http://localhost."
                    .into())
            }
            Err(e) => return Err(e.to_string()),
        };
        oauth
            .exchange_code(&code, &pkce.verifier, &redirect_uri)
            .await
            .map_err(|e| format!("Sign-in failed: {e}"))
    }

    /// Which drive — and whose email — the new access token is for (§8.2). Only the drive
    /// is needed: it is the check. The email names the wallet item.
    async fn identify(&self, access_token: &str) -> Result<Identity, String> {
        let graph = self.graph();
        let (drive, profile) = tokio::join!(graph.drive(access_token), graph.profile(access_token));
        let drive = drive.map_err(|e| e.to_string())?;
        if drive.id.is_empty() {
            return Err("Microsoft Graph did not say which drive this is".into());
        }
        Ok(Identity { drive: drive.id, email: profile.ok().map(|p| p.email).filter(|e| !e.is_empty()) })
    }

    /// The other accounts the guard cannot tell apart from this drive (§8.2), by id: those
    /// with no drive recorded that may be signed in to one — signed in, or holding a refresh
    /// token they have not used yet (a wallet that did not answer at startup leaves an
    /// account signed out with its token still stored). Each is asked for its drive first,
    /// and is left out once it has one.
    async fn settle_siblings(&self) -> Vec<String> {
        let others = self.siblings().map(|s| s.others(&self.id)).unwrap_or_default();
        let mut unsettled = Vec::new();
        for other in others {
            if other.recorded_drive().is_some() {
                continue;
            }
            let signed_in = other.state.get().state == SignInState::SignedIn;
            if !signed_in && matches!(other.secrets.exists().await, Ok(false)) {
                continue;
            }
            if let Err(e) = other.learn_drive().await {
                tracing::warn!("cannot learn the drive of account {:?}: {e}", other.id);
            }
            if other.recorded_drive().is_none() {
                unsettled.push(other.id.clone());
            }
        }
        unsettled
    }

    /// The identity guard's check and record (§8.2), in one `ConfigStore` update so that two
    /// sign-ins cannot pass it together: this slot is one drive, and a drive is one slot.
    /// `unsettled` are the other accounts that may be this drive without saying so
    /// ([`settle_siblings`](Self::settle_siblings)). `Err` says why the sign-in is refused;
    /// `Ok(true)` when the drive was recorded now, which [`unclaim`](Self::unclaim) undoes
    /// if the sign-in fails after all. The email the sign-in found, when there is one, is kept
    /// as the account's `login_hint` in the same write.
    fn claim(&self, drive: &str, email: Option<&str>, unsettled: &[String]) -> Result<bool, String> {
        let who = {
            let s = self.state.get();
            if s.email.is_empty() {
                format!("'{}'", s.label)
            } else {
                s.email
            }
        };
        self.config.update(|config| -> Result<bool, String> {
            let mine = config.account(&self.id).ok_or_else(|| "This account was removed.".to_owned())?;
            if !mine.drive_id.is_empty() && mine.drive_id != drive {
                return Err(format!(
                    "This account is {who}. You signed in as a different Microsoft account; to \
                     connect that one, add a new account."
                ));
            }
            if let Some(other) = config.accounts.iter().find(|a| a.id != self.id && a.drive_id == drive) {
                return Err(format!("This Microsoft account is already connected as '{}'.", other.label));
            }
            if config.accounts.iter().any(|a| a.id != self.id && a.drive_id.is_empty() && unsettled.contains(&a.id)) {
                return Err("Could not check which account this is; try again.".into());
            }
            let mine = config.account_mut(&self.id).expect("found above");
            let recorded = mine.drive_id.is_empty();
            mine.drive_id = drive.to_owned();
            if let Some(email) = email.filter(|email| !email.is_empty()) {
                mine.login_hint = email.to_owned();
            }
            Ok(recorded)
        })
    }

    /// Takes back a drive [`claim`](Self::claim) recorded for a sign-in that then failed:
    /// a failed sign-in stores nothing (§8.2), and a slot that was never signed in must not
    /// keep the identity of the account it tried.
    fn unclaim(&self, drive: &str) {
        let taken_back = self.config.update_account(&self.id, |account| {
            if account.drive_id == drive {
                account.drive_id.clear();
            }
            Ok::<_, ConfigError>(())
        });
        if let Err(e) = taken_back {
            tracing::warn!("cannot take the drive of a failed sign-in back out of config.toml: {e}");
        }
    }

    /// Stores the refresh token and marks the session signed in, but only if `generation`
    /// is still the current attempt, and only once the identity guard (§8.2) has let this
    /// drive into this slot. Otherwise (a cancel or a sign-out ran first, a newer
    /// `begin_sign_in` superseded this one, or the guard refused) the tokens are discarded
    /// without ever touching Secret Service. The guard is fail-closed: a drive that cannot
    /// be asked for refuses the sign-in too.
    async fn commit_sign_in(&self, generation: u64, asked: &'static str, tokens: TokenResponse) {
        let Some(refresh_token) = tokens.refresh_token.clone() else {
            self.abort_sign_in(generation, "Microsoft did not return a refresh token.".into()).await;
            return;
        };
        let identity = match self.identify(&tokens.access_token).await {
            Ok(identity) => identity,
            Err(e) => {
                let message = format!("Could not check which account this is ({e}); try again.");
                self.abort_sign_in(generation, message).await;
                return;
            }
        };
        let unsettled = self.settle_siblings().await;
        let session = self.session.lock().await;
        // A retired account (`Accounts1.Remove`) stores nothing, whatever the browser said.
        if session.generation != generation || self.is_retired() {
            return;
        }
        let recorded = match self.claim(&identity.drive, identity.email.as_deref(), &unsettled) {
            Ok(recorded) => recorded,
            Err(message) => {
                drop(session);
                self.abort_sign_in(generation, message).await;
                return;
            }
        };
        if let Some(email) = &identity.email {
            self.secrets.describe(email);
        }
        if let Err(e) = self.secrets.store(&refresh_token).await {
            if recorded {
                self.unclaim(&identity.drive);
            }
            drop(session);
            self.abort_sign_in(generation, e.to_string()).await;
            return;
        }
        self.tokens.seed_as(&tokens, asked).await;
        self.state.update(|s| {
            s.state = SignInState::SignedIn;
            s.last_error.clear();
        });
        // What the sign-in granted, and the drive it reached, decide the mode (§7): a
        // read-write account that signed in again with Files.ReadWrite, to its own drive, is
        // read-write again.
        self.record_live_drive(&identity.drive);
        self.note_granted(asked, &tokens.granted(asked));
        drop(session);
        self.refresh_account_info().await;
    }

    /// Records a sign-in failure, but only if `generation` is still the current attempt.
    /// An empty `message` means the user cancelled; `cancel_sign_in` already updated the
    /// state directly, so there is nothing left to do here.
    async fn abort_sign_in(&self, generation: u64, message: String) {
        let session = self.session.lock().await;
        if session.generation != generation || message.is_empty() {
            return;
        }
        self.state.update(|s| {
            s.state = SignInState::SignedOut;
            s.last_error = message.clone();
        });
    }

    /// `Account1.SetMode` (`docs/design/writes.md` §2): switches the account to `mode` — `read-only` or
    /// `read-write` — and answers the URL of the sign-in the switch needs, empty when it
    /// needs none.
    ///
    /// - **To read-write**: refused `WritesNotAllowed` unless the gate lets the account's
    ///   drive through, whatever else holds; then `NotSignedIn` unless the account is signed
    ///   in. Nothing is written yet: a sign-in asking for `Files.ReadWrite` begins, and only
    ///   when its token response grants that — for this account's own drive, still on the
    ///   gate's list — are the refresh token stored and `mode = "read-write"` written; the
    ///   folder then follows `Mode`. A cancelled, refused or failed sign-in changes nothing
    ///   and says why in `LastError`; the account stays signed in, read-only. An account
    ///   read-write already needs no sign-in.
    /// - **To read-only**: refused `PendingUploads` while changes wait to be uploaded, unless
    ///   `force`, which drops them (the files stay, as ordinary local changes). Then
    ///   `mode = "read-only"` is written, the token that could write is dropped, and the next
    ///   refresh asks for `Files.Read`, a subset of the grant: no sign-in. A switch to
    ///   read-write still waiting for its browser is given up.
    pub async fn set_mode(self: &Arc<Self>, mode: &str, force: bool) -> Result<String, ModeError> {
        let mode = Mode::parse(mode)
            .ok_or_else(|| ModeError::InvalidMode(format!("{mode:?} is not a mode: read-only or read-write")))?;
        if self.is_retired() {
            return Err(ModeError::Failed(RETIRED.into()));
        }
        match mode {
            Mode::ReadWrite => self.switch_to_read_write().await,
            Mode::ReadOnly => self.switch_to_read_only(force).await.map(|()| String::new()),
        }
    }

    /// [`set_mode`](Self::set_mode) to read-write: the gate, then the sign-in it needs.
    async fn switch_to_read_write(self: &Arc<Self>) -> Result<String, ModeError> {
        // The gate first: no account whose drive is not listed is ever asked to sign in for
        // write access, signed in or not.
        if !self.config.writes_allowed(&self.id) {
            return Err(ModeError::WritesNotAllowed(WRITES_NOT_ALLOWED.into()));
        }
        let not_signed_in = || ModeError::NotSignedIn("sign in first; then switch the account to read-write".into());
        let snapshot = self.state.get();
        if snapshot.state != SignInState::SignedIn {
            return Err(not_signed_in());
        }
        if snapshot.mode == Mode::ReadWrite {
            return Ok(String::new());
        }
        if snapshot.client_id.is_empty() {
            return Err(ModeError::Failed(AccountError::NoClientId.to_string()));
        }
        let listener = LoopbackListener::bind()
            .await
            .map_err(|e| ModeError::Failed(format!("cannot listen on localhost: {e}")))?;
        let oauth = self.oauth_client(&snapshot.client_id, Mode::ReadWrite);
        let redirect_uri = listener.redirect_uri();
        let pkce = Pkce::new();
        let csrf = random_token();
        // Pinned to this account: the password asked for again, its email filled
        // in, so that a browser signed in to another account cannot consent for it.
        let url = self.authorize_url(&oauth, &redirect_uri, &pkce, &csrf);
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let generation = {
            let mut session = self.session.lock().await;
            // Asked again under the lock: a sign-out and a new sign-in in the
            // meantime must not have their attempt cancelled by this switch.
            if self.state.get().state != SignInState::SignedIn {
                return Err(not_signed_in());
            }
            // A switch still waiting for its browser gives way to this one.
            session.generation += 1;
            if let Some(previous) = session.cancel.replace(cancel_tx) {
                let _ = previous.send(());
            }
            self.state.update(|s| s.last_error.clear());
            session.generation
        };
        let attempt = SignInAttempt { oauth, listener, redirect_uri, pkce, csrf, cancel: cancel_rx, generation };
        let this = Arc::clone(self);
        tokio::spawn(async move { this.finish_read_write(attempt).await });
        Ok(url)
    }

    /// The browser's answer to a switch to read-write, and what it leads to.
    async fn finish_read_write(&self, attempt: SignInAttempt) {
        let (generation, asked) = (attempt.generation, attempt.oauth.scope());
        match self.complete_sign_in(attempt).await {
            Ok(tokens) => self.commit_read_write(generation, asked, tokens).await,
            Err(message) => self.abort_read_write(generation, message).await,
        }
    }

    /// The switch to read-write, once its token response is in: only a grant of
    /// `Files.ReadWrite`, for this account's own drive (`GET /me/drive` with the new token),
    /// still on the gate's list, is taken. Then the refresh token is stored, the new token
    /// cached and what it was granted recorded, and only then `mode = "read-write"` written
    /// and the mode worked out again. Anything else changes nothing but
    /// `LastError`.
    async fn commit_read_write(&self, generation: u64, asked: &'static str, tokens: TokenResponse) {
        let granted = tokens.granted(asked);
        if !grants_writes(&granted) {
            let message = format!(
                "Microsoft did not allow konedrive to change files in OneDrive (the sign-in granted \
                 {granted:?}); the account stays read-only."
            );
            return self.abort_read_write(generation, message).await;
        }
        let Some(refresh_token) = tokens.refresh_token.clone() else {
            return self.abort_read_write(generation, "Microsoft did not return a refresh token; the account stays read-only.".into()).await;
        };
        let identity = match self.identify(&tokens.access_token).await {
            Ok(identity) => identity,
            Err(e) => {
                let message = format!("Could not check which account this is ({e}); the account stays read-only.");
                return self.abort_read_write(generation, message).await;
            }
        };
        let session = self.session.lock().await;
        if session.generation != generation || self.is_retired() || self.state.get().state != SignInState::SignedIn {
            return;
        }
        let who = self.who();
        let now = self.config.current();
        let refusal = match &now {
            Some(config) => read_write_refusal(config, &self.id, &identity.drive, &who),
            None => Some(format!("{}.", WRITES_NOT_ALLOWED)),
        };
        if let Some(why) = refusal {
            drop(session);
            return self.abort_read_write(generation, why).await;
        }
        // The token first: one that can write, for this very drive, is harmless under a
        // read-only mode (every refresh asks for Files.Read then), and the mode is written
        // only once it is stored.
        if let Err(e) = self.secrets.store(&refresh_token).await {
            drop(session);
            return self.abort_read_write(generation, format!("{e}; the account stays read-only.")).await;
        }
        // All with the token cache held: no refresh can run between recording the
        // grant and caching the new token, so none can record its narrower grant over it. The
        // grant and the drive are recorded before config.toml says read-write, so
        // a crash in between finds them; the new token is cached only once it does, and the
        // mode is worked out again under the same hold.
        let email = identity.email.clone().unwrap_or_default();
        let written = self
            .tokens
            .commit_as(&tokens, asked, || {
                self.record_granted(asked, &granted);
                self.record_live_drive(&identity.drive);
                // The check again, and the write it allows, in one step.
                let written = self.config.update(|config| match read_write_refusal(config, &self.id, &identity.drive, &who) {
                    Some(why) => Err(why),
                    None => {
                        let mine = config.account_mut(&self.id).expect("checked above");
                        mine.mode = Mode::ReadWrite;
                        if !email.is_empty() {
                            mine.login_hint = email.clone();
                        }
                        Ok(())
                    }
                });
                if written.is_ok() {
                    self.state.update(|s| s.last_error.clear());
                }
                self.recompute_mode();
                written
            })
            .await;
        if let Err(why) = written {
            drop(session);
            return self.abort_read_write(generation, why).await;
        }
        tracing::info!("the account {:?} is read-write now", self.id);
    }

    /// A switch to read-write that did not go through: `LastError` says why, and nothing
    /// else changes — the account stays signed in, read-only. Only for the current attempt;
    /// an empty `message` is a cancel, which says nothing.
    async fn abort_read_write(&self, generation: u64, message: String) {
        let session = self.session.lock().await;
        if session.generation != generation || message.is_empty() {
            return;
        }
        self.state.update(|s| s.last_error = message);
    }

    /// [`set_mode`](Self::set_mode) to read-only.
    async fn switch_to_read_only(&self, force: bool) -> Result<(), ModeError> {
        {
            // A switch to read-write still waiting for its browser is given up. Signing in has
            // its own attempt, which this leaves alone: the state is asked under the lock.
            let mut session = self.session.lock().await;
            if self.state.get().state == SignInState::SignedIn {
                session.generation += 1;
                if let Some(cancel) = session.cancel.take() {
                    let _ = cancel.send(());
                }
            }
        }
        let uploads = self.uploads.lock().unwrap().as_ref().and_then(Weak::upgrade);
        if self.configured_mode() == Mode::ReadOnly {
            // Read-only already, changes may still wait: a switch nobody forced kept them.
            // Forced, they go now — the way out a Forget and a Remove refused
            // `PendingUploads` point to.
            if let (true, Some(uploads)) = (force, &uploads) {
                uploads.drop_pending_uploads().await;
            }
            self.recompute_mode();
            return Ok(());
        }
        if let Some(uploads) = &uploads {
            let pending = uploads.pending_uploads().await;
            if pending > 0 && !force {
                return Err(ModeError::PendingUploads(format!(
                    "{pending} changes made here have not been uploaded yet; wait for them, or force \
                     the switch: they then stay here, and are not uploaded"
                )));
            }
        }
        self.config
            .update_account(&self.id, |account| {
                account.mode = Mode::ReadOnly;
                Ok::<_, ConfigError>(())
            })
            .map_err(|e| ModeError::Failed(format!("cannot save the configuration: {e}")))?;
        // Dropped only once config.toml says read-only: a write that failed leaves
        // the account read-write with its rows.
        if let (true, Some(uploads)) = (force, &uploads) {
            uploads.drop_pending_uploads().await;
        }
        self.recompute_mode();
        // The token that could write goes now; the next one comes from a refresh that asks
        // for Files.Read (§7).
        self.tokens.invalidate().await;
        tracing::info!("the account {:?} is read-only now", self.id);
        Ok(())
    }

    /// `Dev1.AccessToken` (`docs/design/writes.md` §8.2; SECURITY.md): a token that can change nothing, whatever the
    /// account's mode.
    pub async fn read_only_token(&self) -> Result<String, AuthError> {
        self.tokens.read_only_token().await
    }

    /// `Dev1.ReadWriteAccessToken`, for the test-account harness only (`docs/design/writes.md` §8.2, §12; SECURITY.md):
    /// refused `WritesNotAllowed` unless the gate lets the account's drive through, and
    /// `ModeNotGranted` unless the account is read-write and its token carries
    /// `Files.ReadWrite`. The token itself is then asked which drive it reaches
    /// (`GET /me/drive`): another than the one the gate lets through refuses it
    /// `WritesNotAllowed`, and turns the account read-only.
    pub async fn read_write_token(&self) -> Result<String, ModeError> {
        let Some(drive) = self.config.writable_drive(&self.id) else {
            return Err(ModeError::WritesNotAllowed(WRITES_NOT_ALLOWED.into()));
        };
        let not_granted = || ModeError::ModeNotGranted("this account is read-only: switch it to read-write first".into());
        if self.mode() != Mode::ReadWrite {
            return Err(not_granted());
        }
        let token = match self.tokens.access_token_and_scope().await {
            Ok((token, scope)) if grants_writes(&scope) => token,
            Ok(_) => return Err(not_granted()),
            Err(AuthError::SignedOut) => return Err(ModeError::NotSignedIn("nobody is signed in".into())),
            Err(e) => return Err(ModeError::Failed(e.to_string())),
        };
        let live = self
            .graph()
            .drive(&token)
            .await
            .map_err(|e| ModeError::Failed(format!("cannot check which drive the token reaches: {e}")))?
            .id;
        self.record_live_drive(&live);
        if live != drive {
            self.recompute_mode();
            return Err(ModeError::WritesNotAllowed(format!(
                "the account's token reaches drive {live}, not drive {drive}, which config.toml records \
                 and write_test_drive_ids lists; it is not handed out, and the account runs read-only"
            )));
        }
        Ok(token)
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

/// Why a switch to read-write that reached `drive` is refused, if it is: the drive must be
/// the account's own — a sign-in as someone else changes nothing — and on the gate's list.
fn read_write_refusal(config: &Config, id: &str, drive: &str, who: &str) -> Option<String> {
    let Some(mine) = config.account(id) else { return Some("This account was removed.".into()) };
    if mine.drive_id != drive {
        return Some(format!(
            "This account is {who}, and the browser signed in as a different Microsoft account; the \
             account stays read-only."
        ));
    }
    if !config.writes_allowed(drive) {
        return Some(format!("{}.", WRITES_NOT_ALLOWED));
    }
    None
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
    pub fn remove(&self, id: &str) {
        self.0.lock().unwrap().retain(|a| a.upgrade().is_some_and(|a| a.id != id));
    }

    fn others(&self, id: &str) -> Vec<Arc<AccountService>> {
        self.0.lock().unwrap().iter().filter_map(Weak::upgrade).filter(|a| a.id != id).collect()
    }
}

fn apply_info(s: &mut AccountSnapshot, info: &AccountInfo) {
    s.display_name = info.display_name.clone();
    s.email = info.email.clone();
    s.quota_used = info.quota_used;
    s.quota_total = info.quota_total;
}

/// What a retired account answers a sign-in.
const RETIRED: &str = "this account is being removed";

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
