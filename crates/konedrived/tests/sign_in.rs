//! `Accounts.SignIn` over a private test bus: a new account is a draft nobody lists until its
//! sign-in succeeds, named by its email then, and nothing is left of one that did not.
//! Microsoft is wiremock; the wallet is in memory.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use futures_util::StreamExt;
use konedrive_dbus::accounts::{AccountProxy, AccountsProxy, SignInFinishedStream};
use konedrive_dbus::sign_in::{ALREADY_ADDED, CANCELLED, FAILED, SIGNED_IN};
use konedrive_dbus::testing::TestBus;
use konedrive_dbus::{error_name, ACCOUNTS_PATH};
use konedrived::account::secret::{Slot, Wallet};
use konedrived::account::testing::MemoryWallet;
use konedrived::config::{ConfigStore, Paths};
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zbus::zvariant::OwnedObjectPath;

/// The daemon with no account, a client of it that hears every `SignInFinished`, and
/// Microsoft played by wiremock: the code `good-code` signs `test@outlook.com` in, drive `D1`.
struct Setup {
    daemon: konedrived::daemon::startup::Daemon,
    client: zbus::Connection,
    manager: AccountsProxy<'static>,
    finished: SignInFinishedStream,
    wallet: Arc<MemoryWallet>,
    server: MockServer,
    dir: tempfile::TempDir,
    _bus: TestBus,
}

async fn setup(sign_in_timeout: Duration) -> Setup {
    let bus = TestBus::start();
    let server = MockServer::start().await;
    mock_microsoft(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let wallet = Arc::new(MemoryWallet::default());
    let daemon = start_daemon(&bus, dir.path(), endpoints(&server), wallet.clone(), sign_in_timeout).await;
    let client = bus.connect().await;
    let manager = AccountsProxy::builder(&client).cache_properties(zbus::proxy::CacheProperties::No).build().await.unwrap();
    let finished = manager.receive_sign_in_finished().await.unwrap();
    Setup { daemon, client, manager, finished, wallet, server, dir, _bus: bus }
}

/// Another Microsoft account: the code `code` signs it in, with `email` (or none) and `drive`.
async fn mock_identity(server: &MockServer, code: &str, token: &str, email: Option<&str>, drive: &str) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains(format!("code={code}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"token_type": "Bearer", "access_token": token, "expires_in": 3600, "refresh_token": format!("R-{token}")}),
        ))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .and(header("authorization", format!("Bearer {token}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"displayName": "Somebody", "mail": email})))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/me/drive"))
        .and(header("authorization", format!("Bearer {token}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": drive, "quota": {"used": 1u64, "total": 2u64}})))
        .mount(server)
        .await;
}

impl Setup {
    /// The next `SignInFinished`: the draft, the outcome and the message.
    async fn next_finished(&mut self) -> (OwnedObjectPath, String, String) {
        let signal = tokio::time::timeout(Duration::from_secs(20), self.finished.next())
            .await
            .expect("no SignInFinished came")
            .expect("the signal stream ended");
        let said = signal.args().unwrap();
        (said.account.clone(), said.outcome.clone(), said.message.clone())
    }

    /// No further `SignInFinished` comes.
    async fn no_more_finished(&mut self) {
        let more = tokio::time::timeout(Duration::from_millis(300), self.finished.next()).await;
        assert!(more.is_err(), "a second SignInFinished: {:?}", more.unwrap().map(|s| format!("{:?}", s.args())));
    }

