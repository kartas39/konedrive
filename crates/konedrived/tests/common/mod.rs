#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use konedrived::account::AccountService;
use konedrived::config::Paths;
use konedrived::oauth::Endpoints;
use konedrived::secret::MemoryStore;
use konedrived::state::{AccountSnapshot, StateHandle};
use serde_json::json;
use url::Url;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub const CLIENT_ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

pub fn endpoints(server: &MockServer) -> Endpoints {
    let base = Url::parse(&format!("{}/", server.uri())).unwrap();
    Endpoints { authority: base.clone(), graph: base }
}

fn tokens_body() -> serde_json::Value {
    json!({"token_type": "Bearer", "access_token": "AT1", "expires_in": 3600, "refresh_token": "RT1"})
}

/// Token endpoint (code `good-code` and any refresh) plus `/me` and `/me/drive` for `AT1`.
pub async fn mock_microsoft(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=good-code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tokens_body()))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tokens_body()))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .and(header("authorization", "Bearer AT1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "displayName": "Test User", "mail": null, "userPrincipalName": "test@outlook.com"
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive"))
        .and(header("authorization", "Bearer AT1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "quota": {"used": 1073741824u64, "total": 5368709120u64}
        })))
        .mount(server)
        .await;
}

pub struct Fixture {
    pub server: MockServer,
    pub dir: tempfile::TempDir,
    pub store: Arc<MemoryStore>,
    pub svc: Arc<AccountService>,
}

impl Fixture {
    pub async fn new(sign_in_timeout: Duration) -> Self {
        let server = MockServer::start().await;
        mock_microsoft(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::default());
        let svc = AccountService::new(Paths::in_dir(dir.path()), endpoints(&server), store.clone(), sign_in_timeout)
            .unwrap();
        Self { server, dir, store, svc }
    }
}

/// Plays the browser: calls the authorization URL's redirect URI with `params` and its `state`.
pub async fn simulate_browser(authorize_url: &str, params: &str) -> reqwest::Response {
    let url = Url::parse(authorize_url).unwrap();
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    let redirect = query["redirect_uri"].replace("localhost", "127.0.0.1");
    reqwest::get(format!("{redirect}/?{params}&state={}", query["state"])).await.unwrap()
}

pub async fn wait_for(state: &StateHandle, pred: impl Fn(&AccountSnapshot) -> bool) -> AccountSnapshot {
    let mut changes = state.subscribe();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = changes.borrow_and_update().clone();
            if pred(&snapshot) {
                return snapshot;
            }
            changes.changed().await.expect("state channel closed");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; last state: {:?}", state.get()))
}
