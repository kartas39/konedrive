//! The per-account mode (`docs/design/writes.md` §2), against a fake Microsoft (wiremock) and
//! over a private bus. Any signed-in account may be switched to read-write, whatever
//! `write_test_drive_ids` lists: that list limits `TokenExport.ReadWrite` alone. Read-only →
//! read-write is written only once the grant arrives; a cancelled, refused, narrower or foreign sign-in
//! changes nothing; read-write → read-only is a subset refresh; `TokenExport`'s token stays
//! read-only; a read-write account whose token cannot write runs read-only.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use konedrive_dbus::accounts::{AccountProxy, AccountsProxy, TokenExportProxy};
use konedrive_dbus::error_name;
use konedrive_dbus::testing::TestBus;
use konedrived::account::state::{ReadOnlyWish, SignInState};
use konedrived::account::{
    AccountService, FolderAccount, HeldUnsentChanges, PendingUploads, CONFIG_UNREADABLE, DRIVE_NOT_KNOWN, DRIVE_NOT_SEEN, SIGN_IN_TO_WRITE,
};
use konedrived::account::cache::AccountInfo;
use konedrived::config::{AccountId, ConfigError, ConfigStore, DriveId, Mode, Paths};
use konedrive_graph::secret::MemoryStore;
use konedrived::account::secret::Slot;
use konedrived::account::testing::{single_account, MemoryWallet};
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
    daemon: konedrived::daemon::startup::Daemon,
    account: AccountProxy<'static>,
    /// Served only by a development build (the `dev-tools` feature).
    #[cfg_attr(not(feature = "dev-tools"), allow(dead_code))]
    export: TokenExportProxy<'static>,
    service: Arc<AccountService>,
    id: AccountId,
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
/// AT1/RT1), with `allowed` as `write_test_drive_ids`: the drives `TokenExport.ReadWrite` is
/// handed out for, and nothing more.
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
    let allowed: Vec<DriveId> = allowed.iter().filter_map(|d| DriveId::new(*d)).collect();
    daemon
        .manager
        .config()
        .update(|config| {
            config.write_test_drive_ids = allowed;
            Ok::<_, ConfigError>(())
        })
        .unwrap();
    let client = bus.connect().await;
    let manager = AccountsProxy::new(&client).await.unwrap();
    manager.set_client_id(CLIENT_ID).await.unwrap();
    let path = daemon.manager.add("Test", &daemon.connection).await.unwrap().path.clone();
    let id = path.as_str().rsplit('/').next().unwrap().to_owned();
    let account = AccountProxy::builder(&client)
        .path(path.clone())
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();
    let export = TokenExportProxy::new(&client, path).await.unwrap();
    let url = account.begin_sign_in().await.unwrap();
    assert_eq!(simulate_browser(&url, &format!("code={code}")).await.status(), 200);
    let proxy = &account;
    eventually("signed in", || async move { proxy.state().await.unwrap() == "signed-in" }).await;
    let service = Arc::clone(&daemon.manager.accounts()[0].account);
    let config = Arc::clone(daemon.manager.config());
    let recorded = || config.account(service.id()).unwrap().drive_id == DriveId::new("D1");
    for _ in 0..500 {
        if recorded() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(recorded(), "the sign-in records the drive");
    eventually("the account's email", || async move { proxy.email().await.unwrap() == "test@outlook.com" }).await;
    assert_eq!(account.mode().await.unwrap(), "read-only");
    Setup { server, wallet, daemon, account, export, service, id: AccountId::new(id), _dir: dir, _bus: bus }
}

async fn wait_for_error(account: &AccountProxy<'static>, words: &str) -> String {
    eventually(words, || async move { account.last_error().await.unwrap().contains(words) }).await;
    account.last_error().await.unwrap()
}