    async fn account(&self, path: &OwnedObjectPath) -> AccountProxy<'static> {
        AccountProxy::builder(&self.client)
            .path(path.clone())
            .unwrap()
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .unwrap()
    }

    fn config_text(&self) -> String {
        std::fs::read_to_string(Paths::in_dir(self.dir.path()).config_file).unwrap_or_default()
    }

    /// A `SignIn` played to its end in the browser with `answer`; the draft and how it ended.
    async fn sign_in_with(&mut self, answer: &str) -> (OwnedObjectPath, String, String) {
        let (draft, url) = self.manager.sign_in().await.unwrap();
        assert_eq!(simulate_browser(&url, answer).await.status(), 200);
        let (ended, outcome, message) = self.next_finished().await;
        assert_eq!(ended, draft);
        (draft, outcome, message)
    }

    /// Nothing of the draft `draft` is left, and the daemon has the `accounts` accounts it had.
    async fn nothing_is_left_of(&self, draft: &OwnedObjectPath, accounts: usize) {
        let id = draft.as_str().rsplit('/').next().unwrap();
        assert_eq!(self.manager.list().await.unwrap().len(), accounts, "List is as it was");
        let config = self.config_text();
        assert!(!config.contains(id) && !config.contains("draft"), "config.toml keeps nothing of it: {config}");
        assert_eq!(config.matches("[[accounts]]").count(), accounts, "{config}");
        assert!(!account_dir(self.dir.path(), id).exists(), "its directory is gone");
        assert_eq!(self.wallet.current(&Slot::Account(konedrived::config::AccountId::new(id))), None, "no token is stored");
        let objects = introspect(&self.client, ACCOUNTS_PATH).await;
        assert!(!objects.contains(id), "no object of it is left on the bus: {objects}");
        assert_eq!(self.daemon.manager.registry().accounts().len(), accounts, "nor is its folder one of the daemon's");
    }
}

fn account_dir(dir: &Path, id: &str) -> std::path::PathBuf {
    Paths::in_dir(dir).state_dir.join("accounts").join(id)
}

/// A `SignIn` that succeeds: a draft on the bus that `List` never shows, then the account in
/// `List` under its email, and one `signed-in`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sign_in_that_succeeds_lists_the_account_under_its_email() {
    let mut s = setup(Duration::from_secs(10)).await;
    let (draft, url) = s.manager.sign_in().await.unwrap();
    let id = draft.as_str().rsplit('/').next().unwrap().to_owned();

    // The draft: on the bus, signing in, marked in config.toml, in no list.
    let account = s.account(&draft).await;
    assert_eq!(account.state().await.unwrap(), "signing-in");
    assert_eq!(account.label().await.unwrap(), id, "a draft's label is its id");
    assert!(s.manager.list().await.unwrap().is_empty(), "List does not show a draft");
    assert!(s.config_text().contains("draft = true"), "{}", s.config_text());
    assert!(s.daemon.manager.accounts().is_empty());
    // A draft takes no folder.
    let folder = konedrive_dbus::accounts::FolderProxy::new(&s.client, draft.clone()).await.unwrap();
    assert!(folder.register_without_interception(s.dir.path().join("f").to_str().unwrap()).await.is_err());

    assert_eq!(simulate_browser(&url, "code=good-code").await.status(), 200);
    let (ended, outcome, message) = s.next_finished().await;
    assert_eq!((ended, outcome.as_str(), message.as_str()), (draft.clone(), SIGNED_IN, "test@outlook.com"));
    // Listed, under its final label, by the time the signal is read.
    assert_eq!(s.manager.list().await.unwrap(), [draft.clone()]);
    assert_eq!(account.label().await.unwrap(), "test@outlook.com");
    assert_eq!(account.state().await.unwrap(), "signed-in");
    let config = s.config_text();
    assert!(config.contains("label = \"test@outlook.com\"") && !config.contains("draft"), "{config}");
    assert_eq!(s.wallet.current(&Slot::Account(konedrived::config::AccountId::new(id))).as_deref(), Some("RT1"));
    s.no_more_finished().await;

    // It is an account like any other from now on: it takes a folder, and is removed.
    let root = s.dir.path().join("folder");
    std::fs::create_dir(&root).unwrap();
    folder.register_without_interception(root.to_str().unwrap()).await.unwrap();
    s.manager.remove(&draft.as_ref()).await.unwrap();
    assert!(s.manager.list().await.unwrap().is_empty());
    s.no_more_finished().await;
}

