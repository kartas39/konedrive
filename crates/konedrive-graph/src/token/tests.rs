use std::time::Duration;

use serde_json::json;
use url::Url;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::oauth::Endpoints;
use crate::secret::MemoryStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignInState {
    SignedOut,
    SignedIn,
}

#[derive(Debug, Clone)]
struct AccountSnapshot {
    state: SignInState,
    last_error: String,
}

/// The account's state as a refresh changes it: what the daemon's `StateHandle` does with a
/// [`RefreshReport`].
#[derive(Clone)]
struct StateHandle(Arc<std::sync::Mutex<AccountSnapshot>>);

impl StateHandle {
    fn get(&self) -> AccountSnapshot {
        self.0.lock().unwrap().clone()
    }
}

impl RefreshReport for StateHandle {
    fn failed(&self, message: &str) {
        self.0.lock().unwrap().last_error = message.to_owned();
    }

    fn signed_out(&self, message: &str) {
        *self.0.lock().unwrap() = AccountSnapshot { state: SignInState::SignedOut, last_error: message.to_owned() };
    }
}

fn manager(server: &MockServer, store: Arc<MemoryStore>) -> (TokenManager, StateHandle) {
    let state = StateHandle(Arc::new(std::sync::Mutex::new(AccountSnapshot { state: SignInState::SignedIn, last_error: String::new() })));
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

/// `TokenExport`'s token (`docs/design/writes.md` §8.2; SECURITY.md): a token that can write is never handed out. A
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

fn read_only_refresh(delay: Duration) -> Mock {
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("scope=Files.Read+User.Read"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"access_token": "AT-RO", "expires_in": 3600, "scope": "Files.Read User.Read"}))
                .set_delay(delay),
        )
}

/// A read-write account's read-only token is kept while it is fresh: asked for
/// again, it needs no request.
#[tokio::test]
#[ignore = "shows GR5: every read-only token of a read-write account is a refresh"]
async fn a_read_write_accounts_read_only_token_is_cached() {
    let server = MockServer::start().await;
    read_only_refresh(Duration::ZERO).mount(&server).await;
    let (tokens, _) = read_write_manager(&server, Arc::new(MemoryStore::with_token("RT0")));
    tokens.seed(&scoped("AT-RW", "Files.ReadWrite User.Read")).await;
    assert_eq!(tokens.read_only_token().await.unwrap(), "AT-RO");
    assert_eq!(tokens.read_only_token().await.unwrap(), "AT-RO");
    assert_eq!(server.received_requests().await.unwrap().len(), 1, "one refresh for both");
}

/// While a read-write account's read-only token is fetched, the account's own
/// cached token is still handed out: Graph calls do not wait for that request.
#[tokio::test]
#[ignore = "shows GR5: the read-only refresh holds the cache every Graph call reads"]
async fn the_accounts_own_token_does_not_wait_for_a_read_only_refresh() {
    let server = MockServer::start().await;
    read_only_refresh(Duration::from_secs(2)).mount(&server).await;
    let (tokens, _) = read_write_manager(&server, Arc::new(MemoryStore::with_token("RT0")));
    let tokens = Arc::new(tokens);
    tokens.seed(&scoped("AT-RW", "Files.ReadWrite User.Read")).await;
    let export = tokio::spawn({
        let tokens = tokens.clone();
        async move { tokens.read_only_token().await }
    });
    // The refresh is under way, and answers in two seconds.
    while server.received_requests().await.unwrap().is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let own = tokio::time::timeout(Duration::from_millis(500), tokens.access_token()).await;
    assert_eq!(own.expect("the account's cached token waits for the read-only refresh").unwrap(), "AT-RW");
    assert_eq!(export.await.unwrap().unwrap(), "AT-RO");
}
