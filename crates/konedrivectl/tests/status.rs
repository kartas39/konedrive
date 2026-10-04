mod common;

use std::time::Duration;

use konedrive_dbus::accounts::AccountsProxy;
use konedrive_dbus::testing::TestBus;

const CLIENT_ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_state_and_client_id() {
    let bus = TestBus::start();
    let dir = tempfile::tempdir().unwrap();
    let _daemon = common::start_daemon(&bus, dir.path()).await;
    let client = bus.connect().await;
    let manager = AccountsProxy::new(&client).await.unwrap();
    manager.add("Personal").await.unwrap();

    let out = common::run(bus.address(), &["status"]);
    assert!(out.status.success(), "{out:?}");
    let text = common::out_text(&out);
    assert!(text.lines().any(|l| l == "Label:      Personal"), "{text}");
    assert!(text.lines().any(|l| l == "State:      signed-out"), "{text}");
    assert!(text.contains(konedrived::config::DEFAULT_CLIENT_ID), "the built-in client ID: {text}");
    assert!(!text.contains("Account:"), "{text}");

    manager.set_client_id(CLIENT_ID).await.unwrap();
    for _ in 0..500 {
        if manager.client_id().await.unwrap() == CLIENT_ID {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let out = common::run(bus.address(), &["status"]);
    assert!(out.status.success(), "{out:?}");
    assert!(common::out_text(&out).contains(CLIENT_ID), "{}", common::out_text(&out));
}

/// What `status` and `sync status` take for "the daemon has no such property" is what the
/// daemon answers for one: a daemon of an older build then leaves a line out, and no more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_property_the_daemon_does_not_have_is_known_as_such() {
    let bus = TestBus::start();
    let dir = tempfile::tempdir().unwrap();
    let _daemon = common::start_daemon(&bus, dir.path()).await;
    let client = bus.connect().await;
    let manager = AccountsProxy::new(&client).await.unwrap();
    let path = manager.add("Personal").await.unwrap();

    for interface in [konedrive_dbus::ACCOUNT_INTERFACE_NAME, konedrive_dbus::FOLDER_INTERFACE_NAME] {
        for cache in [zbus::proxy::CacheProperties::Lazily, zbus::proxy::CacheProperties::No] {
            let proxy = zbus::proxy::Builder::<zbus::Proxy>::new(&client)
                .destination(konedrive_dbus::SERVICE_NAME)
                .unwrap()
                .path(path.clone())
                .unwrap()
                .interface(interface)
                .unwrap()
                .cache_properties(cache)
                .build()
                .await
                .unwrap();
            let error = proxy.get_property::<String>("NotInThisBuild").await.unwrap_err();
            assert!(konedrive_dbus::is_unknown_property(&error), "{interface}: {error:?}");
        }
    }
}
