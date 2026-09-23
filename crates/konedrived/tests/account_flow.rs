mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use konedrived::account::{AccountError, AccountService};
use konedrived::account_cache::{self, AccountInfo};
use konedrived::config::{Config, Paths};
use konedrived::oauth::TokenResponse;
use konedrived::secret::MemoryStore;
use konedrived::state::SignInState;
use konedrived::token::SESSION_EXPIRED;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn set_client_id_validates_and_persists() {
    let f = Fixture::new(Duration::from_secs(5)).await;
    assert_eq!(f.svc.set_client_id("nope"), Err(AccountError::InvalidClientId));
    f.svc.set_client_id(&format!("  {CLIENT_ID} ")).unwrap();
    assert_eq!(f.svc.state().get().client_id, CLIENT_ID);
    let saved = Config::load(&f.dir.path().join("config.toml")).unwrap();
    assert_eq!(saved.client_id, CLIENT_ID);
}

/// The final review's m5, on the account side. `config.toml` also holds the
/// sync sub-project's registered root — for an intercepted root, the only
/// record of a folder the helper still holds — and a copy that could not be
/// read used to be written back from defaults with just the new client id,
/// erasing that record. What could not be read is never overwritten.
#[tokio::test]
async fn set_client_id_never_overwrites_an_unreadable_config() {
    let f = Fixture::new(Duration::from_secs(5)).await;
    let config_file = f.dir.path().join("config.toml");
    let unreadable = "sync_root = \"/home/u/OneDrive\"\nthis is not [toml\n";
    std::fs::write(&config_file, unreadable).unwrap();

    assert!(matches!(f.svc.set_client_id(CLIENT_ID), Err(AccountError::Failed(_))));
    assert_eq!(std::fs::read_to_string(&config_file).unwrap(), unreadable);
}

#[tokio::test]
async fn sign_in_requires_a_client_id() {
    let f = Fixture::new(Duration::from_secs(5)).await;
    assert_eq!(f.svc.begin_sign_in().await, Err(AccountError::NoClientId));
}

#[tokio::test]
async fn sign_in_happy_path() {
    let f = Fixture::new(Duration::from_secs(10)).await;
    f.svc.set_client_id(CLIENT_ID).unwrap();
    let url = f.svc.begin_sign_in().await.unwrap();
    assert!(url.contains(&format!("client_id={CLIENT_ID}")), "{url}");
    assert_eq!(f.svc.state().get().state, SignInState::SigningIn);
    assert_eq!(f.svc.begin_sign_in().await, Err(AccountError::Busy));

    assert_eq!(simulate_browser(&url, "code=good-code").await.status(), 200);

    let s = wait_for(f.svc.state(), |s| s.quota_total != 0).await;
    assert_eq!(s.state, SignInState::SignedIn);
    assert_eq!(s.display_name, "Test User");
    assert_eq!(s.email, "test@outlook.com");
    assert_eq!((s.quota_used, s.quota_total), (1073741824, 5368709120));
    assert_eq!(s.last_error, "");
    assert_eq!(f.store.current().as_deref(), Some("RT1"));
    assert!(f.dir.path().join("account.json").exists());
    assert_eq!(f.svc.set_client_id(CLIENT_ID), Err(AccountError::Busy));
}

#[tokio::test]
async fn denied_consent_returns_to_signed_out() {
    let f = Fixture::new(Duration::from_secs(10)).await;
    f.svc.set_client_id(CLIENT_ID).unwrap();
    let url = f.svc.begin_sign_in().await.unwrap();
    simulate_browser(&url, "error=access_denied&error_description=cancelled").await;
    let s = wait_for(f.svc.state(), |s| s.state == SignInState::SignedOut).await;
    assert!(s.last_error.contains("denied"), "{}", s.last_error);
    assert_eq!(f.store.current(), None);
}

#[tokio::test]
async fn sign_in_times_out() {
    let f = Fixture::new(Duration::from_millis(300)).await;
    f.svc.set_client_id(CLIENT_ID).unwrap();
    f.svc.begin_sign_in().await.unwrap();
    let s = wait_for(f.svc.state(), |s| s.state == SignInState::SignedOut).await;
    assert!(s.last_error.contains("Timed out"), "{}", s.last_error);
}

#[tokio::test]
async fn cancel_returns_to_signed_out_without_error() {
    let f = Fixture::new(Duration::from_secs(10)).await;
    f.svc.set_client_id(CLIENT_ID).unwrap();
    f.svc.begin_sign_in().await.unwrap();
    f.svc.cancel_sign_in().await;
    let s = wait_for(f.svc.state(), |s| s.state == SignInState::SignedOut).await;
    assert_eq!(s.last_error, "");
}