/// The mode is the user's choice: with `write_test_drive_ids` empty — the account's drive is
/// not listed — `SetMode("read-write")` answers a sign-in URL, and the account is read-write
/// once that sign-in grants `Files.ReadWrite`. The list still refuses the harness's token,
/// before the switch and after it, and that refusal changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_account_whose_drive_is_not_listed_is_read_write_once_its_sign_in_grants_it() {
    let s = signed_in(&[]).await;
    assert!(s.config().snapshot().write_test_drive_ids.is_empty());
    #[cfg(feature = "dev-tools")]
    {
        let refused = s.export.read_write().await.unwrap_err();
        assert_eq!(error_name(&refused), Some("org.konedrive.Error.WritesNotAllowed"), "{refused:?}");
    }
    let url = s.account.set_mode("read-write", false).await.unwrap();
    let query: HashMap<String, String> = url::Url::parse(&url).unwrap().query_pairs().into_owned().collect();
    assert_eq!(query["scope"], READ_WRITE);
    assert_eq!((s.configured(), s.account.mode().await.unwrap().as_str()), (Mode::ReadOnly, "read-only"), "nothing before the grant");

    let page = simulate_browser(&url, "code=rw-code").await.text().await.unwrap();
    assert!(page.contains("can change files in OneDrive now.</p>"), "{page}");
    let account = &s.account;
    eventually("read-write", || async move { account.mode().await.unwrap() == "read-write" }).await;
    assert_eq!(s.configured(), Mode::ReadWrite);
    assert_eq!(s.refresh_token().as_deref(), Some("RT2"));
    assert_eq!(s.account.last_error().await.unwrap(), "");
    s.service.tokens().invalidate().await;
    assert_eq!(s.service.tokens().access_token().await.unwrap(), "AT-RW");
    assert_eq!(refreshes(&s.server).await.last().unwrap(), READ_WRITE, "every refresh asks for the mode's scope");

    #[cfg(feature = "dev-tools")]
    {
        let refused = s.export.read_write().await.unwrap_err();
        assert_eq!(error_name(&refused), Some("org.konedrive.Error.WritesNotAllowed"), "the list is about the token: {refused:?}");
        assert!(refused.to_string().contains("write_test_drive_ids"), "{refused:?}");
    }
    s.account.refresh_info().await.unwrap();
    assert_eq!(s.account.mode().await.unwrap(), "read-write", "the refused export changes nothing");
    assert_eq!(s.account.last_error().await.unwrap(), "");

    let refused = s.account.set_mode("read-write-please", false).await.unwrap_err();
    assert_eq!(error_name(&refused), Some("org.freedesktop.DBus.Error.InvalidArgs"), "{refused:?}");
}

/// Read-only → read-write: the sign-in asks for `Files.ReadWrite`, and nothing is written
/// until its token response grants it; then the refresh token is the new one, the mode is
/// written and published, and every refresh asks for `Files.ReadWrite`. `TokenExport.ReadOnly`
/// still hands out a `Files.Read` token, from a subset refresh; the harness's token can
/// write. Read-write → read-only needs no sign-in: the next refresh asks for `Files.Read`.
#[cfg(feature = "dev-tools")]
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

    let page = simulate_browser(&url, "code=rw-code").await.text().await.unwrap();
    assert!(page.contains("can change files in OneDrive now.</p>"), "{page}");
    let account = &s.account;
    eventually("read-write", || async move { account.mode().await.unwrap() == "read-write" }).await;
    assert_eq!(s.configured(), Mode::ReadWrite);
    assert_eq!(s.refresh_token().as_deref(), Some("RT2"));
    assert_eq!(s.account.state().await.unwrap(), "signed-in");
    assert_eq!(s.account.last_error().await.unwrap(), "");
    let cache = Paths::in_dir(s._dir.path()).account(&s.id).unwrap().account_cache;
    assert_eq!(konedrived::account::cache::load(&cache).unwrap().granted_scopes, READ_WRITE, "kept with the account");

    assert_eq!(s.export.read_only().await.unwrap(), "AT-RO", "TokenExport's token stays read-only");
    assert_eq!(refreshes(&s.server).await, vec!["Files.Read User.Read offline_access"]);
    assert_eq!(s.export.read_write().await.unwrap(), "AT2");
    s.service.tokens().invalidate().await;
    assert_eq!(s.service.tokens().access_token().await.unwrap(), "AT-RW");
    assert_eq!(refreshes(&s.server).await.last().unwrap(), READ_WRITE, "every refresh asks for the mode's scope");
    assert_eq!(s.account.set_mode("read-write", false).await.unwrap(), "", "read-write already: no sign-in");

    assert_eq!(s.account.set_mode("read-only", false).await.unwrap(), "", "no sign-in");
    eventually("read-only", || async move { account.mode().await.unwrap() == "read-only" }).await;
    assert_eq!(s.configured(), Mode::ReadOnly);
    assert_eq!(s.service.tokens().access_token().await.unwrap(), "AT-RO", "the token that could write is gone");
    assert_eq!(refreshes(&s.server).await.last().unwrap(), "Files.Read User.Read offline_access");
    let refused = s.export.read_write().await.unwrap_err();
    assert_eq!(error_name(&refused), Some("org.konedrive.Error.ModeNotGranted"), "{refused:?}");
    assert_eq!(s.account.state().await.unwrap(), "signed-in");
}

