//! The per-account mode (`docs/design/writes.md` §2), against a fake Microsoft (wiremock) and
//! over a private bus. The development gate refuses by default; read-only → read-write is
//! written only once the grant arrives; a cancelled, refused, narrower or foreign sign-in
//! changes nothing; read-write → read-only is a subset refresh; `Dev1`'s token stays
//! read-only; a read-write account whose token cannot write runs read-only.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use konedrive_dbus::accounts::{Account1Proxy, Accounts1Proxy, Dev1Proxy};
use konedrive_dbus::error_name;
use konedrive_dbus::testing::TestBus;
use konedrived::account::{
    AccountService, ModeError, PendingUploads, CONFIG_UNREADABLE, DRIVE_NOT_SEEN, GATE_KEEPS_READ_ONLY, SIGN_IN_TO_WRITE,
};
use konedrived::account_cache::{self, AccountInfo};
use konedrived::config::{ConfigError, ConfigStore, Mode, Paths};
use konedrived::secret::{MemoryStore, MemoryWallet, Slot};
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const READ_WRITE: &str = "Files.ReadWrite User.Read offline_access";

/// The token endpoint's answers beyond `mock_microsoft`'s: the code `rw-code` grants
/// `Files.ReadWrite` (AT2/RT2, drive D1), `narrow-code` grants only `Files.Read` (AT3, D1),
/// `other-code` is another Microsoft account (AT4, drive D9), and `wide-code` grants
/// `Files.ReadWrite` whatever was asked (AT5, D1). A refresh answers what it was asked for:
/// `AT-RW` for `Files.ReadWrite`, `AT-RO` for `Files.Read` — or, `wide`, `AT-WIDE`, which can
/// write, for `Files.Read` too.
async fn mock_read_write(server: &MockServer, wide: bool) {
    let code = |code: &str, body: serde_json::Value| {
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains(format!("code={code}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .with_priority(1)
    };
    code("rw-code", json!({"access_token": "AT2", "expires_in": 3600, "refresh_token": "RT2", "scope": READ_WRITE}))
        .mount(server)
        .await;
    code("narrow-code", json!({"access_token": "AT3", "expires_in": 3600, "refresh_token": "RT3", "scope": "Files.Read User.Read offline_access"}))
        .mount(server)
        .await;
    code("other-code", json!({"access_token": "AT4", "expires_in": 3600, "refresh_token": "RT4", "scope": READ_WRITE}))
        .mount(server)
        .await;
    code("wide-code", json!({"access_token": "AT5", "expires_in": 3600, "refresh_token": "RT1", "scope": READ_WRITE}))
        .mount(server)
        .await;
    let tokens = [("AT2", "D1"), ("AT3", "D1"), ("AT4", "D9"), ("AT5", "D1"), ("AT-RW", "D1"), ("AT-RO", "D1"), ("AT-WIDE", "D1")];
    for (token, drive) in tokens {
        Mock::given(method("GET"))
            .and(path("/me/drive"))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": drive, "quota": {"used": 1, "total": 2}})))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/me"))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"displayName": "Test User", "userPrincipalName": "test@outlook.com"})))
            .mount(server)
            .await;
    }
    let refresh = |scope: &str, body: serde_json::Value| {
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains(format!("scope={scope}+User.Read")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .with_priority(1)
    };
    refresh("Files.ReadWrite", json!({"access_token": "AT-RW", "expires_in": 3600, "refresh_token": "RT2", "scope": READ_WRITE}))
        .mount(server)
        .await;
    let read = if wide {
        json!({"access_token": "AT-WIDE", "expires_in": 3600, "scope": READ_WRITE})
    } else {
        json!({"access_token": "AT-RO", "expires_in": 3600, "scope": "Files.Read User.Read offline_access"})
    };
    refresh("Files.Read", read).mount(server).await;
}

/// The scope every refresh asked for so far, in order.
async fn refreshes(server: &MockServer) -> Vec<String> {
    let requests = server.received_requests().await.unwrap();
    requests
        .iter()
        .filter(|r| r.url.path() == "/token")
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .filter(|body| body.contains("grant_type=refresh_token"))
        .map(|body| url::form_urlencoded::parse(body.as_bytes()).find(|(k, _)| k == "scope").map(|(_, v)| v.into_owned()).unwrap_or_default())
        .collect()
}