#[tokio::test]
async fn rejected_code_returns_to_signed_out() {
    let f = Fixture::new(Duration::from_secs(10)).await;
    f.svc.set_client_id(CLIENT_ID).unwrap();
    let url = f.svc.begin_sign_in().await.unwrap();
    // The mock token endpoint only knows `good-code`; anything else gets a 404.
    simulate_browser(&url, "code=unknown-code").await;
    let s = wait_for(f.svc.state(), |s| s.state == SignInState::SignedOut).await;
    assert!(s.last_error.starts_with("Sign-in failed"), "{}", s.last_error);
    assert_eq!(f.store.current(), None);
}

#[tokio::test]
async fn sign_out_forgets_everything() {
    let f = Fixture::new(Duration::from_secs(10)).await;
    f.svc.set_client_id(CLIENT_ID).unwrap();
    let url = f.svc.begin_sign_in().await.unwrap();
    simulate_browser(&url, "code=good-code").await;
    wait_for(f.svc.state(), |s| s.quota_total != 0).await;

    f.svc.sign_out().await.unwrap();
    let s = f.svc.state().get();
    assert_eq!(s.state, SignInState::SignedOut);
    assert_eq!((s.display_name.as_str(), s.quota_total), ("", 0));
    assert_eq!(s.client_id, CLIENT_ID);
    assert_eq!(f.store.current(), None);
    assert!(!f.dir.path().join("account.json").exists());
}

#[tokio::test]
async fn profile_failure_does_not_undo_sign_in() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token_type": "Bearer", "access_token": "AT1", "expires_in": 3600, "refresh_token": "RT1"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::default());
    let svc = AccountService::new(Paths::in_dir(dir.path()), endpoints(&server), store.clone(), Duration::from_secs(10))
        .unwrap();
    svc.set_client_id(CLIENT_ID).unwrap();
    let url = svc.begin_sign_in().await.unwrap();
    simulate_browser(&url, "code=any").await;
    let s = wait_for(svc.state(), |s| !s.last_error.is_empty()).await;
    assert_eq!(s.state, SignInState::SignedIn);
    assert!(s.last_error.contains("account info"), "{}", s.last_error);
    assert_eq!(store.current().as_deref(), Some("RT1"));
}

/// `startup`'s wallet check must not clobber a sign-in that is already under way, e.g. one
/// answered from a D-Bus call that arrived in the window before the daemon finished
/// starting up (the daemon claims its bus name before this runs; see `main.rs`).
#[tokio::test]
async fn startup_does_not_clobber_an_in_progress_sign_in() {
    let server = MockServer::start().await;
    mock_microsoft(&server).await;
    let dir = tempfile::tempdir().unwrap();
    // A token already in the wallet, as if a previous session had signed in.
    let store = Arc::new(MemoryStore::with_token("RT-OLD"));
    let svc = AccountService::new(Paths::in_dir(dir.path()), endpoints(&server), store.clone(), Duration::from_secs(10))
        .unwrap();
    svc.set_client_id(CLIENT_ID).unwrap();
    svc.begin_sign_in().await.unwrap();
    assert_eq!(svc.state().get().state, SignInState::SigningIn);

    svc.startup().await;

    assert_eq!(svc.state().get().state, SignInState::SigningIn);
}

