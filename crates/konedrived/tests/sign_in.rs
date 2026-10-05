//! `Accounts.SignIn` over a private test bus: a sign-in for a new account belongs to no
//! account, and the account is made, named by its email, only once it has succeeded; a
//! sign-in that did not succeed never made anything. Microsoft is wiremock; the wallet is
//! in memory.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use futures_util::StreamExt;
use konedrive_dbus::accounts::{AccountProxy, AccountsProxy, SignInFinishedStream};
use konedrive_dbus::sign_in::{ALREADY_ADDED, CANCELLED, FAILED, SIGNED_IN};
use konedrive_dbus::testing::TestBus;
use konedrive_dbus::ACCOUNTS_PATH;
use konedrived::account::secret::Slot;
use konedrived::account::testing::MemoryWallet;
use konedrived::config::Paths;
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zbus::zvariant::OwnedObjectPath;

/// What a `SignInFinished` said: the sign-in's number, the outcome, the message, the account.
type Finished = (u32, String, String, String);

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
    /// The next `SignInFinished`.
    async fn next_finished(&mut self) -> Finished {
        let signal = tokio::time::timeout(Duration::from_secs(20), self.finished.next())
            .await
            .expect("no SignInFinished came")
            .expect("the signal stream ended");
        let said = signal.args().unwrap();
        (said.sign_in, said.outcome.clone(), said.message.clone(), said.account.to_string())
    }

    /// No further `SignInFinished` comes.
    async fn no_more_finished(&mut self) {
        let more = tokio::time::timeout(Duration::from_millis(300), self.finished.next()).await;
        assert!(more.is_err(), "a second SignInFinished: {:?}", more.unwrap().map(|s| format!("{:?}", s.args())));
    }

    async fn account(&self, path: &str) -> AccountProxy<'static> {
        AccountProxy::builder(&self.client)
            .path(path.to_owned())
            .unwrap()
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .unwrap()
    }

    fn config_text(&self) -> String {
        std::fs::read_to_string(Paths::in_dir(self.dir.path()).config_file).unwrap_or_default()
    }

    /// A `SignIn` played to its end in the browser with `answer`: how it ended, without
    /// its number, which is the one `SignIn` answered.
    async fn sign_in_with(&mut self, answer: &str) -> (String, String, String) {
        let (sign_in, url) = self.manager.sign_in().await.unwrap();
        assert_eq!(simulate_browser(&url, answer).await.status(), 200);
        let (ended, outcome, message, account) = self.next_finished().await;
        assert_eq!(ended, sign_in);
        (outcome, message, account)
    }

    /// The daemon has `accounts` accounts and nothing else: in `List`, in `config.toml`,
    /// in the state directory, in the wallet, on the bus and among the folders. So a
    /// sign-in that is under way, or did not succeed, has made nothing.
    async fn there_are(&self, accounts: usize) {
        assert_eq!(self.manager.list().await.unwrap().len(), accounts, "List");
        let config = self.config_text();
        assert_eq!(config.matches("[[accounts]]").count(), accounts, "{config}");
        let dirs: Vec<_> = std::fs::read_dir(Paths::in_dir(self.dir.path()).state_dir.join("accounts"))
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        assert_eq!(dirs.len(), accounts, "the state directory: {dirs:?}");
        assert_eq!(self.wallet.count(), accounts, "the stored tokens");
        let objects = introspect(&self.client, ACCOUNTS_PATH).await;
        assert_eq!(objects.matches("<node name=").count(), accounts, "the objects on the bus: {objects}");
        assert_eq!(self.daemon.manager.accounts().len(), accounts);
        assert_eq!(self.daemon.manager.registry().accounts().len(), accounts, "the folders");
    }
}

fn id_of(path: &str) -> konedrived::config::AccountId {
    konedrived::config::AccountId::new(path.rsplit('/').next().unwrap())
}

