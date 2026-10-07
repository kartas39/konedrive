// --- A folder that shows OneDrive ----------

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::*;
use crate::conditions::running;
use super::{persist, wait_until, Config, FakeHelper, Seen};
use konedrive_graph::drive::{DriveClient, RetryPolicy};
use crate::account::state::{AccountSnapshot, SignInState, StateHandle};
use crate::remote::listing::Schedule;
use konedrive_graph::token::{AuthError, StaticToken, TokenSource};

mod folder;
mod read_write;
mod settings;

struct World {
    server: MockServer,
    config: tempfile::TempDir,
    folder: tempfile::TempDir,
    /// Where the fake `balooctl6` lives: `calls` gets every
    /// `add`/`rm` it is run with, appended one per line; its
    /// `baloofilerc` is what is excluded already — nothing, unless a
    /// test writes it.; `crate::desktop::baloo`.
    baloo: tempfile::TempDir,
    /// A helper that acknowledges everything, at `sockets/helper.sock`:
    /// a folder that shows OneDrive is kept in step only with one
    /// (HS2). [`connected`] links a service to it.
    helper: FakeHelper,
    sockets: tempfile::TempDir,
}

impl Drop for World {
    fn drop(&mut self) {
        // A locked tree cannot be removed by the temporary directory.
        let _ = std::process::Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(self.folder.path())
            .status();
    }
}

/// A drive holding `docs/f.txt`, listed in full from the start and
/// with no changes since from its delta link `L1`.
async fn world() -> World {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D1"})))
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": [], "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L1", server.uri())})))
        .with_priority(1)
        .mount(&server).await;
    Mock::given(method("GET")).and(path("/me/drive/root/delta"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [
                {"id": "R", "root": {}, "folder": {}},
                {"id": "D", "name": "docs", "folder": {}, "parentReference": {"id": "R"}},
                {"id": "F", "name": "f.txt", "size": 3, "cTag": "c1", "file": {}, "parentReference": {"id": "D"}}
            ],
            "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L1", server.uri())
        })))
        .with_priority(5)
        .mount(&server).await;
    let baloo = tempfile::tempdir().unwrap();
    write_fake_balooctl6(baloo.path());
    let sockets = tempfile::tempdir().unwrap();
    let helper = FakeHelper::start(sockets.path().join("helper.sock"));
    World { server, config: tempfile::tempdir().unwrap(), folder: tempfile::tempdir().unwrap(), baloo, helper, sockets }
}

/// A new link to the world's helper.
async fn link(w: &World) -> HelperLink {
    HelperLink::connect(&w.sockets.path().join("helper.sock")).await.unwrap().0
}

/// [`service`], linked to the world's helper: what a OneDrive folder
/// is registered and kept in step with (HS2).
async fn connected(w: &World, signed_in: bool) -> Arc<SyncService> {
    service_with(w, account(signed_in), Some(link(w).await), Arc::new(StaticToken::new("T")))
}

/// The world's account as the write gate needs it to change OneDrive:
/// `config.toml` says read-write and records its drive, its token can write and was seen
/// to reach that drive, and the account runs read-write. `write_test_drive_ids` stays
/// empty: no list decides it.
fn let_write(service: &SyncService) {
    use crate::config::{ConfigError, Mode};
    let parts = testing::parts(service);
    let persist = &parts.persist;
    persist
        .store
        .update(|c| {
            let account = c.accounts.iter_mut().find(|a| a.id == persist.account).unwrap();
            account.mode = Mode::ReadWrite;
            account.drive_id = crate::config::DriveId::new("D1");
            Ok::<_, ConfigError>(())
        })
        .unwrap();
    parts.account.state().update(|s| {
        s.mode = Mode::ReadWrite;
        s.granted_scopes = "Files.ReadWrite offline_access".into();
        s.live_drive = "D1".into();
    });
}

