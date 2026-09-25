//! Access tokens for Graph callers: cached, refreshed on demand, one refresh at a time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::oauth::{is_read_only, OAuthClient, OAuthError, TokenResponse, SCOPES};
use crate::secret::{SecretError, SecretStore};
use crate::state::{SignInState, StateHandle};

/// Refresh when less than this remains.
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

pub const SESSION_EXPIRED: &str = "Session expired. Sign in again.";
pub const WALLET_LOCKED: &str = "Secret storage is locked.";

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
    state: StateHandle,
    /// `None` while no client ID is configured. Its scope is what every refresh asks for:
    /// the account's mode's (`AccountService::install_oauth`).
    oauth: std::sync::Mutex<Option<OAuthClient>>,
    /// Held across a refresh, so concurrent callers wait for it instead of refreshing again.
    cached: tokio::sync::Mutex<Option<Cached>>,
    /// Called, with the cache's lock held, after every refresh of the account's token.
    on_granted: std::sync::Mutex<Option<GrantedHook>>,
}

impl TokenManager {
    pub fn new(secrets: Arc<dyn SecretStore>, state: StateHandle) -> Self {
        Self {
            secrets,
            state,
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

    /// An access token that can change nothing (`docs/design/writes.md` §8.2; SECURITY.md): `Dev1.AccessToken`'s,
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
                self.state.update(|s| s.last_error = WALLET_LOCKED.into());
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
                self.state.update(|s| {
                    s.state = SignInState::SignedOut;
                    s.last_error = SESSION_EXPIRED.into();
                    s.clear_account();
                });
                Err(AuthError::SignedOut)
            }
            Err(OAuthError::Rejected { error, description }) => {
                let message = format!("Microsoft rejected the token refresh: {error}: {description}");
                self.state.update(|s| s.last_error = message.clone());
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
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use url::Url;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::oauth::Endpoints;
    use crate::secret::MemoryStore;
    use crate::state::{AccountSnapshot, SignInState};

    fn manager(server: &MockServer, store: Arc<MemoryStore>) -> (TokenManager, StateHandle) {
        let state = StateHandle::new(AccountSnapshot { state: SignInState::SignedIn, ..AccountSnapshot::default() });
        let base = Url::parse(&format!("{}/", server.uri())).unwrap();
        let tokens = TokenManager::new(store, state.clone());
        tokens.set_oauth(Some(OAuthClient::new(
            reqwest::Client::new(),
            Endpoints { authority: base.clone(), graph: base },
            "cid".into(),
        )));
        (tokens, state)
    }

    fn response(access: &str, expires_in: u64) -> TokenResponse {
        TokenResponse { access_token: access.into(), expires_in, refresh_token: None, scope: None }
    }

    /// Every refresh asks for the installed client's scope, and what it grants is told to
    /// the hook — the scope the response names, or what was asked when it names none.
    #[tokio::test]
    async fn a_refresh_asks_for_the_installed_scope_and_reports_the_grant() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("scope=Files.ReadWrite+User.Read"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "AT-RW", "expires_in": 3600, "scope": "https://graph.microsoft.com/Files.ReadWrite User.Read"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (tokens, _) = manager(&server, Arc::new(MemoryStore::with_token("RT0")));
        let oauth = tokens.oauth.lock().unwrap().clone().unwrap();
        tokens.set_oauth(Some(oauth.with_scope(crate::oauth::READ_WRITE_SCOPES)));
        let granted = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&granted);
        tokens.set_on_granted(Arc::new(move |asked: &'static str, scope: &str| {
            seen.lock().unwrap().push((asked, scope.to_owned()))
        }));
        let (token, scope) = tokens.access_token_and_scope().await.unwrap();
        assert_eq!(token, "AT-RW");
        assert_eq!(scope, "https://graph.microsoft.com/Files.ReadWrite User.Read");
        assert_eq!(*granted.lock().unwrap(), vec![(crate::oauth::READ_WRITE_SCOPES, scope)]);
    }

    /// While a switch commits, no token can be refreshed — the cache is held — and
    /// the new token is cached only once the commit went through.
    #[tokio::test]
    async fn a_commit_holds_the_cache_and_seeds_only_when_it_succeeds() {
        let server = MockServer::start().await;
        mock_refresh(&server, 200, rotated(), 0).await;
        let (tokens, _) = manager(&server, Arc::new(MemoryStore::with_token("RT0")));
        tokens.seed(&response("AT-OLD", 3600)).await;
        let refused: Result<(), &str> = tokens.commit_as(&scoped("AT-NEW", "Files.ReadWrite"), crate::oauth::SCOPES, || Err("refused")).await;
        assert!(refused.is_err());
        assert_eq!(tokens.access_token().await.unwrap(), "AT-OLD", "a failed commit caches nothing");
        let held = tokens
            .commit_as(&scoped("AT-NEW", "Files.ReadWrite"), crate::oauth::SCOPES, || {
                Ok::<_, ()>(tokens.cached.try_lock().is_err())
            })
            .await
            .unwrap();
        assert!(held, "the cache is held while the commit runs");
        assert_eq!(tokens.access_token().await.unwrap(), "AT-NEW");
    }

