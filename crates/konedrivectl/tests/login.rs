mod common;

use std::process::Stdio;
use std::time::Duration;

use konedrive_dbus::accounts::{AccountProxy, AccountsProxy};
use konedrive_dbus::testing::TestBus;

const CLIENT_ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

/// `login` waits by asking for `State`, not by watching `StateChanged`, whose changes the
/// daemon may fold into one: a sign-in cancelled by another client while `login` waits ends
/// the wait promptly, and `login` says that it was cancelled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_reports_a_sign_in_cancelled_elsewhere_promptly() {
    let bus = TestBus::start();
    let dir = tempfile::tempdir().unwrap();
    let _daemon = common::start_daemon(&bus, dir.path()).await;

    // The other client: it adds the account, sets the client ID and later cancels the sign-in.
    let driver = bus.connect().await;
    let manager = AccountsProxy::new(&driver).await.unwrap();
    let path = manager.add("Personal").await.unwrap();
    manager.set_client_id(CLIENT_ID).await.unwrap();
    let account = AccountProxy::builder(&driver)
        .path(path)
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();

    let mut login = common::command(bus.address(), &["login"], &[]);
    let login = login.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("failed to run the konedrivectl binary");
    for _ in 0..500 {
        if account.state().await.unwrap() == "signing-in" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(account.state().await.unwrap(), "signing-in", "login began the sign-in");
    account.cancel_sign_in().await.unwrap();

    let waited = tokio::task::spawn_blocking(move || login.wait_with_output());
    let out = tokio::time::timeout(Duration::from_secs(5), waited)
        .await
        .expect("login did not end promptly")
        .unwrap()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    assert!(common::err_text(&out).contains("sign-in was cancelled"), "{out:?}");
}