/// A sign-in the browser refuses and one that is cancelled each leave nothing, and say how
/// they ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_and_a_cancelled_sign_in_leave_nothing() {
    let mut s = setup(Duration::from_secs(10)).await;

    let (draft, outcome, message) = s.sign_in_with("error=access_denied").await;
    assert_eq!((outcome.as_str(), message.as_str()), (FAILED, "Access was denied in the browser."));
    s.nothing_is_left_of(&draft, 0).await;

    let (draft, _url) = s.manager.sign_in().await.unwrap();
    s.account(&draft).await.cancel_sign_in().await.unwrap();
    let (ended, outcome, message) = s.next_finished().await;
    assert_eq!((ended, outcome.as_str(), message.as_str()), (draft.clone(), CANCELLED, ""));
    s.nothing_is_left_of(&draft, 0).await;
    s.no_more_finished().await;
}

/// A sign-in nobody finishes ends at the timeout as `failed`, and leaves nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sign_in_that_times_out_leaves_nothing() {
    let mut s = setup(Duration::from_millis(300)).await;
    let (draft, _url) = s.manager.sign_in().await.unwrap();
    let (ended, outcome, message) = s.next_finished().await;
    assert_eq!((ended, outcome.as_str()), (draft.clone(), FAILED));
    assert!(message.starts_with("Timed out waiting for the browser"), "{message}");
    s.nothing_is_left_of(&draft, 0).await;
    s.no_more_finished().await;
}

/// A drive another account has is `already-added`, with that account's label; the label
/// of an account whose email is another account's label, and of one with no email.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_drive_decides_whether_an_account_is_added_and_the_email_names_it() {
    let mut s = setup(Duration::from_secs(10)).await;
    let (first, outcome, _) = s.sign_in_with("code=good-code").await;
    assert_eq!(outcome, SIGNED_IN);

    // The same Microsoft account again.
    let (draft, outcome, message) = s.sign_in_with("code=good-code").await;
    assert_eq!((outcome.as_str(), message.as_str()), (ALREADY_ADDED, "test@outlook.com"));
    s.nothing_is_left_of(&draft, 1).await;
    assert_eq!(s.manager.list().await.unwrap(), [first.clone()]);

    // Another account whose email is, whatever the case, the label of an account that is
    // not that drive: the name decides nothing, and the label is the next free one.
    s.daemon.manager.add("ANN@live.com", &s.daemon.connection).await.unwrap();
    mock_identity(&s.server, "ann-code", "AT2", Some("ann@live.com"), "D2").await;
    let (second, outcome, message) = s.sign_in_with("code=ann-code").await;
    assert_eq!((outcome.as_str(), message.as_str()), (SIGNED_IN, "ann@live.com 2"));
    assert_eq!(s.account(&second).await.label().await.unwrap(), "ann@live.com 2");

    // No email: `Personal`.
    mock_identity(&s.server, "nameless-code", "AT3", None, "D3").await;
    let (third, outcome, message) = s.sign_in_with("code=nameless-code").await;
    assert_eq!((outcome.as_str(), message.as_str()), (SIGNED_IN, "Personal"));
    assert_eq!(s.manager.list().await.unwrap().last(), Some(&third));
    s.no_more_finished().await;
}

/// One draft at a time: a second `SignIn` ends the first as `cancelled`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_sign_in_ends_the_first_draft_as_cancelled() {
    let mut s = setup(Duration::from_secs(10)).await;
    let (first, _url) = s.manager.sign_in().await.unwrap();
    let (second, url) = s.manager.sign_in().await.unwrap();
    assert_ne!(first, second);
    let (ended, outcome, message) = s.next_finished().await;
    assert_eq!((ended, outcome.as_str(), message.as_str()), (first.clone(), CANCELLED, ""));
    let objects = introspect(&s.client, ACCOUNTS_PATH).await;
    assert!(!objects.contains(first.as_str().rsplit('/').next().unwrap()), "{objects}");
    assert_eq!(s.account(&second).await.state().await.unwrap(), "signing-in");

    // The second is the one that signs in.
    assert_eq!(simulate_browser(&url, "code=good-code").await.status(), 200);
    let (ended, outcome, _) = s.next_finished().await;
    assert_eq!((ended, outcome.as_str()), (second.clone(), SIGNED_IN));
    assert_eq!(s.manager.list().await.unwrap(), [second]);
    s.no_more_finished().await;
}