/// A `SignIn` that succeeds: nothing exists for it until the browser has answered; then
/// the account is in `List` under its email, signed in, with one `signed-in` naming its
/// path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sign_in_that_succeeds_makes_the_account_under_its_email() {
    let mut s = setup(Duration::from_secs(10)).await;
    let (sign_in, url) = s.manager.sign_in().await.unwrap();
    assert!(url.contains("prompt=select_account"), "Microsoft's account picker: {url}");

    // Under way: nothing of it anywhere.
    s.there_are(0).await;

    assert_eq!(simulate_browser(&url, "code=good-code").await.status(), 200);
    let (ended, outcome, message, path) = s.next_finished().await;
    assert_eq!((ended, outcome.as_str(), message.as_str()), (sign_in, SIGNED_IN, "test@outlook.com"));
    // Listed, signed in, under its final label, by the time the signal is read.
    let listed: Vec<String> = s.manager.list().await.unwrap().iter().map(|p| p.to_string()).collect();
    assert_eq!(listed, [path.clone()]);
    let account = s.account(&path).await;
    assert_eq!(account.label().await.unwrap(), "test@outlook.com");
    assert_eq!(account.state().await.unwrap(), "signed-in");
    assert_eq!(account.mode().await.unwrap(), "read-only");
    let config = s.config_text();
    assert!(config.contains("label = \"test@outlook.com\"") && config.contains("drive_id = \"D1\""), "{config}");
    assert!(config.contains("login_hint = \"test@outlook.com\"") && !config.contains("draft"), "{config}");
    assert_eq!(s.wallet.current(&Slot::Account(id_of(&path))).as_deref(), Some("RT1"));
    s.there_are(1).await;
    s.no_more_finished().await;

    // It is an account like any other: it takes a folder, and is removed.
    let folder = konedrive_dbus::accounts::FolderProxy::new(&s.client, path.clone()).await.unwrap();
    let root = s.dir.path().join("folder");
    std::fs::create_dir(&root).unwrap();
    folder.register_without_interception(root.to_str().unwrap()).await.unwrap();
    s.manager.remove(&OwnedObjectPath::try_from(path).unwrap().as_ref()).await.unwrap();
    s.there_are(0).await;
    s.no_more_finished().await;
}

/// A sign-in the browser refuses, one that is cancelled and one `SetClientId` ends each
/// say how they ended, and nothing was ever made. `CancelSignIn` says whether it
/// cancelled: with a number that is not under way it changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_and_a_cancelled_sign_in_make_nothing() {
    let mut s = setup(Duration::from_secs(10)).await;

    let (outcome, message, account) = s.sign_in_with("error=access_denied").await;
    assert_eq!((outcome.as_str(), message.as_str(), account.as_str()), (FAILED, "Access was denied in the browser.", "/"));
    s.there_are(0).await;

    let (sign_in, url) = s.manager.sign_in().await.unwrap();
    // Not this one's number: one that ended, and one that never was.
    assert!(!s.manager.cancel_sign_in(sign_in - 1).await.unwrap());
    assert!(!s.manager.cancel_sign_in(sign_in + 7).await.unwrap());
    s.no_more_finished().await;
    assert!(s.manager.cancel_sign_in(sign_in).await.unwrap(), "under way: this call ended it");
    assert_eq!(s.next_finished().await, (sign_in, CANCELLED.to_owned(), String::new(), "/".to_owned()));
    // Cancelled already: nothing more is said, and what the browser answers now is dropped.
    assert!(!s.manager.cancel_sign_in(sign_in).await.unwrap());
    let _ = reqwest::get(redirect_of(&url, "code=good-code")).await;
    s.no_more_finished().await;
    s.there_are(0).await;

    let (sign_in, _url) = s.manager.sign_in().await.unwrap();
    s.manager.set_client_id("11111111-2222-3333-4444-555555555555").await.unwrap();
    assert_eq!(s.next_finished().await, (sign_in, CANCELLED.to_owned(), String::new(), "/".to_owned()));
    s.there_are(0).await;
    s.no_more_finished().await;
}

/// The address the browser is sent back to for the sign-in of `authorize_url`, with `answer`.
fn redirect_of(authorize_url: &str, answer: &str) -> String {
    let url = url::Url::parse(authorize_url).unwrap();
    let get = |name: &str| url.query_pairs().find(|(k, _)| k == name).unwrap().1.into_owned();
    format!("{}/?{answer}&state={}", get("redirect_uri").replace("localhost", "127.0.0.1"), get("state"))
}

/// A sign-in nobody finishes ends at the timeout as `failed`, and nothing was made.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sign_in_that_times_out_makes_nothing() {
    let mut s = setup(Duration::from_millis(300)).await;
    let (sign_in, _url) = s.manager.sign_in().await.unwrap();
    let (ended, outcome, message, account) = s.next_finished().await;
    assert_eq!((ended, outcome.as_str(), account.as_str()), (sign_in, FAILED, "/"));
    assert!(message.starts_with("Timed out waiting for the browser"), "{message}");
    s.there_are(0).await;
    s.no_more_finished().await;
}