struct Setup {
    server: MockServer,
    wallet: Arc<MemoryWallet>,
    daemon: konedrived::accounts::Daemon,
    account: Account1Proxy<'static>,
    dev: Dev1Proxy<'static>,
    service: Arc<AccountService>,
    id: String,
    _dir: tempfile::TempDir,
    _bus: TestBus,
}

impl Setup {
    fn config(&self) -> &Arc<ConfigStore> {
        self.daemon.manager.config()
    }

    fn configured(&self) -> Mode {
        self.config().account(&self.id).unwrap().mode
    }

    fn refresh_token(&self) -> Option<String> {
        self.wallet.current(&Slot::Account(self.id.clone()))
    }
}

/// A daemon whose one account, "Test", is signed in read-only to drive D1 (code `good-code`:
/// AT1/RT1), with `allowed` as `write_test_drive_ids`.
async fn signed_in(allowed: &[&str]) -> Setup {
    signed_in_with(allowed, "good-code", false).await
}

/// [`signed_in`], with the sign-in's `code`, and `wide` for [`mock_read_write`].
async fn signed_in_with(allowed: &[&str], code: &str, wide: bool) -> Setup {
    let bus = TestBus::start();
    let server = MockServer::start().await;
    mock_microsoft(&server).await;
    mock_read_write(&server, wide).await;
    let dir = tempfile::tempdir().unwrap();
    let wallet = Arc::new(MemoryWallet::default());
    let daemon = start_daemon(&bus, dir.path(), endpoints(&server), wallet.clone(), Duration::from_secs(10)).await;
    let allowed: Vec<String> = allowed.iter().map(|d| d.to_string()).collect();
    daemon
        .manager
        .config()
        .update(|config| {
            config.write_test_drive_ids = allowed;
            Ok::<_, ConfigError>(())
        })
        .unwrap();
    let client = bus.connect().await;
    let manager = Accounts1Proxy::new(&client).await.unwrap();
    manager.set_client_id(CLIENT_ID).await.unwrap();
    let path = manager.add("Test").await.unwrap();
    let id = path.as_str().rsplit('/').next().unwrap().to_owned();
    let account = Account1Proxy::builder(&client)
        .path(path.clone())
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();
    let dev = Dev1Proxy::new(&client, path).await.unwrap();
    let url = account.begin_sign_in().await.unwrap();
    assert_eq!(simulate_browser(&url, &format!("code={code}")).await.status(), 200);
    let proxy = &account;
    eventually("signed in", || async move { proxy.state().await.unwrap() == "signed-in" }).await;
    let service = Arc::clone(&daemon.manager.accounts()[0].account);
    let config = Arc::clone(daemon.manager.config());
    let recorded = || config.account(service.id()).unwrap().drive_id == "D1";
    for _ in 0..500 {
        if recorded() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(recorded(), "the sign-in records the drive");
    eventually("the account's email", || async move { proxy.email().await.unwrap() == "test@outlook.com" }).await;
    assert_eq!(account.mode().await.unwrap(), "read-only");
    Setup { server, wallet, daemon, account, dev, service, id, _dir: dir, _bus: bus }
}

async fn wait_for_error(account: &Account1Proxy<'static>, words: &str) -> String {
    eventually(words, || async move { account.last_error().await.unwrap().contains(words) }).await;
    account.last_error().await.unwrap()
}

/// The gate refuses by default — `write_test_drive_ids` is empty — the switch and the
/// harness's token alike, and a hand edit of `config.toml` to read-write does not get around
/// it: the account runs read-only, says why, and keeps refreshing with `Files.Read`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_gate_refuses_read_write_by_default() {
    let s = signed_in(&[]).await;
    let refused = s.account.set_mode("read-write", false).await.unwrap_err();
    assert_eq!(error_name(&refused), Some("org.konedrive.Error.WritesNotAllowed"), "{refused:?}");
    let refused = s.dev.read_write_access_token().await.unwrap_err();
    assert_eq!(error_name(&refused), Some("org.konedrive.Error.WritesNotAllowed"), "{refused:?}");
    assert_eq!((s.configured(), s.account.mode().await.unwrap().as_str()), (Mode::ReadOnly, "read-only"));
    assert_eq!(s.account.last_error().await.unwrap(), "");

    s.config().update_account(&s.id, |a| {
        a.mode = Mode::ReadWrite;
        Ok::<_, ConfigError>(())
    })
    .unwrap();
    s.account.refresh_account_info().await.unwrap();
    assert_eq!(wait_for_error(&s.account, "write_test_drive_ids").await, GATE_KEEPS_READ_ONLY);
    assert_eq!(s.account.mode().await.unwrap(), "read-only");
    s.service.tokens().invalidate().await;
    assert_eq!(s.service.tokens().access_token().await.unwrap(), "AT-RO");
    assert_eq!(refreshes(&s.server).await.last().unwrap(), "Files.Read User.Read offline_access");

    let refused = s.account.set_mode("read-write-please", false).await.unwrap_err();
    assert_eq!(error_name(&refused), Some("org.freedesktop.DBus.Error.InvalidArgs"), "{refused:?}");
}

