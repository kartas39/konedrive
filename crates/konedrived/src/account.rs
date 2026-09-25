//! Sign-in state machine behind the D-Bus interface: one Microsoft account.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{oneshot, Mutex};

use crate::account_cache::{self, AccountInfo};
use crate::config::{is_valid_client_id, AccountPaths, ConfigError, ConfigStore, Mode, Paths, MIGRATED_LABEL};
use crate::graph::{GraphClient, GraphError};
use crate::loopback::{Callback, LoopbackError, LoopbackListener};
use crate::oauth::{scopes_for, Endpoints, OAuthClient, TokenResponse};
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

/// One account: its sign-in, its tokens and its cached name and quota. Its entry in
/// `config.toml` (label, mode, drive) is read and written through the daemon's one
/// [`ConfigStore`].
pub struct AccountService {
    id: String,
    mode: Mode,
    config: Arc<ConfigStore>,
    state: StateHandle,
    /// `account.json`: the cached name and quota.
    cache: PathBuf,
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
        let (label, mode) = config.account(id).map(|a| (a.label, a.mode)).unwrap_or_default();
        let client_id = config.client_id();
        let state = StateHandle::new(AccountSnapshot {
            client_id: client_id.clone(),
            label,
            ..AccountSnapshot::default()
        });
        let tokens = Arc::new(TokenManager::new(secrets.clone(), state.clone()));
        let service = Arc::new(Self {
            id: id.to_owned(),
            mode,
            config,
            state,
            cache: paths.account_cache,
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

    /// `Account1.Mode`.
    pub fn mode(&self) -> Mode {
        self.mode
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

    /// With the scope of this account's mode, for the authorization, the code exchange and
    /// every refresh.
    fn oauth_client(&self, client_id: &str) -> OAuthClient {
        OAuthClient::new(self.http.clone(), self.endpoints.clone(), client_id.to_owned()).with_scope(scopes_for(self.mode))
    }

    fn install_oauth(&self, client_id: &str) {
        self.tokens
            .set_oauth((!client_id.is_empty()).then(|| self.oauth_client(client_id)));
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
                    }
                });
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
        let oauth = self.oauth_client(&client_id);
        let redirect_uri = listener.redirect_uri();
        let pkce = Pkce::new();
        let csrf = random_token();
        let url = oauth.authorize_url(&redirect_uri, &pkce, &csrf).to_string();
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
        if let Err(e) = account_cache::save(&self.cache, &info) {
            tracing::warn!("cannot write the account cache: {e}");
        }
        self.state.update(|s| {
            if s.state == SignInState::SignedIn {
                apply_info(s, &info);
                s.last_error.clear();
            }
        });
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
        let generation = attempt.generation;
        match self.complete_sign_in(attempt).await {
            Ok(tokens) => self.commit_sign_in(generation, tokens).await,
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
    /// if the sign-in fails after all.
    fn claim(&self, drive: &str, unsettled: &[String]) -> Result<bool, String> {
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
    async fn commit_sign_in(&self, generation: u64, tokens: TokenResponse) {
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
        let recorded = match self.claim(&identity.drive, &unsettled) {
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
        self.tokens.seed(&tokens).await;
        self.state.update(|s| {
            s.state = SignInState::SignedIn;
            s.last_error.clear();
        });
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
