//! Sign-in state machine behind the D-Bus interface.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{oneshot, Mutex};

use crate::account_cache::{self, AccountInfo};
use crate::config::{is_valid_client_id, Config, Paths};
use crate::graph::{GraphClient, GraphError};
use crate::loopback::{Callback, LoopbackError, LoopbackListener};
use crate::oauth::{Endpoints, OAuthClient, TokenResponse};
use crate::pkce::{random_token, Pkce};
use crate::secret::SecretStore;
use crate::state::{AccountSnapshot, SignInState, StateHandle};
use crate::token::{AuthError, TokenManager};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccountError {
    #[error("invalid client ID: expected a GUID like 00000000-0000-0000-0000-000000000000")]
    InvalidClientId,
    #[error("not possible right now: finish or cancel the current sign-in, or sign out first")]
    Busy,
    #[error("set a client ID first")]
    NoClientId,
    #[error("{0}")]
    Failed(String),
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

pub struct AccountService {
    state: StateHandle,
    paths: Paths,
    endpoints: Endpoints,
    http: reqwest::Client,
    secrets: Arc<dyn SecretStore>,
    tokens: Arc<TokenManager>,
    sign_in_timeout: Duration,
    session: Mutex<Session>,
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

impl AccountService {
    pub fn new(
        paths: Paths,
        endpoints: Endpoints,
        secrets: Arc<dyn SecretStore>,
        sign_in_timeout: Duration,
    ) -> anyhow::Result<Arc<Self>> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("konedrive/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .build()?;
        let config = Config::load(&paths.config_file).unwrap_or_else(|e| {
            tracing::warn!("ignoring unreadable {}: {e}", paths.config_file.display());
            Config::default()
        });
        let state = StateHandle::new(AccountSnapshot {
            client_id: config.client_id.clone(),
            ..AccountSnapshot::default()
        });
        let tokens = Arc::new(TokenManager::new(secrets.clone(), state.clone()));
        let service = Arc::new(Self {
            state,
            paths,
            endpoints,
            http,
            secrets,
            tokens,
            sign_in_timeout,
            session: Mutex::new(Session { generation: 0, cancel: None }),
        });
        service.install_oauth(&config.client_id);
        Ok(service)
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

    fn oauth_client(&self, client_id: &str) -> OAuthClient {
        OAuthClient::new(self.http.clone(), self.endpoints.clone(), client_id.to_owned())
    }

    fn install_oauth(&self, client_id: &str) {
        self.tokens
            .set_oauth((!client_id.is_empty()).then(|| self.oauth_client(client_id)));
    }

    /// Restores the session from the wallet (existence check only) and the account cache.
    pub async fn startup(self: &Arc<Self>) {
        match self.secrets.exists().await {
            Ok(true) => {
                let cached = account_cache::load(&self.paths.account_cache);
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
            Ok(false) => account_cache::remove(&self.paths.account_cache),
            Err(e) => self.state.update(|s| s.last_error = e.to_string()),
        }
    }

    pub fn set_client_id(&self, id: &str) -> Result<(), AccountError> {
        let id = id.trim();
        if !is_valid_client_id(id) {
            return Err(AccountError::InvalidClientId);
        }
        if self.state.get().state != SignInState::SignedOut {
            return Err(AccountError::Busy);
        }
        // Read-modify-write: `config.toml` also holds the sync sub-project's
        // registered root (§3.1), and writing a fresh `Config` here would
        // silently forget it — a folder full of placeholders that no startup
        // ever re-registers or recovers, and, for a folder registered with
        // the helper, the only record that the helper still holds it. So a
        // file that cannot be read is not written back from defaults either
        //: refused, and left exactly as it is. A
        // missing file is not unreadable; it is an empty configuration.
        let mut config = Config::load(&self.paths.config_file).map_err(|e| {
            AccountError::Failed(format!(
                "cannot save the configuration: {} cannot be read ({e}), and it is not \
                 overwritten, since it holds the sync folder's settings too",
                self.paths.config_file.display()
            ))
        })?;
        config.client_id = id.to_owned();
        config
            .save(&self.paths.config_file)
            .map_err(|e| AccountError::Failed(format!("cannot save the configuration: {e}")))?;
        self.install_oauth(id);
        self.state.update(|s| s.client_id = id.to_owned());
        Ok(())
    }

    /// Starts a sign-in and returns the URL the user must open in a browser.
    pub async fn begin_sign_in(self: &Arc<Self>) -> Result<String, AccountError> {
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
        account_cache::remove(&self.paths.account_cache);
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
        let graph = GraphClient::new(self.http.clone(), self.endpoints.graph.clone());
        let mut result = tokio::try_join!(graph.profile(&token), graph.quota(&token));
        if matches!(result, Err(GraphError::Unauthorized)) {
            // The cached access token was rejected (e.g. its lifetime elapsed across a
            // suspend, which `Instant`-based expiry cannot see coming). Invalidate it and
            // retry once with a freshly refreshed one before giving up.
            self.tokens.invalidate().await;
            let Ok(token) = self.token_for_refresh().await else { return };
            result = tokio::try_join!(graph.profile(&token), graph.quota(&token));
        }
        let (profile, quota) = match result {
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
            quota_used: quota.used,
            quota_total: quota.total,
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
        if let Err(e) = account_cache::save(&self.paths.account_cache, &info) {
            tracing::warn!("cannot write the account cache: {e}");
        }
        self.state.update(|s| {
            if s.state == SignInState::SignedIn {
                apply_info(s, &info);
                s.last_error.clear();
            }
        });
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

    /// Stores the refresh token and marks the session signed in, but only if `generation`
    /// is still the current attempt. Otherwise (a cancel or a sign-out ran first, or a
    /// newer `begin_sign_in` superseded this one) the tokens are discarded without ever
    /// touching Secret Service or the state.
    async fn commit_sign_in(&self, generation: u64, tokens: TokenResponse) {
        let Some(refresh_token) = tokens.refresh_token.clone() else {
            self.abort_sign_in(generation, "Microsoft did not return a refresh token.".into()).await;
            return;
        };
        let session = self.session.lock().await;
        if session.generation != generation {
            return;
        }
        if let Err(e) = self.secrets.store(&refresh_token).await {
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

fn apply_info(s: &mut AccountSnapshot, info: &AccountInfo) {
    s.display_name = info.display_name.clone();
    s.email = info.email.clone();
    s.quota_used = info.quota_used;
    s.quota_total = info.quota_total;
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