/// Read-only → read-write: the sign-in asks for `Files.ReadWrite`, and nothing is written
/// until its token response grants it; then the refresh token is the new one, the mode is
/// written and published, and every refresh asks for `Files.ReadWrite`. `Dev1.AccessToken`
/// still hands out a `Files.Read` token, from a subset refresh; the harness's token can
/// write. Read-write → read-only needs no sign-in: the next refresh asks for `Files.Read`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_write_is_written_only_once_the_grant_arrives_and_read_only_is_a_subset_refresh() {
    let s = signed_in(&["D1"]).await;
    let url = s.account.set_mode("read-write", false).await.unwrap();
    let query: HashMap<String, String> = url::Url::parse(&url).unwrap().query_pairs().into_owned().collect();
    assert_eq!(query["scope"], READ_WRITE);
    // Pinned to the account: its password asked for again, its name filled in.
    assert_eq!((query["prompt"].as_str(), query["login_hint"].as_str()), ("login", "test@outlook.com"));
    assert_eq!(s.configured(), Mode::ReadOnly, "nothing is written before the grant");
    assert_eq!(s.account.mode().await.unwrap(), "read-only");

    assert_eq!(simulate_browser(&url, "code=rw-code").await.status(), 200);
    let account = &s.account;
    eventually("read-write", || async move { account.mode().await.unwrap() == "read-write" }).await;
    assert_eq!(s.configured(), Mode::ReadWrite);
    assert_eq!(s.refresh_token().as_deref(), Some("RT2"));
    assert_eq!(s.account.state().await.unwrap(), "signed-in");
    assert_eq!(s.account.last_error().await.unwrap(), "");
    let cache = Paths::in_dir(s._dir.path()).account(&s.id).unwrap().account_cache;
    assert_eq!(account_cache::load(&cache).unwrap().granted_scopes, READ_WRITE, "kept with the account");

    assert_eq!(s.dev.access_token().await.unwrap(), "AT-RO", "Dev1's token stays read-only");
    assert_eq!(refreshes(&s.server).await, vec!["Files.Read User.Read offline_access"]);
    assert_eq!(s.dev.read_write_access_token().await.unwrap(), "AT2");
    s.service.tokens().invalidate().await;
    assert_eq!(s.service.tokens().access_token().await.unwrap(), "AT-RW");
    assert_eq!(refreshes(&s.server).await.last().unwrap(), READ_WRITE, "every refresh asks for the mode's scope");
    assert_eq!(s.account.set_mode("read-write", false).await.unwrap(), "", "read-write already: no sign-in");

    assert_eq!(s.account.set_mode("read-only", false).await.unwrap(), "", "no sign-in");
    eventually("read-only", || async move { account.mode().await.unwrap() == "read-only" }).await;
    assert_eq!(s.configured(), Mode::ReadOnly);
    assert_eq!(s.service.tokens().access_token().await.unwrap(), "AT-RO", "the token that could write is gone");
    assert_eq!(refreshes(&s.server).await.last().unwrap(), "Files.Read User.Read offline_access");
    let refused = s.dev.read_write_access_token().await.unwrap_err();
    assert_eq!(error_name(&refused), Some("org.konedrive.Error.ModeNotGranted"), "{refused:?}");
    assert_eq!(s.account.state().await.unwrap(), "signed-in");
}

