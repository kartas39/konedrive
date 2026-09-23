//! Access tokens for Graph callers: cached, refreshed on demand, one refresh at a time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::oauth::{OAuthClient, OAuthError, TokenResponse};
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
}

impl Cached {
    fn from_response(response: &TokenResponse) -> Self {
        Self {
            token: response.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(response.expires_in),
        }
    }
}

pub struct TokenManager {
    secrets: Arc<dyn SecretStore>,
    state: StateHandle,
    /// `None` while no client ID is configured.
    oauth: std::sync::Mutex<Option<OAuthClient>>,
    /// Held across a refresh, so concurrent callers wait for it instead of refreshing again.
    cached: tokio::sync::Mutex<Option<Cached>>,
}

impl TokenManager {
    pub fn new(secrets: Arc<dyn SecretStore>, state: StateHandle) -> Self {
        Self {
            secrets,
            state,
            oauth: std::sync::Mutex::new(None),
            cached: tokio::sync::Mutex::new(None),
        }
    }

    pub fn set_oauth(&self, oauth: Option<OAuthClient>) {
        *self.oauth.lock().unwrap() = oauth;
    }

    /// Caches the access token obtained at sign-in.
    pub async fn seed(&self, response: &TokenResponse) {
        *self.cached.lock().await = Some(Cached::from_response(response));
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
        let mut cached = self.cached.lock().await;
        if let Some(c) = cached.as_ref() {
            if c.expires_at > Instant::now() + REFRESH_MARGIN {
                return Ok(c.token.clone());
            }
        }
        let refresh_token = match self.secrets.load().await {
            Ok(Some(token)) => token,
            Ok(None) => return Err(AuthError::SignedOut),
            Err(SecretError::Locked) => {
                self.state.update(|s| s.last_error = WALLET_LOCKED.into());
                return Err(AuthError::Locked);
            }
            Err(e) => return Err(AuthError::Transient(e.to_string())),
        };
        let oauth = self.oauth.lock().unwrap().clone().ok_or(AuthError::SignedOut)?;
        match oauth.refresh(&refresh_token).await {
            Ok(response) => {
                if let Some(rotated) = response.refresh_token.as_deref() {
                    if rotated != refresh_token {
                        self.secrets
                            .store(rotated)
                            .await
                            .map_err(|e| AuthError::Transient(e.to_string()))?;
                    }
                }
                *cached = Some(Cached::from_response(&response));
                Ok(response.access_token)
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
        TokenResponse { access_token: access.into(), expires_in, refresh_token: None }
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