/// A cached access token can look fresh to `Instant`-based expiry (e.g. the daemon was
/// suspended past the token's real lifetime) while Microsoft Graph already rejects it.
/// `refresh_account_info` must invalidate the cache and retry once with a freshly
/// refreshed token instead of reporting the 401 forever.
#[tokio::test]
async fn graph_401_invalidates_the_cached_token_and_retries_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token_type": "Bearer", "access_token": "AT-FRESH", "expires_in": 3600, "refresh_token": "RT1"
        })))
        .expect(1)
        .mount(&server)
        .await;
    // The stale (but not-yet-`Instant`-expired) cached token is rejected on both routes,
    // whichever `try_join!` happens to poll first.
    Mock::given(method("GET"))
        .and(header("authorization", "Bearer AT-STALE"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .and(header("authorization", "Bearer AT-FRESH"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "displayName": "Test User", "mail": null, "userPrincipalName": "test@outlook.com"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive"))
        .and(header("authorization", "Bearer AT-FRESH"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "quota": {"used": 1u64, "total": 2u64}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::with_token("RT0"));
    let svc = AccountService::new(Paths::in_dir(dir.path()), endpoints(&server), store.clone(), Duration::from_secs(10))
        .unwrap();
    svc.set_client_id(CLIENT_ID).unwrap();
    svc.state().update(|s| s.state = SignInState::SignedIn);
    svc.tokens()
        .seed(&TokenResponse { access_token: "AT-STALE".into(), expires_in: 3600, refresh_token: None })
        .await;

    svc.refresh_account_info().await;

    let s = svc.state().get();
    assert_eq!(s.state, SignInState::SignedIn);
    assert_eq!(s.display_name, "Test User");
    assert_eq!((s.quota_used, s.quota_total), (1, 2));
    assert_eq!(s.last_error, "");
}

/// `TokenManager::access_token` also returns `SignedOut` when no client ID is configured
/// (e.g. `config.toml` was lost while a wallet item survives). Unlike the invalid_grant
/// path, the token manager never touched the state, so `refresh_account_info` must be the
/// one to bring it back to `signed-out` -- otherwise the daemon is stuck reporting
/// signed-in with `SetClientId` refused as `Busy`.
#[tokio::test]
async fn refresh_with_no_client_id_signs_out_and_explains_why() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::with_token("RT0"));
    let svc = AccountService::new(Paths::in_dir(dir.path()), endpoints(&server), store.clone(), Duration::from_secs(10))
        .unwrap();
    // As `startup` would after a lost config.toml: the wallet still holds a token, so the
    // service is signed in, but no client ID is configured.
    svc.state().update(|s| s.state = SignInState::SignedIn);

    svc.refresh_account_info().await;

    let s = svc.state().get();
    assert_eq!(s.state, SignInState::SignedOut);
    assert_eq!(s.last_error, "Set a client ID first, then sign in again.");
    assert_eq!((s.display_name.as_str(), s.quota_total), ("", 0));
    // The daemon is usable again: SetClientId is no longer refused as `Busy`.
    assert!(svc.set_client_id(CLIENT_ID).is_ok());
}

/// The other `SignedOut` case from `access_token`: no refresh token in the wallet at all
/// (e.g. it was removed outside the daemon), with a client ID configured.
#[tokio::test]
async fn refresh_with_missing_wallet_item_signs_out_and_explains_why() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::default());
    let svc = AccountService::new(Paths::in_dir(dir.path()), endpoints(&server), store.clone(), Duration::from_secs(10))
        .unwrap();
    svc.set_client_id(CLIENT_ID).unwrap();
    svc.state().update(|s| s.state = SignInState::SignedIn);

    svc.refresh_account_info().await;

    let s = svc.state().get();
    assert_eq!(s.state, SignInState::SignedOut);
    assert_eq!(s.last_error, "The stored sign-in was lost. Sign in again.");
}

/// `refresh_account_info` must not overwrite the token manager's own `SESSION_EXPIRED`
/// message with its generic "lost sign-in" text: the invalid_grant path already moved the
/// state to `SignedOut` itself, so the "still signed-in" guard must see nothing to do.
#[tokio::test]
async fn refresh_after_invalid_grant_keeps_the_session_expired_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": "invalid_grant", "error_description": "expired"
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::with_token("RT0"));
    let svc = AccountService::new(Paths::in_dir(dir.path()), endpoints(&server), store.clone(), Duration::from_secs(10))
        .unwrap();
    svc.set_client_id(CLIENT_ID).unwrap();
    svc.state().update(|s| s.state = SignInState::SignedIn);

    svc.refresh_account_info().await;

    let s = svc.state().get();
    assert_eq!(s.state, SignInState::SignedOut);
    assert_eq!(s.last_error, SESSION_EXPIRED);
}

