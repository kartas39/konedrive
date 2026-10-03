//! Access tokens for Graph callers: cached, refreshed on demand, one refresh at a time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::oauth::{is_read_only, OAuthClient, OAuthError, TokenResponse, SCOPES};
use crate::secret::{SecretError, SecretStore};

/// Refresh when less than this remains.
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

pub const SESSION_EXPIRED: &str = "Session expired. Sign in again.";
pub const WALLET_LOCKED: &str = "Secret storage is locked.";

/// What a refresh that fails says to the account it is for: the daemon's account state.
pub trait RefreshReport: Send + Sync {
    /// The refresh could not be made, and the user should know why. The account stays signed
    /// in.
    fn failed(&self, message: &str);
    /// The refresh token is no longer valid: the account is signed out, and `message` says
    /// why.
    fn signed_out(&self, message: &str);
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("signed out")]
    SignedOut,
    #[error("secret storage is locked")]
    Locked,
    #[error("{0}")]
    Transient(String),
}

struct Cached {
    token: String,
    expires_at: Instant,
    /// What the token is valid for ([`TokenResponse::granted`]).
    scope: String,
    /// What was asked for when it was obtained. A token asked for under another scope than
    /// the one installed now is stale: the account changed mode since, and a read-write
    /// token must not serve a read-only account for the rest of its hour.
    asked: &'static str,
}

impl Cached {
    fn from_response(response: &TokenResponse, asked: &'static str) -> Self {
        Self {
            token: response.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(response.expires_in),
            scope: response.granted(asked),
            asked,
        }
    }

    fn fresh(&self) -> bool {
        self.expires_at > Instant::now() + REFRESH_MARGIN
    }
}