/// A switch to read-write that is cancelled, refused in the browser, granted without
/// `Files.ReadWrite`, or signed in as another Microsoft account changes nothing: the account
/// stays signed in, read-only, with its refresh token; `LastError` says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_that_is_not_granted_changes_nothing() {
    let s = signed_in(&[]).await;
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
    let page = simulate_browser(&url, "code=other-code").await.text().await.unwrap();
    assert!(page.contains("the browser signed in as a different Microsoft account; the account stays read-only.</p>"), "{page}");
    wait_for_error(&s.account, "different Microsoft account").await;
    unchanged(&s);

    assert_eq!(s.account.mode().await.unwrap(), "read-only");
    assert_eq!(s.account.state().await.unwrap(), "signed-in");
    assert_eq!(s.config().account(&s.id).unwrap().drive_id, DriveId::new("D1"));
}

/// An account with no drive recorded is asked which drive it is before any sign-in begins:
/// learned, the drive is recorded and the switch goes on to its sign-in URL; not learnable,
/// the switch is refused at once — no URL, no trip through the browser — in words that say
/// the drive is not known yet, and nothing changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_of_an_account_with_no_recorded_drive_learns_it_or_is_refused_before_a_url() {
    let s = signed_in(&[]).await;
    let forget_drive = |s: &Setup| {
        s.config()
            .update(|config| {
                config.account_mut(&s.id).unwrap().drive_id = None;
                Ok::<_, ConfigError>(())
            })
            .unwrap();
        assert_eq!(s.config().account(&s.id).unwrap().drive_id, None);
    };

    forget_drive(&s);
    let url = s.account.set_mode("read-write", false).await.unwrap();
    assert!(url.contains("scope="), "the sign-in begins: {url}");
    assert_eq!(s.config().account(&s.id).unwrap().drive_id, DriveId::new("D1"), "asked and recorded first");
    s.account.cancel_sign_in().await.unwrap();

    forget_drive(&s);
    // Graph answers nothing any more: the drive cannot be learned.
    s.server.reset().await;
    let config_before = std::fs::read_to_string(s.config().file()).unwrap();
    let refused = s.account.set_mode("read-write", false).await.unwrap_err();
    assert_eq!(error_name(&refused), Some("org.konedrive.Error.Failed"), "{refused:?}");
    assert!(refused.to_string().contains(DRIVE_NOT_KNOWN), "{refused:?}");
    assert!(!refused.to_string().contains("different Microsoft account"), "{refused:?}");
    assert_eq!(std::fs::read_to_string(s.config().file()).unwrap(), config_before);
    assert_eq!((s.configured(), s.account.mode().await.unwrap().as_str()), (Mode::ReadOnly, "read-only"));
    assert_eq!(s.account.state().await.unwrap(), "signed-in");
    assert_eq!(s.refresh_token().as_deref(), Some("RT1"));
}

/// An account's folder as the switch between the modes asks it (`PendingUploads`), faked
/// here: how many changes are unsent, whether a request that changes OneDrive is in
/// flight, and whether it was told to hold what is unsent. It drops nothing: what is
/// unsent stays unsent when it is held.
#[derive(Default)]
struct UnsentFolder {
    unsent: AtomicU64,
    is_request_in_flight: AtomicBool,
    is_held: AtomicBool,
    /// How many times it was asked again whether a request is in flight.
    asked_again: AtomicU64,
}

impl UnsentFolder {
    fn with_unsent(unsent: u64) -> Arc<Self> {
        Arc::new(Self { unsent: AtomicU64::new(unsent), ..Self::default() })
    }
}

#[async_trait::async_trait]
impl PendingUploads for UnsentFolder {
    async fn pending_uploads(&self) -> u64 {
        self.unsent.load(Ordering::SeqCst)
    }