#[tokio::test]
async fn startup_restores_session_from_wallet_and_cache() {
    let server = MockServer::start().await;
    mock_microsoft(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::in_dir(dir.path());
    Config { client_id: CLIENT_ID.into(), ..Config::default() }
        .save(&paths.config_file)
        .unwrap();
    account_cache::save(
        &paths.account_cache,
        &AccountInfo {
            display_name: "Cached User".into(),
            email: "cached@example.com".into(),
            quota_used: 1,
            quota_total: 2,
            fetched_at: 0,
        },
    )
    .unwrap();
    let store = Arc::new(MemoryStore::with_token("RT0"));
    let svc = AccountService::new(paths, endpoints(&server), store.clone(), Duration::from_secs(5)).unwrap();

    svc.startup().await;
    let s = svc.state().get();
    assert_eq!(s.state, SignInState::SignedIn);
    assert_eq!(s.display_name, "Cached User");

    wait_for(svc.state(), |s| s.display_name == "Test User").await;
    assert_eq!(store.current().as_deref(), Some("RT1"));
}

#[tokio::test]
async fn startup_without_token_stays_signed_out_and_drops_stale_cache() {
    let f = Fixture::new(Duration::from_secs(5)).await;
    let cache = f.dir.path().join("account.json");
    std::fs::write(&cache, "{}").unwrap();
    f.svc.startup().await;
    assert_eq!(f.svc.state().get().state, SignInState::SignedOut);
    assert!(!cache.exists());
}

/// A background refresh (from `refresh_account_info`) that is in flight when `sign_out`
/// runs must never leave a rotated refresh token behind in the wallet.
#[tokio::test]
async fn sign_out_during_refresh_keeps_wallet_empty() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "token_type": "Bearer", "access_token": "AT1", "expires_in": 3600, "refresh_token": "RT1"
                }))
                .set_delay(Duration::from_millis(300)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .and(header("authorization", "Bearer AT1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "displayName": "Test User", "mail": null, "userPrincipalName": "test@outlook.com"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive"))
        .and(header("authorization", "Bearer AT1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "quota": {"used": 1u64, "total": 2u64}
        })))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::with_token("RT0"));
    let svc = AccountService::new(Paths::in_dir(dir.path()), endpoints(&server), store.clone(), Duration::from_secs(10))
        .unwrap();
    // Without a client ID no OAuth client is installed and `access_token()` would return
    // `SignedOut` without ever making a request, making this test pass vacuously.
    svc.set_client_id(CLIENT_ID).unwrap();
    // As `startup` would: the wallet already holds a token, so the service is signed in.
    svc.state().update(|s| s.state = SignInState::SignedIn);

    let refreshing = svc.clone();
    let refresh = tokio::spawn(async move { refreshing.refresh_account_info().await });
    // Give the refresh a chance to start its (300ms-delayed) request before signing out.
    tokio::time::sleep(Duration::from_millis(50)).await;
    svc.sign_out().await.unwrap();
    refresh.await.unwrap();

    assert_eq!(store.current(), None);
    assert_eq!(svc.state().get().state, SignInState::SignedOut);
}

/// A `sign_out` that runs while a sign-in's token exchange is still in flight must win:
/// once the exchange completes, its (now stale) attempt must discard the tokens instead
/// of storing them and re-declaring the session signed in.
#[tokio::test]
async fn sign_out_during_token_exchange_discards_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "token_type": "Bearer", "access_token": "AT1", "expires_in": 3600, "refresh_token": "RT1"
                }))
                .set_delay(Duration::from_millis(300)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::default());
    let svc = AccountService::new(Paths::in_dir(dir.path()), endpoints(&server), store.clone(), Duration::from_secs(10))
        .unwrap();
    svc.set_client_id(CLIENT_ID).unwrap();
    let url = svc.begin_sign_in().await.unwrap();
    simulate_browser(&url, "code=good-code").await;
    // The exchange is now in flight (delayed 300ms); sign out before it completes.
    tokio::time::sleep(Duration::from_millis(50)).await;
    svc.sign_out().await.unwrap();
    assert_eq!(svc.state().get().state, SignInState::SignedOut);

    // Give the delayed exchange time to finish and attempt (and fail) its commit.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(store.current(), None);
    assert_eq!(svc.state().get().state, SignInState::SignedOut);
}

/// `cancel_sign_in` after the browser callback arrived must return to signed-out right
/// away, without waiting for the in-flight token exchange, and that exchange's eventual
/// result must not resurrect the session.
#[tokio::test]
async fn cancel_after_callback_signs_out() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({
                    "token_type": "Bearer", "access_token": "AT1", "expires_in": 3600, "refresh_token": "RT1"
                }))
                .set_delay(Duration::from_millis(300)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::default());
    let svc = AccountService::new(Paths::in_dir(dir.path()), endpoints(&server), store.clone(), Duration::from_secs(10))
        .unwrap();
    svc.set_client_id(CLIENT_ID).unwrap();
    let url = svc.begin_sign_in().await.unwrap();
    simulate_browser(&url, "code=good-code").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    svc.cancel_sign_in().await;
    // Immediate: does not wait for the delayed exchange.
    assert_eq!(svc.state().get().state, SignInState::SignedOut);
    assert_eq!(store.current(), None);

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(store.current(), None);
    assert_eq!(svc.state().get().state, SignInState::SignedOut);
}