/// A fake `balooctl6`, so these tests never reach the real Baloo:
/// `config add`/`config rm` are logged to `calls`, one
/// call per line. What is excluded already is read from the
/// `baloofilerc` beside it (`crate::desktop::baloo`), never
/// `~/.config`'s.
fn write_fake_balooctl6(dir: &std::path::Path) {
    let script = dir.join("balooctl6");
    let log = dir.join("calls");
    std::fs::write(&script, format!("#!/bin/sh\necho \"$@\" >> '{}'\n", log.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Every `add`/`rm` the fake `balooctl6` was run with, in order.
fn baloo_calls(w: &World) -> String {
    std::fs::read_to_string(w.baloo.path().join("calls")).unwrap_or_default()
}

/// Writes the test's `baloofilerc` as if `folder` (or, passed
/// directly, a directory above it) were already excluded — the
/// user's own doing, which says a registration must never
/// add to or a Forget take off. In the form KConfig writes it.
fn mark_already_excluded(w: &World, folder: &std::path::Path) {
    let line = format!("[General]\nexclude folders[$e]={}/\n", folder.display());
    std::fs::write(w.baloo.path().join("baloofilerc"), line).unwrap();
}

fn account(signed_in: bool) -> StateHandle {
    StateHandle::new(AccountSnapshot {
        state: if signed_in { SignInState::SignedIn } else { SignInState::SignedOut },
        ..AccountSnapshot::default()
    })
}

fn service(w: &World, signed_in: bool) -> Arc<SyncService> {
    service_with(w, account(signed_in), None, Arc::new(StaticToken::new("T")))
}

/// A service wired as `main` wires it — a drive, its paths — with an
/// hour between cycles, so that any cycle a test sees was asked for.
fn service_with(
    w: &World,
    account: StateHandle,
    link: Option<HelperLink>,
    tokens: Arc<dyn TokenSource>,
) -> Arc<SyncService> {
    made(w, wiring(w, account, tokens).link(link))
}

/// A signed-in service on `registry`, wired as [`service_with`] wires one: a restart of
/// the daemon whose registry knows what the machine's sources say.
fn service_on(w: &World, registry: &Arc<registry::Registry>) -> Arc<SyncService> {
    made(w, wiring(w, account(true), Arc::new(StaticToken::new("T"))).registry(registry))
}

/// The world's drive, through `tokens`.
fn drive(w: &World, tokens: Arc<dyn TokenSource>) -> DriveClient {
    DriveClient::new(Url::parse(&format!("{}/", w.server.uri())).unwrap(), tokens)
        .unwrap()
        .with_retry(RetryPolicy { attempts: 2, default_wait: Duration::from_millis(5), max_wait: Duration::from_millis(10) })
}

/// Where the world's services keep their OneDrive folder's own files.
fn sync_paths(w: &World) -> SyncPaths {
    SyncPaths {
        tree_db: w.config.path().join("tree.sqlite"),
        rescue_dir: w.config.path().join("rescued"),
        thumbnails: Some(w.config.path().join("thumbnails")),
    }
}

/// [`service_with`]'s wiring, for a test to change before the service is [`made`]: the
/// world's `config.toml`, its drive and paths, an hour between cycles, and the fake
/// `balooctl6` with a `baloofilerc` of the test's own — never the real ones, so these
/// tests never touch ~/.config/baloofilerc.
fn wiring(w: &World, account: StateHandle, tokens: Arc<dyn TokenSource>) -> testing::Builder {
    testing::wiring()
        .account(account)
        .persist(persist(&w.config.path().join("config.toml")))
        .onedrive(drive(w, tokens), sync_paths(w))
        .schedule(Schedule::polled(Duration::from_secs(3600), vec![Duration::from_millis(50)]))
        .baloo(crate::desktop::baloo::Baloo {
            program: Some(w.baloo.path().join("balooctl6")),
            settings: Some(w.baloo.path().join("baloofilerc")),
            ..crate::desktop::baloo::Baloo::disabled()
        })
}

/// The service of `wiring`. With no link, no helper runs either: a punch goes by "no
/// helper at all".
fn made(w: &World, wiring: testing::Builder) -> Arc<SyncService> {
    let service = wiring.build();
    service.hub().set_socket(w.config.path().join("no-helper.sock"));
    service
}

/// Tokens while the account reads signed in, and "signed out" — as
/// `TokenManager` answers once the refresh token is gone — otherwise.
struct AccountTokens {
    account: StateHandle,
    refused: AtomicUsize,
}

#[async_trait::async_trait]
impl TokenSource for AccountTokens {
    async fn access_token(&self) -> Result<String, AuthError> {
        if self.account.get().state == SignInState::SignedIn {
            Ok("T".into())
        } else {
            self.refused.fetch_add(1, Ordering::SeqCst);
            Err(AuthError::SignedOut)
        }
    }

    async fn invalidate(&self) {}
}

fn config_of(w: &World) -> Config {
    Config::load(&w.config.path().join("config.toml")).unwrap()
}

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

async fn requests(w: &World) -> usize {
    w.server.received_requests().await.unwrap().len()
}

async fn deltas(w: &World) -> usize {
    w.server.received_requests().await.unwrap().iter().filter(|r| r.url.path() == "/me/drive/root/delta").count()
}

/// Delta requests that started a listing of the whole drive.
async fn full_listings(w: &World) -> usize {
    w.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/me/drive/root/delta" && r.url.query().is_none())
        .count()
}

async fn wait_for_deltas(w: &World, more_than: usize) {
    for _ in 0..300 {
        if deltas(w).await > more_than {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no delta request came");
}

/// The first cycle is over: its counts are published only once the
/// folder has been made to match the tree.
async fn listed(service: &SyncService) {
    wait_until("the drive is listed into the folder", || service.items() == (2, 2, 0)).await;
}

/// OneDrive's answer about one item (or `root`), with the address of its page.
async fn mount_page(w: &World, route: &str, id: &str, url: &str) {
    Mock::given(method("GET")).and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": id, "webUrl": url})))
        .mount(&w.server).await;
}