    async fn hold_unsent_changes(&self) -> HeldUnsentChanges {
        self.is_held.store(true, Ordering::SeqCst);
        HeldUnsentChanges { unsent: self.unsent.load(Ordering::SeqCst), is_request_in_flight: self.is_request_in_flight.load(Ordering::SeqCst) }
    }

    fn has_request_in_flight(&self) -> bool {
        self.asked_again.fetch_add(1, Ordering::SeqCst);
        self.is_request_in_flight.load(Ordering::SeqCst)
    }
}

/// An account that runs read-write with no sign-in made: `config.toml` says read-write and
/// records drive D1, and its state says signed in, with a token that can write and was seen
/// to reach D1. Its folder is `folder`.
async fn read_write_account(folder: &Arc<UnsentFolder>) -> Fixture {
    let f = Fixture::new(Duration::from_secs(5)).await;
    f.svc.config().record_drive(f.svc.id(), &DriveId::new("D1").unwrap()).unwrap();
    f.svc
        .config()
        .update_account(f.svc.id(), |a| {
            a.mode = Mode::ReadWrite;
            Ok::<_, ConfigError>(())
        })
        .unwrap();
    f.svc.state().update(|s| {
        s.state = SignInState::SignedIn;
        s.granted_scopes = READ_WRITE.to_owned();
        s.live_drive = "D1".to_owned();
    });
    f.svc.recheck_mode();
    assert_eq!((f.svc.mode(), f.svc.state().get().mode_note), (Mode::ReadWrite, None), "the account runs read-write");
    let uploads: std::sync::Weak<UnsentFolder> = Arc::downgrade(folder);
    f.svc.set_uploads(uploads);
    f
}

/// Whether `config.toml` keeps the account's wish for read-only.
fn keeps_wish(f: &Fixture) -> bool {
    f.svc.config().account(f.svc.id()).unwrap().read_only_wanted
}

/// Read-write → read-only asked while changes are unsent, not forced, is neither made nor
/// refused: the wish is kept, in `config.toml` too, the account stays read-write and says
/// "switching" with the count the folder last told; when the folder tells that nothing is
/// unsent the switch is made by itself, the folder holds, and the wish and the note are gone.
#[tokio::test]
async fn a_switch_to_read_only_asked_with_changes_unsent_keeps_the_wish_and_is_made_when_none_is_left() {
    let folder = UnsentFolder::with_unsent(3);
    let f = read_write_account(&folder).await;
    assert_eq!(f.svc.set_mode("read-only", false).await.unwrap(), "", "not refused");
    assert_eq!((f.svc.configured_mode(), f.svc.mode()), (Mode::ReadWrite, Mode::ReadWrite), "the account is read-write still");
    assert!(keeps_wish(&f), "the wish is in config.toml");
    assert!(!folder.is_held.load(Ordering::SeqCst), "the folder goes on sending");
    assert_eq!((f.svc.state().get().read_only_wish, f.svc.state().get().published_error()), (Some(ReadOnlyWish { unsent: 3 }), String::new()), "a state, and no error");

    // The folder's plan counts what is left.
    folder.unsent.store(1, Ordering::SeqCst);
    f.svc.tell_unsent_count(1);
    assert_eq!(f.svc.state().get().read_only_wish, Some(ReadOnlyWish { unsent: 1 }));
    assert_eq!(f.svc.configured_mode(), Mode::ReadWrite, "not yet");

    folder.unsent.store(0, Ordering::SeqCst);
    f.svc.tell_unsent_count(0);
    let svc = &f.svc;
    eventually("the switch made by itself", || async move { svc.configured_mode() == Mode::ReadOnly && svc.state().get().read_only_wish.is_none() }).await;
    assert!(folder.is_held.load(Ordering::SeqCst), "the folder was told to hold");
    assert!(!keeps_wish(&f), "the wish went with the switch");
    assert_eq!(f.svc.mode(), Mode::ReadOnly);
}