/// A switch to read-write that is cancelled, refused in the browser, granted without
/// `Files.ReadWrite`, or signed in as another Microsoft account changes nothing: the account
/// stays signed in, read-only, with its refresh token; `LastError` says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_that_is_not_granted_changes_nothing() {
    let s = signed_in(&["D1", "D9"]).await;
    let config_before = std::fs::read_to_string(s.config().file()).unwrap();
    let unchanged = |s: &Setup| {
        assert_eq!(std::fs::read_to_string(s.config().file()).unwrap(), config_before);
        assert_eq!(s.refresh_token().as_deref(), Some("RT1"));
    };

    s.account.set_mode("read-write", false).await.unwrap();
    s.account.cancel_sign_in().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(s.account.last_error().await.unwrap(), "", "a cancel says nothing");
    unchanged(&s);

    let url = s.account.set_mode("read-write", false).await.unwrap();
    simulate_browser(&url, "error=access_denied&error_description=no").await;
    wait_for_error(&s.account, "denied").await;
    unchanged(&s);

    let url = s.account.set_mode("read-write", false).await.unwrap();
    assert_eq!(s.account.last_error().await.unwrap(), "", "a new switch starts afresh");
    simulate_browser(&url, "code=narrow-code").await;
    wait_for_error(&s.account, "did not allow konedrive to change files").await;
    unchanged(&s);

    let url = s.account.set_mode("read-write", false).await.unwrap();
    simulate_browser(&url, "code=other-code").await;
    wait_for_error(&s.account, "different Microsoft account").await;
    unchanged(&s);

    assert_eq!(s.account.mode().await.unwrap(), "read-only");
    assert_eq!(s.account.state().await.unwrap(), "signed-in");
    assert_eq!(s.config().account(&s.id).unwrap().drive_id, "D1");
}

/// A folder whose uploads wait, as `PendingUploads` says (the outbox worker's outbox, faked here).
struct Waiting {
    pending: u64,
    dropped: AtomicBool,
}

#[async_trait::async_trait]
impl PendingUploads for Waiting {
    async fn pending_uploads(&self) -> u64 {
        if self.dropped.load(Ordering::SeqCst) {
            0
        } else {
            self.pending
        }
    }