/// A draft `config.toml` still holds at a start is removed, with its directory and its
/// stored token, before the accounts come up; an account beside it stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_draft_left_in_config_toml_is_removed_at_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let wallet = Arc::new(MemoryWallet::default());
    let draft = {
        let store = ConfigStore::open(&Paths::in_dir(dir.path()), async { false }).await;
        store.add_account("Kept").unwrap();
        store.add_draft().unwrap().id
    };
    std::fs::create_dir_all(account_dir(dir.path(), draft.as_str())).unwrap();
    std::fs::write(account_dir(dir.path(), draft.as_str()).join("account.json"), b"{}").unwrap();
    wallet.store(&Slot::Account(draft.clone()), "KOneDrive", "left").await.unwrap();

    let bus = TestBus::start();
    let server = MockServer::start().await;
    let _daemon = start_daemon(&bus, dir.path(), endpoints(&server), wallet.clone(), Duration::from_secs(5)).await;
    let client = bus.connect().await;
    let manager = AccountsProxy::new(&client).await.unwrap();

    let list = manager.list().await.unwrap();
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(AccountProxy::new(&client, list[0].clone()).await.unwrap().label().await.unwrap(), "Kept");
    let config = std::fs::read_to_string(Paths::in_dir(dir.path()).config_file).unwrap();
    assert!(!config.contains(draft.as_str()) && !config.contains("draft") && config.contains("Kept"), "{config}");
    assert!(!account_dir(dir.path(), draft.as_str()).exists());
    assert_eq!(wallet.current(&Slot::Account(draft.clone())), None);
    assert!(!introspect(&client, ACCOUNTS_PATH).await.contains(draft.as_str()));
}

/// A `SignIn` that is refused leaves nothing. (`NoClientId` cannot be made to happen: with
/// none set the daemon signs in with its own client ID. A `config.toml` that cannot be read
/// refuses here.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_sign_in_leaves_nothing() {
    let mut s = setup(Duration::from_secs(10)).await;
    std::fs::write(Paths::in_dir(s.dir.path()).config_file, b"this is not = = toml").unwrap();
    let refused = s.manager.sign_in().await.expect_err("config.toml cannot be read");
    assert_eq!(error_name(&refused), Some("org.freedesktop.DBus.Error.Failed"), "{refused}");
    assert!(s.manager.list().await.unwrap().is_empty());
    let objects = introspect(&s.client, ACCOUNTS_PATH).await;
    assert!(!objects.contains("<node name="), "no object is left on the bus: {objects}");
    let left: Vec<_> = std::fs::read_dir(Paths::in_dir(s.dir.path()).state_dir.join("accounts"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "no directory is left: {left:?}");
    s.no_more_finished().await;
}

/// A development build serves `org.konedrive.DevTools` as its XML says, and its `AddAccount`
/// adds a signed-out account; a release build serves none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dev_tools_are_a_development_builds() {
    let s = setup(Duration::from_secs(5)).await;
    let live = introspect(&s.client, ACCOUNTS_PATH).await;
    #[cfg(not(feature = "dev-tools"))]
    assert!(!live.contains(konedrive_dbus::DEV_TOOLS_INTERFACE_NAME), "{live}");
    #[cfg(feature = "dev-tools")]
    {
        const XML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dbus/org.konedrive.DevTools.xml"));
        let name = konedrive_dbus::DEV_TOOLS_INTERFACE_NAME;
        assert_eq!(signature_lines(&live, name), signature_lines(XML, name));
        let tools = konedrive_dbus::accounts::DevToolsProxy::new(&s.client).await.unwrap();
        let added = tools.add_account("Local").await.unwrap();
        assert_eq!(s.manager.list().await.unwrap(), [added.clone()]);
        assert_eq!(s.account(&added).await.state().await.unwrap(), "signed-out");
        assert_eq!(s.account(&added).await.label().await.unwrap(), "Local");
    }
}