/// On the bus a wish for read-only that waits is a state of its own: `ReadOnlyWanted` with
/// `UnsentBeforeReadOnly`, the count as the folder last told it, and `LastError` says
/// nothing of it; taken back, both are as with no wish.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bus_says_a_wish_for_read_only_with_its_count_and_not_as_an_error() {
    let s = signed_in(&[]).await;
    let folder = UnsentFolder::with_unsent(2);
    s.config().record_drive(&s.id, &DriveId::new("D1").unwrap()).unwrap();
    s.config()
        .update_account(&s.id, |a| {
            a.mode = Mode::ReadWrite;
            Ok::<_, ConfigError>(())
        })
        .unwrap();
    s.service.state().update(|state| {
        state.granted_scopes = READ_WRITE.to_owned();
        state.live_drive = "D1".to_owned();
    });
    s.service.recheck_mode();
    let uploads: std::sync::Weak<UnsentFolder> = Arc::downgrade(&folder);
    s.service.set_uploads(uploads);
    assert_eq!(s.account.mode().await.unwrap(), "read-write");
    assert_eq!((s.account.read_only_wanted().await.unwrap(), s.account.unsent_before_read_only().await.unwrap()), (false, 0));

    assert_eq!(s.account.set_mode("read-only", false).await.unwrap(), "");
    assert_eq!((s.account.read_only_wanted().await.unwrap(), s.account.unsent_before_read_only().await.unwrap()), (true, 2));
    assert_eq!((s.account.mode().await.unwrap().as_str(), s.account.last_error().await.unwrap().as_str()), ("read-write", ""));

    s.service.tell_unsent_count(1);
    assert_eq!((s.account.read_only_wanted().await.unwrap(), s.account.unsent_before_read_only().await.unwrap()), (true, 1));

    assert_eq!(s.account.set_mode("read-write", false).await.unwrap(), "", "the wish is taken back with no sign-in");
    assert_eq!((s.account.read_only_wanted().await.unwrap(), s.account.unsent_before_read_only().await.unwrap()), (false, 0));
    assert_eq!(s.account.mode().await.unwrap(), "read-write");
}

/// A wish for read-only is taken back by a switch to read-write: no sign-in, the account
/// is read-write as it was, the note is gone, and a folder that then says nothing is
/// unsent switches nothing.
#[tokio::test]
async fn a_wish_for_read_only_is_taken_back_by_a_switch_to_read_write() {
    let folder = UnsentFolder::with_unsent(2);
    let f = read_write_account(&folder).await;
    f.svc.set_mode("read-only", false).await.unwrap();
    assert!(keeps_wish(&f));
    assert_eq!(f.svc.set_mode("read-write", false).await.unwrap(), "", "no sign-in is needed");
    assert!(!keeps_wish(&f));
    assert_eq!((f.svc.configured_mode(), f.svc.mode(), f.svc.state().get().read_only_wish), (Mode::ReadWrite, Mode::ReadWrite, None));

    folder.unsent.store(0, Ordering::SeqCst);
    f.svc.tell_unsent_count(0);
    // Whatever that call started has run by now: nothing was switched.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(f.svc.configured_mode(), Mode::ReadWrite);
    assert!(!folder.is_held.load(Ordering::SeqCst));
}

/// A forced switch to read-only goes through at once with changes unsent: `config.toml`
/// says read-only, no wish is kept, the folder holds what is unsent, and nothing of it is
/// dropped.
#[tokio::test]
async fn a_forced_switch_to_read_only_is_made_at_once_and_what_is_unsent_stays_held() {
    let folder = UnsentFolder::with_unsent(3);
    let f = read_write_account(&folder).await;
    assert_eq!(f.svc.set_mode("read-only", true).await.unwrap(), "");
    assert_eq!((f.svc.configured_mode(), f.svc.mode()), (Mode::ReadOnly, Mode::ReadOnly));
    assert!(!keeps_wish(&f));
    assert!(folder.is_held.load(Ordering::SeqCst), "the folder holds what is unsent");
    assert_eq!(folder.pending_uploads().await, 3, "nothing is dropped");
    assert_eq!((f.svc.state().get().read_only_wish, f.svc.state().get().mode_note), (None, None));
}