/// A drive another account has is `already-added`, naming that account; the label of an
/// account whose email is another account's label, and of one with no email.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_drive_decides_whether_an_account_is_added_and_the_email_names_it() {
    let mut s = setup(Duration::from_secs(10)).await;
    let (outcome, _, first) = s.sign_in_with("code=good-code").await;
    assert_eq!(outcome, SIGNED_IN);

    // The same Microsoft account again: the account that has the drive, and nothing made.
    let (outcome, message, account) = s.sign_in_with("code=good-code").await;
    assert_eq!((outcome.as_str(), message.as_str(), account.as_str()), (ALREADY_ADDED, "test@outlook.com", first.as_str()));
    s.there_are(1).await;

    // Another account whose email is, whatever the case, the label of an account that is
    // not that drive: the name decides nothing, and the label is the next free one.
    s.daemon.manager.add("ANN@live.com", &s.daemon.connection).await.unwrap();
    mock_identity(&s.server, "ann-code", "AT2", Some("ann@live.com"), "D2").await;
    let (outcome, message, second) = s.sign_in_with("code=ann-code").await;
    assert_eq!((outcome.as_str(), message.as_str()), (SIGNED_IN, "ann@live.com 2"));
    assert_eq!(s.account(&second).await.label().await.unwrap(), "ann@live.com 2");

    // No email: `Personal`.
    mock_identity(&s.server, "nameless-code", "AT3", None, "D3").await;
    let (outcome, message, third) = s.sign_in_with("code=nameless-code").await;
    assert_eq!((outcome.as_str(), message.as_str()), (SIGNED_IN, "Personal"));
    assert_eq!(s.manager.list().await.unwrap().last().map(|p| p.to_string()), Some(third));
    s.no_more_finished().await;
}

/// One sign-in at a time: a second `SignIn` ends the first as `cancelled`, and each
/// client is told of its own number.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_sign_in_ends_the_first_as_cancelled() {
    let mut s = setup(Duration::from_secs(10)).await;
    let other = s._bus.connect().await;
    let other = AccountsProxy::builder(&other).cache_properties(zbus::proxy::CacheProperties::No).build().await.unwrap();
    let (first, first_url) = s.manager.sign_in().await.unwrap();
    let (second, url) = other.sign_in().await.unwrap();
    assert_ne!(first, second, "a number is given once");
    assert_eq!(s.next_finished().await, (first, CANCELLED.to_owned(), String::new(), "/".to_owned()));
    s.there_are(0).await;

    // The first one's browser answers too late: nothing comes of it. The second is the
    // one that signs in, and its outcome carries its own number.
    let _ = reqwest::get(redirect_of(&first_url, "code=good-code")).await;
    assert_eq!(simulate_browser(&url, "code=good-code").await.status(), 200);
    let (ended, outcome, _, account) = s.next_finished().await;
    assert_eq!((ended, outcome.as_str()), (second, SIGNED_IN));
    assert_eq!(s.manager.list().await.unwrap().iter().map(|p| p.to_string()).collect::<Vec<_>>(), [account]);
    s.there_are(1).await;
    s.no_more_finished().await;
}

/// A refused call ends nothing: a `SignIn` and a `SetClientId` that are refused, here for a
/// `config.toml` that cannot be read, leave the sign-in under way as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_call_leaves_the_sign_in_under_way() {
    let mut s = setup(Duration::from_secs(10)).await;
    let (first, url) = s.manager.sign_in().await.unwrap();
    let config = Paths::in_dir(s.dir.path()).config_file;
    std::fs::write(&config, b"this is not = = toml").unwrap();
    assert!(s.manager.sign_in().await.is_err(), "refused before the browser is opened");
    assert!(s.manager.set_client_id("11111111-2222-3333-4444-555555555555").await.is_err());
    s.no_more_finished().await;

    // The first one is still the one under way: it signs in once the file can be read.
    std::fs::remove_file(&config).unwrap();
    assert_eq!(simulate_browser(&url, "code=good-code").await.status(), 200);
    let (ended, outcome, _, _) = s.next_finished().await;
    assert_eq!((ended, outcome.as_str()), (first, SIGNED_IN));
    s.there_are(1).await;
    s.no_more_finished().await;
}

/// A sign-in whose account cannot be written to `config.toml` is `failed`, and nothing is
/// left of what it began to make.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sign_in_whose_account_cannot_be_saved_makes_nothing() {
    let mut s = setup(Duration::from_secs(10)).await;
    let (sign_in, url) = s.manager.sign_in().await.unwrap();
    std::fs::write(Paths::in_dir(s.dir.path()).config_file, b"this is not = = toml").unwrap();
    assert_eq!(simulate_browser(&url, "code=good-code").await.status(), 200);
    let (ended, outcome, message, account) = s.next_finished().await;
    assert_eq!((ended, outcome.as_str(), account.as_str()), (sign_in, FAILED, "/"), "{message}");
    assert!(!message.is_empty());
    s.there_are(0).await;
    s.no_more_finished().await;
}

/// A token the wallet does not take: `failed`, and the entry, the directory and the
/// object the account was being made with are taken back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sign_in_whose_token_cannot_be_stored_makes_nothing() {
    let mut s = setup(Duration::from_secs(10)).await;
    s.wallet.set_locked(true);
    let (outcome, message, account) = s.sign_in_with("code=good-code").await;
    assert_eq!((outcome.as_str(), account.as_str()), (FAILED, "/"), "{message}");
    s.there_are(0).await;
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
        assert_eq!(s.account(added.as_str()).await.state().await.unwrap(), "signed-out");
        assert_eq!(s.account(added.as_str()).await.label().await.unwrap(), "Local");
    }
}