    async fn drop_pending_uploads(&self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// Read-write → read-only is refused `PendingUploads` while changes wait, and forced, drops
/// them and switches.
#[tokio::test]
async fn a_switch_to_read_only_waits_for_uploads_unless_forced() {
    let f = Fixture::new(Duration::from_secs(5)).await;
    f.svc
        .config()
        .update_account(f.svc.id(), |a| {
            a.mode = Mode::ReadWrite;
            Ok::<_, ConfigError>(())
        })
        .unwrap();
    let waiting = Arc::new(Waiting { pending: 3, dropped: AtomicBool::new(false) });
    let folder: std::sync::Weak<Waiting> = Arc::downgrade(&waiting);
    f.svc.set_uploads(folder);
    let refused = f.svc.set_mode("read-only", false).await.unwrap_err();
    assert!(matches!(&refused, ModeError::PendingUploads(why) if why.starts_with("3 changes")), "{refused:?}");
    assert_eq!(f.svc.configured_mode(), Mode::ReadWrite, "nothing changed");
    assert!(!waiting.dropped.load(Ordering::SeqCst));
    assert_eq!(f.svc.set_mode("read-only", true).await.unwrap(), "");
    assert!(waiting.dropped.load(Ordering::SeqCst), "forced: the rows are dropped");
    assert_eq!(f.svc.configured_mode(), Mode::ReadOnly);
}

/// Read-only already — a switch nobody forced kept the changes — a forced
/// switch to read-only drops them all the same: the way out of a Forget or a Remove refused
/// `PendingUploads`. Unforced, it changes nothing.
#[tokio::test]
async fn a_forced_switch_drops_what_a_read_only_account_kept() {
    let f = Fixture::new(Duration::from_secs(5)).await;
    let waiting = Arc::new(Waiting { pending: 2, dropped: AtomicBool::new(false) });
    let folder: std::sync::Weak<Waiting> = Arc::downgrade(&waiting);
    f.svc.set_uploads(folder);
    assert_eq!(f.svc.configured_mode(), Mode::ReadOnly);
    assert_eq!(f.svc.set_mode("read-only", false).await.unwrap(), "");
    assert!(!waiting.dropped.load(Ordering::SeqCst), "unforced: kept");
    assert_eq!(f.svc.set_mode("read-only", true).await.unwrap(), "");
    assert!(waiting.dropped.load(Ordering::SeqCst), "forced: dropped");
}

/// The mode an account runs in at start: read-write only when `config.toml` says so, the gate
/// lets its drive through, and the scopes its last token was granted — kept in
/// `account.json` — carry `Files.ReadWrite`. Otherwise read-only, `LastError` says why, and
/// the refresh asks for `Files.Read`, never for more than was granted.
#[tokio::test]
async fn a_read_write_account_runs_read_write_only_with_the_grant_and_the_gate() {
    const READ: &str = "Files.Read User.Read offline_access";
    // (granted, drive the token was seen to reach, allowed, mode, LastError, next refresh asks)
    let cases = [
        (READ_WRITE, "D1", true, Mode::ReadWrite, "", READ_WRITE),
        (READ, "D1", true, Mode::ReadOnly, SIGN_IN_TO_WRITE, READ),
        ("", "", true, Mode::ReadOnly, SIGN_IN_TO_WRITE, READ),
        (READ_WRITE, "D1", false, Mode::ReadOnly, GATE_KEEPS_READ_ONLY, READ),
        (READ_WRITE, "", true, Mode::ReadOnly, DRIVE_NOT_SEEN, READ),
        (READ_WRITE, "D2", true, Mode::ReadOnly, "reaches the OneDrive drive D2", READ),
    ];
    for (granted, seen, allowed, mode, error, asked) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "AT", "expires_in": 3600})))
            .mount(&server)
            .await;
        // Slow, so that what startup set up is read before the account's info comes back.
        for route in ["/me", "/me/drive"] {
            Mock::given(method("GET"))
                .and(path(route))
                .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)).set_body_json(json!({"id": "D1"})))
                .mount(&server)
                .await;
        }
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::in_dir(dir.path());
        let config = ConfigStore::open(&paths, async { false }).await;
        config.set_client_id(CLIENT_ID).unwrap();
        let id = config.add_account("Test").unwrap().id;
        config.record_drive(&id, "D1").unwrap();
        config
            .update(|c| {
                c.account_mut(&id).unwrap().mode = Mode::ReadWrite;
                c.write_test_drive_ids = if allowed { vec!["D1".into()] } else { Vec::new() };
                Ok::<_, ConfigError>(())
            })
            .unwrap();
        if !granted.is_empty() {
            let info = AccountInfo {
                email: "test@outlook.com".into(),
                granted_scopes: granted.into(),
                drive_id: seen.into(),
                ..AccountInfo::default()
            };
            account_cache::save(&paths.account(&id).unwrap().account_cache, &info).unwrap();
        }
        let svc = AccountService::single(dir.path(), endpoints(&server), Arc::new(MemoryStore::with_token("RT0")), Duration::from_secs(5))
            .await
            .unwrap();
        svc.startup().await;
        let case = format!("granted {granted:?}, seen {seen:?}, allowed {allowed}");
        assert_eq!(svc.mode(), mode, "{case}");
        let last_error = svc.state().get().last_error;
        assert!(if error.is_empty() { last_error.is_empty() } else { last_error.contains(error) }, "{case}: {last_error}");
        svc.tokens().invalidate().await;
        svc.tokens().access_token().await.unwrap();
        assert_eq!(refreshes(&server).await.last().unwrap(), asked, "{case}");
    }
}