/// A wish that is kept is not asked a second time: asked again with force the switch is
/// made at once, and the wish is forgotten with it.
#[tokio::test]
async fn a_kept_wish_asked_again_with_force_switches_at_once() {
    let folder = UnsentFolder::with_unsent(3);
    let f = read_write_account(&folder).await;
    f.svc.set_mode("read-only", false).await.unwrap();
    assert!(keeps_wish(&f));
    f.svc.set_mode("read-only", true).await.unwrap();
    assert_eq!(f.svc.configured_mode(), Mode::ReadOnly);
    assert!(!keeps_wish(&f));
    assert!(folder.is_held.load(Ordering::SeqCst));
    assert_eq!(folder.pending_uploads().await, 3, "nothing is dropped");
}

/// Read-only already, with changes that were unsent at the switch: a switch to read-only
/// asked again, forced or not, tells the folder to hold again, keeps no wish and drops
/// nothing.
#[tokio::test]
async fn a_switch_to_read_only_of_a_read_only_account_holds_again_and_drops_nothing() {
    for force in [false, true] {
        let f = Fixture::new(Duration::from_secs(5)).await;
        let folder = UnsentFolder::with_unsent(2);
        let uploads: std::sync::Weak<UnsentFolder> = Arc::downgrade(&folder);
        f.svc.set_uploads(uploads);
        assert_eq!(f.svc.configured_mode(), Mode::ReadOnly);
        assert_eq!(f.svc.set_mode("read-only", force).await.unwrap(), "");
        assert!(folder.is_held.load(Ordering::SeqCst), "force {force}: the folder is told again");
        assert!(!keeps_wish(&f), "force {force}");
        assert_eq!(folder.pending_uploads().await, 2, "force {force}: nothing is dropped");
        assert_eq!(f.svc.configured_mode(), Mode::ReadOnly);
    }
}