    /// A token asked for with `Files.ReadWrite` is not used once the account asks
    /// for `Files.Read` only, however long it has left: the next call refreshes down.
    #[tokio::test]
    async fn a_token_asked_for_under_another_scope_is_refreshed_down() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("scope=Files.Read+User.Read"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "AT-RO", "expires_in": 3600})))
            .expect(1)
            .mount(&server)
            .await;
        let (tokens, _) = manager(&server, Arc::new(MemoryStore::with_token("RT0")));
        tokens.seed_as(&response("AT-RW", 3600), crate::oauth::READ_WRITE_SCOPES).await;
        assert_eq!(tokens.access_token().await.unwrap(), "AT-RO");
        assert_eq!(tokens.access_token().await.unwrap(), "AT-RO", "cached from then on");
    }

    /// `Dev1`'s token (`docs/design/writes.md` §8.2; SECURITY.md): a token that can write is never handed out. A
    /// read-write account's comes from a refresh that asks for `Files.Read` only, and the
    /// account's own token stays cached as it was; a read-only token is handed out as is.
    #[tokio::test]
    async fn the_read_only_token_is_a_subset_refresh_when_the_account_can_write() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("scope=Files.Read+User.Read"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "AT-RO", "expires_in": 3600, "refresh_token": "RT1", "scope": "Files.Read User.Read"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let store = Arc::new(MemoryStore::with_token("RT0"));
        let (tokens, _) = read_write_manager(&server, store.clone());
        tokens.seed(&scoped("AT-RW", "Files.ReadWrite User.Read")).await;
        assert_eq!(tokens.read_only_token().await.unwrap(), "AT-RO");
        assert_eq!(store.current().as_deref(), Some("RT1"), "the rotated refresh token is kept");
        assert_eq!(tokens.access_token().await.unwrap(), "AT-RW", "the account's own token is untouched");

        tokens.seed(&scoped("AT-READ", "Files.Read User.Read")).await;
        assert_eq!(tokens.read_only_token().await.unwrap(), "AT-READ", "no refresh for a read-only token");
    }

    /// Microsoft answering the subset refresh with a token that can write: refused.
    #[tokio::test]
    async fn a_read_only_token_that_can_write_is_refused() {
        let server = MockServer::start().await;
        mock_refresh(&server, 200, json!({"access_token": "AT-RW2", "expires_in": 3600, "scope": "Files.ReadWrite"}), 1).await;
        let (tokens, _) = read_write_manager(&server, Arc::new(MemoryStore::with_token("RT0")));
        tokens.seed(&scoped("AT-RW", "Files.ReadWrite User.Read")).await;
        assert!(matches!(tokens.read_only_token().await, Err(AuthError::Transient(_))));
    }

    fn scoped(access: &str, scope: &str) -> TokenResponse {
        TokenResponse { scope: Some(scope.into()), ..response(access, 3600) }
    }

    /// [`manager`], refreshing for a read-write account.
    fn read_write_manager(server: &MockServer, store: Arc<MemoryStore>) -> (TokenManager, StateHandle) {
        let (tokens, state) = manager(server, store);
        let oauth = tokens.oauth.lock().unwrap().clone().unwrap();
        tokens.set_oauth(Some(oauth.with_scope(crate::oauth::READ_WRITE_SCOPES)));
        (tokens, state)
    }

    async fn mock_refresh(server: &MockServer, status: u16, body: serde_json::Value, expect: u64) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body).set_delay(Duration::from_millis(100)))
            .expect(expect)
            .mount(server)
            .await;
    }

    fn rotated() -> serde_json::Value {
        json!({"token_type": "Bearer", "access_token": "AT1", "expires_in": 3600, "refresh_token": "RT1"})
    }

    #[tokio::test]
    async fn fresh_cached_token_needs_no_request() {
        let server = MockServer::start().await;
        mock_refresh(&server, 200, rotated(), 0).await;
        let (tokens, _) = manager(&server, Arc::new(MemoryStore::with_token("RT0")));
        tokens.seed(&response("AT0", 3600)).await;
        assert_eq!(tokens.access_token().await.unwrap(), "AT0");
    }

    #[tokio::test]
    async fn near_expiry_refreshes_and_stores_the_rotated_token() {
        let server = MockServer::start().await;
        mock_refresh(&server, 200, rotated(), 1).await;
        let store = Arc::new(MemoryStore::with_token("RT0"));
        let (tokens, _) = manager(&server, store.clone());
        tokens.seed(&response("AT0", 60)).await;
        assert_eq!(tokens.access_token().await.unwrap(), "AT1");
        assert_eq!(store.current().as_deref(), Some("RT1"));
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_refresh() {
        let server = MockServer::start().await;
        mock_refresh(&server, 200, rotated(), 1).await;
        let (tokens, _) = manager(&server, Arc::new(MemoryStore::with_token("RT0")));
        let tokens = Arc::new(tokens);
        let calls: Vec<_> = (0..10)
            .map(|_| {
                let tokens = tokens.clone();
                tokio::spawn(async move { tokens.access_token().await })
            })
            .collect();
        for call in calls {
            assert_eq!(call.await.unwrap().unwrap(), "AT1");
        }
    }

    #[tokio::test]
    async fn invalid_grant_signs_out() {
        let server = MockServer::start().await;
        mock_refresh(&server, 400, json!({"error": "invalid_grant", "error_description": "expired"}), 1).await;
        let store = Arc::new(MemoryStore::with_token("RT0"));
        let (tokens, state) = manager(&server, store.clone());
        assert_eq!(tokens.access_token().await, Err(AuthError::SignedOut));
        assert_eq!(store.current(), None);
        let s = state.get();
        assert_eq!(s.state, SignInState::SignedOut);
        assert_eq!(s.last_error, SESSION_EXPIRED);
    }

    #[tokio::test]
    async fn server_errors_are_transient_and_keep_the_session() {
        let server = MockServer::start().await;
        mock_refresh(&server, 503, json!({}), 1).await;
        let store = Arc::new(MemoryStore::with_token("RT0"));
        let (tokens, state) = manager(&server, store.clone());
        assert!(matches!(tokens.access_token().await, Err(AuthError::Transient(_))));
        assert_eq!(store.current().as_deref(), Some("RT0"));
        assert_eq!(state.get().state, SignInState::SignedIn);
    }

    #[tokio::test]
    async fn locked_wallet_is_reported() {
        let server = MockServer::start().await;
        mock_refresh(&server, 200, rotated(), 0).await;
        let store = Arc::new(MemoryStore::with_token("RT0"));
        store.set_locked(true);
        let (tokens, state) = manager(&server, store);
        assert_eq!(tokens.access_token().await, Err(AuthError::Locked));
        assert_eq!(state.get().last_error, WALLET_LOCKED);
        assert_eq!(state.get().state, SignInState::SignedIn);
    }

    #[tokio::test]
    async fn no_stored_token_means_signed_out() {
        let server = MockServer::start().await;
        mock_refresh(&server, 200, rotated(), 0).await;
        let (tokens, _) = manager(&server, Arc::new(MemoryStore::default()));
        assert_eq!(tokens.access_token().await, Err(AuthError::SignedOut));
    }

    #[tokio::test]
    async fn forget_deletes_the_secret_and_the_cache() {
        let server = MockServer::start().await;
        mock_refresh(&server, 200, rotated(), 0).await;
        let store = Arc::new(MemoryStore::with_token("RT0"));
        let (tokens, _) = manager(&server, store.clone());
        tokens.seed(&response("AT0", 3600)).await;
        tokens.forget().await.unwrap();
        assert_eq!(store.current(), None);
        assert_eq!(tokens.access_token().await, Err(AuthError::SignedOut));
    }

    #[tokio::test]
    async fn forget_waits_for_an_in_flight_refresh_to_commit_first() {
        let server = MockServer::start().await;
        mock_refresh(&server, 200, rotated(), 1).await;
        let store = Arc::new(MemoryStore::with_token("RT0"));
        let (tokens, _) = manager(&server, store.clone());
        tokens.seed(&response("AT0", 60)).await;
        let tokens = Arc::new(tokens);

        let refreshing = tokens.clone();
        let refresh = tokio::spawn(async move { refreshing.access_token().await });
        // Give the refresh a chance to take the cache lock and start its (100ms-delayed)
        // request before `forget` tries to take the same lock.
        tokio::time::sleep(Duration::from_millis(20)).await;
        tokens.forget().await.unwrap();

        // Whichever order the two actually ran in, the rotated token from the refresh
        // must never survive a `forget` that raced with it.
        let _ = refresh.await.unwrap();
        assert_eq!(store.current(), None);
    }
}