/// A read-only request that Microsoft answers with a token that can write —
/// consent it still holds — for an account `config.toml` sets to read-write by hand and the
/// gate does not let through. The account stays read-only and says so; neither `Dev1` token
/// is handed out; the wide token is used to read only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_request_answered_with_write_access_stays_read_only() {
    let s = signed_in_with(&[], "wide-code", true).await;
    let account = &s.account;
    eventually("the wider grant said", || async move { account.last_error().await.unwrap().contains("consent") }).await;
    s.config()
        .update_account(&s.id, |a| {
            a.mode = Mode::ReadWrite;
            Ok::<_, ConfigError>(())
        })
        .unwrap();
    s.account.refresh_account_info().await.unwrap();
    wait_for_error(&s.account, "account.live.com/consent/Manage").await;
    assert_eq!(s.account.mode().await.unwrap(), "read-only");
    let refused = s.dev.read_write_access_token().await.unwrap_err();
    assert_eq!(error_name(&refused), Some("org.konedrive.Error.WritesNotAllowed"), "{refused:?}");
    let refused = s.dev.access_token().await.unwrap_err();
    assert_eq!(error_name(&refused), Some("org.konedrive.Error.Failed"), "never a token that can write: {refused:?}");
    s.service.tokens().invalidate().await;
    assert_eq!(s.service.tokens().access_token().await.unwrap(), "AT-WIDE", "used to read");
    assert_eq!(refreshes(&s.server).await.last().unwrap(), "Files.Read User.Read offline_access");
    assert_eq!(s.account.mode().await.unwrap(), "read-only");
    assert_eq!(s.service.state().get().granted_scopes, "Files.Read User.Read offline_access", "never more than asked");
}

/// `config.toml` recording another drive than the one the account's token reaches
/// — a hand edit — refuses `Dev1.ReadWriteAccessToken`, whose token is asked which drive it
/// reaches, turns the account read-only, and drops the next refresh to `Files.Read`; the
/// account info's own look at the drive keeps it so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_reaching_another_drive_than_the_recorded_one_is_never_read_write() {
    let s = signed_in(&["D1", "D3"]).await;
    let url = s.account.set_mode("read-write", false).await.unwrap();
    simulate_browser(&url, "code=rw-code").await;
    let account = &s.account;
    eventually("read-write", || async move { account.mode().await.unwrap() == "read-write" }).await;

    s.config()
        .update_account(&s.id, |a| {
            a.drive_id = "D3".into();
            Ok::<_, ConfigError>(())
        })
        .unwrap();
    let refused = s.dev.read_write_access_token().await.unwrap_err();
    assert_eq!(error_name(&refused), Some("org.konedrive.Error.WritesNotAllowed"), "{refused:?}");
    assert_eq!(s.account.mode().await.unwrap(), "read-only");
    assert!(s.account.last_error().await.unwrap().contains("reaches the OneDrive drive D1"));
    assert_eq!(s.service.tokens().access_token().await.unwrap(), "AT-RO", "the token that could write is not used");
    assert_eq!(refreshes(&s.server).await.last().unwrap(), "Files.Read User.Read offline_access");
    s.account.refresh_account_info().await.unwrap();
    wait_for_error(&s.account, "config.toml records drive D3").await;
    assert_eq!(s.account.mode().await.unwrap(), "read-only");
}

/// The drive taken off `write_test_drive_ids` by hand while the daemon runs counts
/// at once: the harness's token is refused, the account turns read-only at its next look, and
/// its next refresh asks for `Files.Read`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drive_taken_off_the_list_while_running_is_read_only_at_once() {
    let s = signed_in(&["D1"]).await;
    let url = s.account.set_mode("read-write", false).await.unwrap();
    simulate_browser(&url, "code=rw-code").await;
    let account = &s.account;
    eventually("read-write", || async move { account.mode().await.unwrap() == "read-write" }).await;

    let text = std::fs::read_to_string(s.config().file()).unwrap();
    assert!(text.contains("write_test_drive_ids = [\"D1\"]"), "{text}");
    std::fs::write(s.config().file(), text.replace("write_test_drive_ids = [\"D1\"]", "write_test_drive_ids = []")).unwrap();
    let refused = s.dev.read_write_access_token().await.unwrap_err();
    assert_eq!(error_name(&refused), Some("org.konedrive.Error.WritesNotAllowed"), "{refused:?}");
    s.account.refresh_account_info().await.unwrap();
    assert_eq!(wait_for_error(&s.account, "write_test_drive_ids").await, GATE_KEEPS_READ_ONLY);
    assert_eq!(s.account.mode().await.unwrap(), "read-only");
    assert_eq!(s.service.tokens().access_token().await.unwrap(), "AT-RO");
    assert_eq!(refreshes(&s.server).await.last().unwrap(), "Files.Read User.Read offline_access");
}

