#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
            "id": "D1", "quota": {"used": 1073741824u64, "total": 5368709120u64}
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
        let svc = AccountService::single(dir.path(), endpoints(&server), store.clone(), sign_in_timeout)
            .await
            .unwrap();
        Self { server, dir, store, svc }
    }

    /// The account's `account.json`.
    pub fn cache(&self) -> PathBuf {
        cache_of(self.dir.path(), &self.svc)
    }
}

/// Where `svc`, made with `AccountService::single(dir, …)`, caches its name and quota.
pub fn cache_of(dir: &Path, svc: &AccountService) -> PathBuf {
    Paths::in_dir(dir).account(svc.id()).unwrap().account_cache
}

/// The daemon as `main` starts it (`accounts::start`), on a private bus: its files in
/// `dir`, Microsoft at `endpoints`, `wallet` for the Secret Service, neither Baloo nor
/// thumbnails, and local folders only (`Options::onedrive`). No helper supervisor runs.
pub async fn start_daemon(
    bus: &konedrive_dbus::testing::TestBus,
    dir: &Path,
    endpoints: Endpoints,
    wallet: Arc<konedrived::secret::MemoryWallet>,
    sign_in_timeout: Duration,
) -> konedrived::accounts::Daemon {
    let options = konedrived::accounts::Options {
        endpoints,
        wallet,
        sign_in_timeout,
        baloo: konedrived::sync::baloo::Baloo::disabled,
        thumbnails: None,
        onedrive: false,
    };
    start_daemon_with(bus, dir, options).await
}

/// [`start_daemon`] with `options` of the test's own. The hub's helper socket is a path in
/// `dir` where nothing listens, so no startup looks at a helper running on the machine.
pub async fn start_daemon_with(
    bus: &konedrive_dbus::testing::TestBus,
    dir: &Path,
    options: konedrived::accounts::Options,
) -> konedrived::accounts::Daemon {
    let hub = konedrived::sync::hub::HelperHub::new();
    hub.set_socket(dir.join("no-helper.sock"));
    konedrived::accounts::start_on(bus.builder(), Paths::in_dir(dir), options, hub).await.unwrap()
}

/// Polls `check` until it holds (proxy properties are read from the daemon each time).
pub async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..500 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

/// The live introspection of `path`.
pub async fn introspect(client: &zbus::Connection, path: &str) -> String {
    let introspectable = zbus::fdo::IntrospectableProxy::builder(client)
        .destination(konedrive_dbus::SERVICE_NAME)
        .unwrap()
        .path(path)
        .unwrap()
        .build()
        .await
        .unwrap();
    introspectable.introspect().await.unwrap()
}

/// Normalizes one interface to sorted lines such as `method SetLabel in=s out=`
/// and `property State s read`. Argument names are ignored.
pub fn signature_lines(xml: &str, interface: &str) -> Vec<String> {
    let start = xml
        .find(&format!("<interface name=\"{interface}\""))
        .unwrap_or_else(|| panic!("interface {interface} missing in:\n{xml}"));
    let end = start + xml[start..].find("</interface>").expect("unterminated interface");
    let mut lines = Vec::new();
    let mut method: Option<(String, String, String)> = None;
    let flush = |method: &mut Option<(String, String, String)>, lines: &mut Vec<String>| {
        if let Some((name, input, output)) = method.take() {
            lines.push(format!("method {name} in={input} out={output}"));
        }
    };
    for raw in xml[start..end].split('<').skip(1) {
        let tag = raw.split('>').next().unwrap_or_default().trim();
        let attr = |key: &str| -> String {
            let pattern = format!("{key}=\"");
            tag.find(&pattern)
                .map(|i| {
                    let rest = &tag[i + pattern.len()..];
                    rest[..rest.find('"').unwrap()].to_owned()
                })
                .unwrap_or_default()
        };
        if tag.starts_with("method ") {
            flush(&mut method, &mut lines);
            method = Some((attr("name"), String::new(), String::new()));
        } else if tag.starts_with("arg ") {
            if let Some((_, input, output)) = method.as_mut() {
                if attr("direction") == "out" {
                    output.push_str(&attr("type"));
                } else {
                    input.push_str(&attr("type"));
                }
            }
        } else if tag.starts_with("/method") {
            flush(&mut method, &mut lines);
        } else if tag.starts_with("property ") {
            flush(&mut method, &mut lines);
            lines.push(format!("property {} {} {}", attr("name"), attr("type"), attr("access")));
        } else if tag.starts_with("signal ") {
            flush(&mut method, &mut lines);
            lines.push(format!("signal {}", attr("name")));
        }
    }
    flush(&mut method, &mut lines);
    lines.sort();
    lines
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