/// The token that can write is not given up while a request that changes OneDrive is in
/// flight: the switch is made (`config.toml`, the folder held), the account is read-only
/// and says nothing of it — no wish waits — and the folder is asked again after a short
/// wait until it says none is; then it is asked no more.
#[tokio::test]
async fn the_switch_to_read_only_asks_the_folder_again_while_a_request_is_in_flight() {
    let folder = UnsentFolder::with_unsent(2);
    folder.is_request_in_flight.store(true, Ordering::SeqCst);
    let f = read_write_account(&folder).await;
    f.svc.set_mode("read-only", true).await.unwrap();
    assert_eq!(f.svc.configured_mode(), Mode::ReadOnly);
    assert!(folder.is_held.load(Ordering::SeqCst));
    assert_eq!((f.svc.state().get().read_only_wish, f.svc.state().get().mode_note), (None, None), "the account is read-only already: no wish waits, and nothing is said");

    // Asked again while the request still hangs: nothing changes. The wait has started first.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(6)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let asked = folder.asked_again.load(Ordering::SeqCst);
    assert!(asked >= 1, "still in flight: the folder is asked again");
    assert_eq!((f.svc.state().get().read_only_wish, f.svc.state().get().mode_note), (None, None));

    // The request ended: at the next ask the token goes, and the folder is asked no more.
    folder.is_request_in_flight.store(false, Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(6)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    let asked = folder.asked_again.load(Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(12)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert_eq!(folder.asked_again.load(Ordering::SeqCst), asked, "the wait has ended");
    assert_eq!((f.svc.state().get().read_only_wish, f.svc.state().get().mode_note), (None, None));
    assert_eq!((f.svc.configured_mode(), f.svc.mode()), (Mode::ReadOnly, Mode::ReadOnly));
}

/// The mode an account runs in at start: read-write only when `config.toml` says so — a hand
/// edit counts — the scopes its last token was granted, kept in `account.json`, carry
/// `Files.ReadWrite`, and that token was seen to reach the recorded drive; whether
/// `write_test_drive_ids` lists the drive (`listed`) changes nothing. Otherwise read-only,
/// `LastError` says why, and the refresh asks for `Files.Read`, never for more than was
/// granted.
#[tokio::test]
async fn a_read_write_account_runs_read_write_only_with_the_grant_for_its_own_drive() {
    const READ: &str = "Files.Read User.Read offline_access";
    // (granted, drive the token was seen to reach, listed, mode, LastError, next refresh asks)
    let cases = [
        (READ_WRITE, "D1", true, Mode::ReadWrite, "", READ_WRITE),
        (READ, "D1", true, Mode::ReadOnly, SIGN_IN_TO_WRITE, READ),
        ("", "", true, Mode::ReadOnly, SIGN_IN_TO_WRITE, READ),
        (READ_WRITE, "D1", false, Mode::ReadWrite, "", READ_WRITE),
        (READ, "D1", false, Mode::ReadOnly, SIGN_IN_TO_WRITE, READ),
        (READ_WRITE, "", true, Mode::ReadOnly, DRIVE_NOT_SEEN, READ),
        (READ_WRITE, "D2", true, Mode::ReadOnly, "reaches the OneDrive drive D2", READ),
    ];
    for (granted, seen, listed, mode, error, asked) in cases {
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
        config.record_drive(&id, &DriveId::new("D1").unwrap()).unwrap();
        config
            .update(|c| {
                c.account_mut(&id).unwrap().mode = Mode::ReadWrite;
                c.write_test_drive_ids = DriveId::new("D1").into_iter().filter(|_| listed).collect();
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
            konedrived::account::cache::save(&paths.account(&id).unwrap().account_cache, &info).unwrap();
        }
        let svc = single_account(dir.path(), endpoints(&server), Arc::new(MemoryStore::with_token("RT0")), Duration::from_secs(5))
            .await
            .unwrap();
        svc.startup().await;
        let case = format!("granted {granted:?}, seen {seen:?}, listed {listed}");
        assert_eq!(svc.mode(), mode, "{case}");
        let last_error = svc.state().get().published_error();
        assert!(if error.is_empty() { last_error.is_empty() } else { last_error.contains(error) }, "{case}: {last_error}");
        svc.tokens().invalidate().await;
        svc.tokens().access_token().await.unwrap();
        assert_eq!(refreshes(&server).await.last().unwrap(), asked, "{case}");
    }
}

/// A read-only request that Microsoft answers with a token that can write —
/// consent it still holds — for an account `config.toml` sets to read-write by hand, whose
/// drive `write_test_drive_ids` does not list. The account stays read-only and says so:
/// what was asked for was read-only. Neither `TokenExport` token is handed out; the wide
/// token is used to read only.
#[cfg(feature = "dev-tools")]
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
    s.account.refresh_info().await.unwrap();
    wait_for_error(&s.account, "account.live.com/consent/Manage").await;
    assert_eq!(s.account.mode().await.unwrap(), "read-only");
    let refused = s.export.read_write().await.unwrap_err();
    assert_eq!(error_name(&refused), Some("org.konedrive.Error.WritesNotAllowed"), "{refused:?}");
    let refused = s.export.read_only().await.unwrap_err();
    assert_eq!(error_name(&refused), Some("org.konedrive.Error.Failed"), "never a token that can write: {refused:?}");
    s.service.tokens().invalidate().await;
    assert_eq!(s.service.tokens().access_token().await.unwrap(), "AT-WIDE", "used to read");
    assert_eq!(refreshes(&s.server).await.last().unwrap(), "Files.Read User.Read offline_access");
    assert_eq!(s.account.mode().await.unwrap(), "read-only");
    assert_eq!(s.service.state().get().granted_scopes, "Files.Read User.Read offline_access", "never more than asked");
}

/// `config.toml` recording another drive than the one the account's token reaches
/// — a hand edit — refuses `TokenExport.ReadWrite`, whose token is asked which drive it
/// reaches, turns the account read-only, and drops the next refresh to `Files.Read`; the
/// account info's own look at the drive keeps it so.
#[cfg(feature = "dev-tools")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_reaching_another_drive_than_the_recorded_one_is_never_read_write() {
    let s = signed_in(&["D1", "D3"]).await;
    let url = s.account.set_mode("read-write", false).await.unwrap();
    simulate_browser(&url, "code=rw-code").await;
    let account = &s.account;
    eventually("read-write", || async move { account.mode().await.unwrap() == "read-write" }).await;

    s.config()
        .update_account(&s.id, |a| {
            a.drive_id = DriveId::new("D3");
            Ok::<_, ConfigError>(())
        })
        .unwrap();
    let refused = s.export.read_write().await.unwrap_err();
    assert_eq!(error_name(&refused), Some("org.konedrive.Error.WritesNotAllowed"), "{refused:?}");
    assert_eq!(s.account.mode().await.unwrap(), "read-only");
    assert!(s.account.last_error().await.unwrap().contains("reaches the OneDrive drive D1"));
    assert_eq!(s.service.tokens().access_token().await.unwrap(), "AT-RO", "the token that could write is not used");
    assert_eq!(refreshes(&s.server).await.last().unwrap(), "Files.Read User.Read offline_access");
    s.account.refresh_info().await.unwrap();
    // The recorded drive is named as a drive id is made: its letters in lower case.
    wait_for_error(&s.account, "config.toml records drive d3").await;
    assert_eq!(s.account.mode().await.unwrap(), "read-only");
}

/// The drive taken off `write_test_drive_ids` by hand while the daemon runs counts
/// at once, for the one thing the list decides: the harness's token is refused. The account
/// stays read-write, and its refreshes keep asking for `Files.ReadWrite`.
#[cfg(feature = "dev-tools")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_drive_taken_off_the_list_while_running_loses_the_token_export_only() {
    let s = signed_in(&["D1"]).await;
    let url = s.account.set_mode("read-write", false).await.unwrap();
    simulate_browser(&url, "code=rw-code").await;
    let account = &s.account;
    eventually("read-write", || async move { account.mode().await.unwrap() == "read-write" }).await;
    assert_eq!(s.export.read_write().await.unwrap(), "AT2", "handed out for a listed drive");

    let text = std::fs::read_to_string(s.config().file()).unwrap();
    // The file has the drive's id as a drive id is made: its letters in lower case.
    assert!(text.contains("write_test_drive_ids = [\"d1\"]"), "{text}");
    std::fs::write(s.config().file(), text.replace("write_test_drive_ids = [\"d1\"]", "write_test_drive_ids = []")).unwrap();
    let refused = s.export.read_write().await.unwrap_err();
    assert_eq!(error_name(&refused), Some("org.konedrive.Error.WritesNotAllowed"), "{refused:?}");
    s.account.refresh_info().await.unwrap();
    assert_eq!(s.account.mode().await.unwrap(), "read-write");
    assert_eq!(s.account.last_error().await.unwrap(), "");
    s.service.tokens().invalidate().await;
    assert_eq!(s.service.tokens().access_token().await.unwrap(), "AT-RW");
    assert_eq!(refreshes(&s.server).await.last().unwrap(), READ_WRITE);
}