/// A sign-out forgets the account's name and email, but a read-write account signing
/// in again is still pinned to its own email, which `config.toml` keeps.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_write_account_signing_in_again_is_pinned_to_its_email() {
    let s = signed_in(&["D1"]).await;
    let url = s.account.set_mode("read-write", false).await.unwrap();
    simulate_browser(&url, "code=rw-code").await;
    let account = &s.account;
    eventually("read-write", || async move { account.mode().await.unwrap() == "read-write" }).await;
    s.account.sign_out().await.unwrap();
    assert_eq!(s.account.email().await.unwrap(), "", "forgotten with the sign-out");
    assert_eq!(s.config().account(&s.id).unwrap().login_hint, "test@outlook.com");

    let url = s.account.begin_sign_in().await.unwrap();
    let query: HashMap<String, String> = url::Url::parse(&url).unwrap().query_pairs().into_owned().collect();
    assert_eq!(query["scope"], READ_WRITE);
    assert_eq!((query["prompt"].as_str(), query["login_hint"].as_str()), ("login", "test@outlook.com"));
    s.account.cancel_sign_in().await.unwrap();
}

/// The mode and the list it is gated by come from one reading of `config.toml`,
/// taken again each time. A file that cannot be read fails closed — read-only, and
/// `LastError` says why — and a hand edit of the mode counts at the next look, as an edit of
/// the list does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mode_and_the_gate_are_read_together_and_fail_closed() {
    let s = signed_in(&["D1"]).await;
    let url = s.account.set_mode("read-write", false).await.unwrap();
    simulate_browser(&url, "code=rw-code").await;
    let account = &s.account;
    eventually("read-write", || async move { account.mode().await.unwrap() == "read-write" }).await;
    let text = std::fs::read_to_string(s.config().file()).unwrap();

    std::fs::write(s.config().file(), "config_version = 2\nthis is not [toml\n").unwrap();
    s.account.refresh_account_info().await.unwrap();
    assert_eq!(wait_for_error(&s.account, "cannot be read").await, CONFIG_UNREADABLE);
    assert_eq!(s.account.mode().await.unwrap(), "read-only", "fails closed");

    assert!(text.contains("mode = \"read-write\""), "{text}");
    std::fs::write(s.config().file(), text.replace("mode = \"read-write\"", "mode = \"read-only\"")).unwrap();
    assert_eq!(s.service.configured_mode(), Mode::ReadOnly, "the mode as the file says now");
    s.account.refresh_account_info().await.unwrap();
    eventually("the reason gone", || async move { !account.last_error().await.unwrap().contains("cannot be read") }).await;
    assert_eq!(s.account.mode().await.unwrap(), "read-only");
}

/// A read-write account that signs in again asks for `Files.ReadWrite`, pinned to
/// the account; landing on another Microsoft account stores nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_write_account_signing_in_as_someone_else_stores_nothing() {
    let s = signed_in(&["D1", "D9"]).await;
    let url = s.account.set_mode("read-write", false).await.unwrap();
    simulate_browser(&url, "code=rw-code").await;
    let account = &s.account;
    eventually("read-write", || async move { account.mode().await.unwrap() == "read-write" }).await;
    s.account.sign_out().await.unwrap();
    assert_eq!(s.account.mode().await.unwrap(), "read-only", "nothing granted once signed out");
    assert_eq!(s.configured(), Mode::ReadWrite, "config.toml keeps the choice");

    let url = s.account.begin_sign_in().await.unwrap();
    let query: HashMap<String, String> = url::Url::parse(&url).unwrap().query_pairs().into_owned().collect();
    assert_eq!((query["scope"].as_str(), query["prompt"].as_str()), (READ_WRITE, "login"));
    simulate_browser(&url, "code=other-code").await;
    eventually("signed out", || async move { account.state().await.unwrap() == "signed-out" }).await;
    assert!(s.account.last_error().await.unwrap().contains("different Microsoft account"));
    assert_eq!(s.refresh_token(), None, "nothing stored");
    assert_eq!(s.config().account(&s.id).unwrap().drive_id, "D1");
    assert_eq!(s.account.mode().await.unwrap(), "read-only");
}