/// Told, after every refresh of the account's token, what was asked for and what the token
/// is valid for (`AccountService` keeps it, `docs/design/writes.md` §2).
pub type GrantedHook = Arc<dyn Fn(&'static str, &str) + Send + Sync>;

pub struct TokenManager {
    secrets: Arc<dyn SecretStore>,
    state: Box<dyn RefreshReport>,
    /// `None` while no client ID is configured. Its scope is what every refresh asks for:
    /// the account's mode's (`AccountService::install_oauth`).
    oauth: std::sync::Mutex<Option<OAuthClient>>,
    /// Held across a refresh, so concurrent callers wait for it instead of refreshing again.
    cached: tokio::sync::Mutex<Option<Cached>>,
    /// Called, with the cache's lock held, after every refresh of the account's token.
    on_granted: std::sync::Mutex<Option<GrantedHook>>,
}

impl TokenManager {
    pub fn new(secrets: Arc<dyn SecretStore>, state: impl RefreshReport + 'static) -> Self {
        Self {
            secrets,
            state: Box::new(state),
            oauth: std::sync::Mutex::new(None),
            cached: tokio::sync::Mutex::new(None),
            on_granted: std::sync::Mutex::new(None),
        }
    }

    pub fn set_oauth(&self, oauth: Option<OAuthClient>) {
        *self.oauth.lock().unwrap() = oauth;
    }

    /// What each refreshed token turns out to be valid for is told to `hook`. It runs with
    /// the token cache's lock held: it must not ask for a token.
    pub fn set_on_granted(&self, hook: GrantedHook) {
        *self.on_granted.lock().unwrap() = Some(hook);
    }

    /// What a refresh asks for now: the installed client's scope, read-only without one.
    fn asked(&self) -> &'static str {
        self.oauth.lock().unwrap().as_ref().map_or(SCOPES, OAuthClient::scope)
    }

    /// Caches the access token obtained at sign-in, as asked for with what a refresh asks
    /// for now: valid for what its response says or, when it says nothing, for that.
    pub async fn seed(&self, response: &TokenResponse) {
        self.seed_as(response, self.asked()).await;
    }

    /// Caches `response`'s access token, obtained by asking for `asked`.
    pub async fn seed_as(&self, response: &TokenResponse, asked: &'static str) {
        *self.cached.lock().await = Some(Cached::from_response(response, asked));
    }

    /// Runs `commit` with the token cache locked, and caches `response`'s token (obtained by
    /// asking for `asked`) once it succeeds. No refresh can start in between: one
    /// that asked for the scope installed before the commit would record its narrower grant
    /// over the one `commit` records. `commit` must not ask for a token.
    pub async fn commit_as<R, E>(
        &self,
        response: &TokenResponse,
        asked: &'static str,
        commit: impl FnOnce() -> Result<R, E>,
    ) -> Result<R, E> {
        let mut cached = self.cached.lock().await;
        let committed = commit()?;
        *cached = Some(Cached::from_response(response, asked));
        Ok(committed)
    }

    /// Drops the cached access token, forcing the next call to refresh it. Used when a
    /// Graph call rejects the cached token (401) without the refresh token itself being
    /// invalid, e.g. after the daemon was suspended past the token's lifetime.
    pub async fn invalidate(&self) {
        *self.cached.lock().await = None;
    }

    /// Deletes the stored refresh token and clears the cached access token, holding the
    /// cache lock across both so that an in-flight `access_token()` refresh (which holds
    /// the same lock while it stores a rotated refresh token) always commits before this
    /// deletes it, never after.
    pub async fn forget(&self) -> Result<(), SecretError> {
        let mut cached = self.cached.lock().await;
        self.secrets.delete().await?;
        *cached = None;
        Ok(())
    }

    pub async fn access_token(&self) -> Result<String, AuthError> {
        self.access_token_and_scope().await.map(|(token, _)| token)
    }

    /// The account's access token, and what it is valid for. A cached token is used only if
    /// it was asked for under the scope installed now: after a switch, or a
    /// downgrade, to read-only, the next call refreshes down to `Files.Read`.
    pub async fn access_token_and_scope(&self) -> Result<(String, String), AuthError> {
        let mut cached = self.cached.lock().await;
        let asked = self.asked();
        if let Some(c) = cached.as_ref().filter(|c| c.fresh() && c.asked == asked) {
            return Ok((c.token.clone(), c.scope.clone()));
        }
        let oauth = self.oauth.lock().unwrap().clone().ok_or(AuthError::SignedOut)?;
        let (response, granted) = self.refresh_with(&oauth, &mut cached).await?;
        *cached = Some(Cached::from_response(&response, oauth.scope()));
        let hook = self.on_granted.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(oauth.scope(), &granted);
        }
        Ok((response.access_token, granted))
    }

    /// An access token that can change nothing (`docs/design/writes.md` §8.2; SECURITY.md): `TokenExport.ReadOnly`'s,
    /// whatever the account's mode. The account's own token when it is read-only already;
    /// otherwise one obtained by a refresh that asks for `Files.Read` only — a subset of what
    /// was granted, which Microsoft allows — kept apart from the account's own, which stays
    /// what it was. Refused if Microsoft answers with more than that.
    pub async fn read_only_token(&self) -> Result<String, AuthError> {
        if self.asked() == SCOPES {
            // A read-only account: its own token, refreshed and cached as usual, is the one.
            let (token, scope) = self.access_token_and_scope().await?;
            if is_read_only(&scope) {
                return Ok(token);
            }
        }
        let mut cached = self.cached.lock().await;
        if let Some(c) = cached.as_ref().filter(|c| c.fresh() && is_read_only(&c.scope)) {
            return Ok(c.token.clone());
        }
        let oauth = self.oauth.lock().unwrap().clone().ok_or(AuthError::SignedOut)?.with_scope(SCOPES);
        let (response, granted) = self.refresh_with(&oauth, &mut cached).await?;
        if !is_read_only(&granted) {
            return Err(AuthError::Transient(format!(
                "Microsoft answered a request for a read-only token with one valid for {granted:?}; it is not handed out"
            )));
        }
        Ok(response.access_token)
    }

    /// One refresh with `oauth`, under the cache's lock (`cached` is its guard): the rotated
    /// refresh token stored, and an `invalid_grant` turned into a sign-out. The response and
    /// what its token is valid for; the cache itself is the caller's to fill.
    async fn refresh_with(&self, oauth: &OAuthClient, cached: &mut Option<Cached>) -> Result<(TokenResponse, String), AuthError> {
        let refresh_token = match self.secrets.load().await {
            Ok(Some(token)) => token,
            Ok(None) => return Err(AuthError::SignedOut),
            Err(SecretError::Locked) => {
                self.state.failed(WALLET_LOCKED);
                return Err(AuthError::Locked);
            }
            Err(e) => return Err(AuthError::Transient(e.to_string())),
        };
        match oauth.refresh(&refresh_token).await {
            Ok(response) => {
                if let Some(rotated) = response.refresh_token.as_deref() {
                    if rotated != refresh_token {
                        // A rotated refresh token keeps the grant of the one it replaces
                        // (RFC 6749 §6), whatever this refresh asked for.
                        self.secrets
                            .store(rotated)
                            .await
                            .map_err(|e| AuthError::Transient(e.to_string()))?;
                    }
                }
                let granted = response.granted(oauth.scope());
                Ok((response, granted))
            }
            Err(OAuthError::InvalidGrant(_)) => {
                if let Err(e) = self.secrets.delete().await {
                    tracing::warn!("cannot delete the stored refresh token after an invalid grant: {e}");
                }
                *cached = None;
                self.state.signed_out(SESSION_EXPIRED);
                Err(AuthError::SignedOut)
            }
            Err(OAuthError::Rejected { error, description }) => {
                let message = format!("Microsoft rejected the token refresh: {error}: {description}");
                self.state.failed(&message);
                Err(AuthError::Transient(message))
            }
            Err(OAuthError::Transient(message)) => Err(AuthError::Transient(message)),
        }
    }
}

/// What a Graph caller needs from the account: a current access token, and a
/// way to say the one it got was refused. `TokenManager` in the daemon; a
/// fixed token in the VM suite and in tests.
#[async_trait::async_trait]
pub trait TokenSource: Send + Sync {
    async fn access_token(&self) -> Result<String, AuthError>;
    async fn invalidate(&self);
}

#[async_trait::async_trait]
impl TokenSource for TokenManager {
    async fn access_token(&self) -> Result<String, AuthError> {
        TokenManager::access_token(self).await
    }

    async fn invalidate(&self) {
        TokenManager::invalidate(self).await
    }
}

/// A token handed in from outside — the short-lived read-only token the VM
/// suite runs with. It cannot be refreshed: once refused, it is signed out.
pub struct StaticToken {
    token: String,
    refused: std::sync::atomic::AtomicBool,
}

impl StaticToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self { token: token.into(), refused: std::sync::atomic::AtomicBool::new(false) }
    }
}

#[async_trait::async_trait]
impl TokenSource for StaticToken {
    async fn access_token(&self) -> Result<String, AuthError> {
        if self.refused.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(AuthError::SignedOut);
        }
        Ok(self.token.clone())
    }

    async fn invalidate(&self) {
        self.refused.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests;