/// A sign-out forgets the account's name and email, but a read-write account signing
/// in again is still pinned to its own email, which `config.toml` keeps.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_write_account_signing_in_again_is_pinned_to_its_email() {
    let s = signed_in(&[]).await;
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

/// The mode and the recorded drive come from one reading of `config.toml`,
/// taken again each time. A file that cannot be read fails closed — read-only, and
/// `LastError` says why — and a hand edit of the mode counts at the next look.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mode_and_the_drive_are_read_together_and_fail_closed() {
    let s = signed_in(&[]).await;
    let url = s.account.set_mode("read-write", false).await.unwrap();
    simulate_browser(&url, "code=rw-code").await;
    let account = &s.account;
    eventually("read-write", || async move { account.mode().await.unwrap() == "read-write" }).await;
    let text = std::fs::read_to_string(s.config().file()).unwrap();

    std::fs::write(s.config().file(), "config_version = 2\nthis is not [toml\n").unwrap();
    s.account.refresh_info().await.unwrap();
    assert_eq!(wait_for_error(&s.account, "cannot be read").await, CONFIG_UNREADABLE);
    assert_eq!(s.account.mode().await.unwrap(), "read-only", "fails closed");

    assert!(text.contains("mode = \"read-write\""), "{text}");
    std::fs::write(s.config().file(), text.replace("mode = \"read-write\"", "mode = \"read-only\"")).unwrap();
    assert_eq!(s.service.configured_mode(), Mode::ReadOnly, "the mode as the file says now");
    s.account.refresh_info().await.unwrap();
    eventually("the reason gone", || async move { !account.last_error().await.unwrap().contains("cannot be read") }).await;
    assert_eq!(s.account.mode().await.unwrap(), "read-only");
}

/// A read-write account that signs in again asks for `Files.ReadWrite`, pinned to
/// the account; landing on another Microsoft account stores nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_write_account_signing_in_as_someone_else_stores_nothing() {
    let s = signed_in(&[]).await;
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
    assert_eq!(s.config().account(&s.id).unwrap().drive_id, DriveId::new("D1"));
    assert_eq!(s.account.mode().await.unwrap(), "read-only");
}
