mod common;

use std::time::Duration;

use konedrive_dbus::accounts::{Account1Proxy, Accounts1Proxy};
use konedrive_dbus::testing::TestBus;

const CLIENT_ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

/// Regression test for the "seen signing-in" gate hang: `wait_for_sign_in` polls `State`
/// directly instead of watching the (coalescing) `StateChanged` signal stream, so it must
/// notice a `signing-in` -> `signed-out` transition driven by a second client promptly,
/// rather than waiting for a signal that a coalesced watch channel might never deliver.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_for_sign_in_reports_cancellation_promptly() {
    let bus = TestBus::start();
    let dir = tempfile::tempdir().unwrap();
    let _daemon = common::start_daemon(&bus, dir.path()).await;

    // The "driving" client: adds the account, sets up the client ID and later cancels the
    // sign-in.
    let driver = bus.connect().await;
    let manager = Accounts1Proxy::new(&driver).await.unwrap();
    let path = manager.add("Personal").await.unwrap();
    let driver_proxy = Account1Proxy::new(&driver, path.clone()).await.unwrap();
    manager.set_client_id(CLIENT_ID).await.unwrap();

    // A second, independent client: this is the one that polls, uncached, exactly as
    // `konedrivectl login` does.
    let waiter = bus.connect().await;
    let waiter_proxy = Account1Proxy::builder(&waiter)
        .path(path)
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap();

    driver_proxy.begin_sign_in().await.unwrap();

    let wait_task = tokio::spawn(async move { konedrivectl::wait_for_sign_in(&waiter_proxy).await });

    // Cancel from the driving client while the waiter is polling.
    driver_proxy.cancel_sign_in().await.unwrap();

    let result = tokio::time::timeout(Duration::from_secs(5), wait_task)
        .await
        .expect("wait_for_sign_in did not return promptly")
        .expect("wait_for_sign_in task panicked");

    let err = result.expect_err("expected wait_for_sign_in to report the cancellation");
    assert!(err.to_string().contains("cancelled"), "{err}");
}
