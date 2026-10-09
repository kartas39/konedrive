//! Exercises `konedrivectl`'s sync side — the binary's `sync` and `dev`
//! commands — against the daemon over a private test bus,
//! with one account, `Personal`: the case where no command needs `--account`.
//! The daemon is started as `konedrived` starts it (`tests/common`); the
//! harness reaches into the account's services only for what nothing on the
//! bus can do (seeding a token, a mocked drive).
//!
//! A folder that is registered is mounted. The tests here that register one
//! are refused before anything is mounted, or go through a mocked drive
//! ([`harness_onedrive`]).

#[path = "../common/mod.rs"]
mod common;
mod activity;
mod fuse;
mod refusals;
mod registration;
mod settings;
mod status;
#[cfg(feature = "dev-tools")]
mod token_export;
mod wording;

use std::sync::Arc;
use std::time::Duration;

use common::{err_text, out_text, run};
use konedrive_dbus::accounts::FolderProxies;
use konedrive_dbus::testing::TestBus;
use konedrived::account::AccountService;
use konedrived::account::state::SignInState;
use konedrived::sync::SyncService;

/// Polls `pred` every 20 ms for up to 5 s — the same shape
/// `konedrived`'s own tests use for a background listing to catch up.
async fn wait_for(mut pred: impl FnMut() -> bool) {
    for _ in 0..250 {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition was not met within 5s");
}

struct Harness {
    /// The account's folder.
    proxy: FolderProxies<'static>,
    dir: tempfile::TempDir,
    /// The daemon-side service itself, for what nothing on the bus can set.
    service: Arc<SyncService>,
    /// The account service, so the token-export test can seed an access
    /// token directly rather than going through a real sign-in.
    account: Arc<AccountService>,
    _daemon: konedrived::daemon::startup::Daemon,
    _config_dir: tempfile::TempDir,
    _bus: TestBus,
}

/// The daemon with one account, `Personal`, signed in, and no drive.
async fn harness() -> Harness {
    build_harness(true).await
}

/// As [`harness`], but nobody has signed in to the account: `RegisterRoot`
/// is refused for that.
async fn harness_signed_out() -> Harness {
    build_harness(false).await
}

/// The account is marked signed in (its `StateHandle` set to `SignedIn`, with
/// no token behind it) unless `signed_in` is false: `RegisterRoot` binds a
/// folder only to a signed-in account. The account has no drive, so no folder can be
/// registered for it, unless a test gives it one ([`harness_onedrive`]).
async fn build_harness(signed_in: bool) -> Harness {
    build_harness_showing(signed_in, konedrived::daemon::manager::no_drive()).await
}

/// [`build_harness`], whose account's folder shows what `drive` gives.
async fn build_harness_showing(
    signed_in: bool,
    drive: konedrived::daemon::manager::DriveOf,
) -> Harness {
    let bus = TestBus::start();
    let config_dir = tempfile::tempdir().unwrap();
    let daemon = common::start_daemon_showing(&bus, config_dir.path(), drive).await;

    let account = daemon.manager.add("Personal", &daemon.connection).await.unwrap();
    if signed_in {
        account.account.state().update(|s| s.state = SignInState::SignedIn);
    }

    let client = bus.connect().await;
    let proxy = FolderProxies::new(&client, account.path.clone()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    Harness {
        proxy,
        dir,
        service: Arc::clone(&account.sync),
        account: Arc::clone(&account.account),
        _daemon: daemon,
        _config_dir: config_dir,
        _bus: bus,
    }
}

/// A signed-in harness whose account's folder shows the
/// drive of the mocked Graph at `graph`.
async fn harness_showing(graph: &wiremock::MockServer) -> Harness {
    let drive = konedrive_graph::drive::DriveClient::new(
        url::Url::parse(&format!("{}/", graph.uri())).unwrap(),
        std::sync::Arc::new(konedrive_graph::token::StaticToken::new("T")),
    )
    .unwrap();
    build_harness_showing(true, Arc::new(move |_| Ok(Some(drive.clone())))).await
}

/// A signed-in harness with a drive on a mocked Graph whose listing holds
/// one folder, one file inside it, and the Personal Vault (skipped) — the
/// shape `sync skipped`, `sync status`'s `Items:`/`Skipped:` lines, and
/// `sync refresh` all need a real listing to exercise.
async fn harness_onedrive() -> (Harness, wiremock::MockServer) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let graph = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me/drive"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": "D1"})))
        .mount(&graph)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive/root/delta"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "value": [
                {"id": "R", "root": {}, "folder": {}},
                {"id": "D", "name": "docs", "folder": {}, "parentReference": {"id": "R"}},
                {"id": "F", "name": "f.txt", "size": 3, "cTag": "c1", "file": {}, "parentReference": {"id": "D"}},
                {"id": "V", "name": "Personal Vault", "folder": {}, "specialFolder": {"name": "vault"}, "parentReference": {"id": "R"}}
            ],
            "@odata.deltaLink": format!("{}/me/drive/root/delta?token=L1", graph.uri())
        })))
        .mount(&graph)
        .await;
    let f = harness_showing(&graph).await;
    (f, graph)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_status_reports_no_folder() {
    let f = harness().await;
    let text = out_text(&run(f._bus.address(), &["sync", "status"]));
    assert!(text.lines().any(|l| l == "Folder:                 (none)"), "{text}");
}

/// Runs a `sync` command the daemon must refuse and returns what the person
/// running it was told. Every refusal exits non-zero, prints nothing on
/// stdout (so nothing there can read as success), and keeps the D-Bus error
/// name out of the explanation: the name is how the CLI knows which
/// refusal it is, not something the person reading it should have to decode.
fn refused(bus_addr: &str, args: &[&str]) -> String {
    let out = run(bus_addr, args);
    assert!(!out.status.success(), "a refusal must not exit 0: {out:?}");
    assert!(out_text(&out).is_empty(), "a refusal must print nothing on stdout: {out:?}");
    let told = err_text(&out);
    assert!(
        !told.contains(konedrive_dbus::ERROR_PREFIX),
        "the D-Bus error name is for programs, not for the person reading this: {told}"
    );
    told
}
