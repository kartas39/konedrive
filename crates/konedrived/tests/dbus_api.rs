//! `org.konedrive.Account1` over a private test bus, on the account object the manager
//! exports (`/org/konedrive/Accounts/<id>`); the client id is `Accounts1`'s.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use konedrive_dbus::accounts::{Account1Proxy, Accounts1Proxy};
use konedrive_dbus::testing::TestBus;
use konedrive_dbus::ACCOUNT_INTERFACE_NAME;
use konedrived::secret::{MemoryWallet, Slot};
use wiremock::MockServer;

const XML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../dbus/org.konedrive.Account1.xml"));

struct Setup {
    manager: Accounts1Proxy<'static>,
    proxy: Account1Proxy<'static>,
    id: String,
    wallet: Arc<MemoryWallet>,
    client: zbus::Connection,
    _daemon: konedrived::accounts::Daemon,
    _server: MockServer,
    _dir: tempfile::TempDir,
    _bus: TestBus,
}

/// A daemon with one account, `Personal`, and Microsoft played by wiremock.
async fn setup(sign_in_timeout: Duration) -> Setup {
    let bus = TestBus::start();
    let server = MockServer::start().await;
    mock_microsoft(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let wallet = Arc::new(MemoryWallet::default());
    let daemon = start_daemon(&bus, dir.path(), endpoints(&server), wallet.clone(), sign_in_timeout).await;
    let client = bus.connect().await;
    let manager = Accounts1Proxy::new(&client).await.unwrap();
    let path = manager.add("Personal").await.unwrap();
    let id = path.as_str().rsplit('/').next().unwrap().to_owned();
    let proxy = Account1Proxy::new(&client, path).await.unwrap();
    Setup { manager, proxy, id, wallet, client, _daemon: daemon, _server: server, _dir: dir, _bus: bus }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exposes_initial_properties() {
    let s = setup(Duration::from_secs(5)).await;
    assert_eq!(s.proxy.state().await.unwrap(), "signed-out");
    assert_eq!(s.proxy.last_error().await.unwrap(), "");
    assert_eq!(s.proxy.quota_total().await.unwrap(), 0);
    assert_eq!(s.proxy.id().await.unwrap(), s.id);
    assert_eq!(s.proxy.label().await.unwrap(), "Personal");
    assert_eq!(s.proxy.mode().await.unwrap(), "read-only");
    assert_eq!(s.manager.client_id().await.unwrap(), "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_client_id_validates_and_notifies() {
    let s = setup(Duration::from_secs(5)).await;
    let err = s.manager.set_client_id("not-a-guid").await.unwrap_err();
    assert!(
        matches!(&err, zbus::Error::MethodError(name, _, _) if name.as_str() == "org.freedesktop.DBus.Error.InvalidArgs"),
        "{err:?}"
    );
    assert_eq!(s.manager.client_id().await.unwrap(), "");
    s.manager.set_client_id(CLIENT_ID).await.unwrap();
    let manager = &s.manager;
    eventually("ClientId", || async move { manager.client_id().await.unwrap() == CLIENT_ID }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sign_in_and_out_over_dbus() {
    let s = setup(Duration::from_secs(10)).await;
    let proxy = &s.proxy;
    s.manager.set_client_id(CLIENT_ID).await.unwrap();
    let url = proxy.begin_sign_in().await.unwrap();
    eventually("signing-in", || async move { proxy.state().await.unwrap() == "signing-in" }).await;

    assert_eq!(simulate_browser(&url, "code=good-code").await.status(), 200);
    eventually("quota", || async move { proxy.quota_total().await.unwrap() == 5368709120 }).await;
    assert_eq!(proxy.state().await.unwrap(), "signed-in");
    assert_eq!(proxy.display_name().await.unwrap(), "Test User");
    assert_eq!(proxy.email().await.unwrap(), "test@outlook.com");
    assert_eq!(proxy.quota_used().await.unwrap(), 1073741824);
    let item = Slot::Account(s.id.clone());
    assert_eq!(s.wallet.current(&item).as_deref(), Some("RT1"));
    assert_eq!(s.wallet.label(&item).as_deref(), Some("KOneDrive: test@outlook.com"));

    // The client id is not changed under a signed-in account.
    let refused = s.manager.set_client_id(CLIENT_ID).await.unwrap_err();
    assert!(matches!(&refused, zbus::Error::MethodError(name, _, _) if name.as_str() == "org.freedesktop.DBus.Error.Failed"));

    proxy.sign_out().await.unwrap();
    eventually("signed-out", || async move { proxy.state().await.unwrap() == "signed-out" }).await;
    assert_eq!(proxy.display_name().await.unwrap(), "");
    assert_eq!(s.wallet.current(&item), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn introspection_matches_the_checked_in_xml() {
    let s = setup(Duration::from_secs(5)).await;
    let live = introspect(&s.client, s.proxy.inner().path().as_str()).await;
    assert_eq!(signature_lines(&live, ACCOUNT_INTERFACE_NAME), signature_lines(XML, ACCOUNT_INTERFACE_NAME));
}
