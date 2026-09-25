mod common;

use std::time::Duration;

use konedrive_dbus::accounts::{Account1Proxy, Accounts1Proxy};
use konedrive_dbus::testing::TestBus;

const CLIENT_ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_state_and_client_id() {
    let bus = TestBus::start();
    let dir = tempfile::tempdir().unwrap();
    let _daemon = common::start_daemon(&bus, dir.path()).await;
    let client = bus.connect().await;
    let manager = Accounts1Proxy::new(&client).await.unwrap();
    let proxy = Account1Proxy::new(&client, manager.add("Personal").await.unwrap()).await.unwrap();

    let text = konedrivectl::status_text(&proxy, Some(&manager.client_id().await.unwrap())).await.unwrap();
    assert!(text.lines().any(|l| l == "Label:      Personal"), "{text}");
    assert!(text.contains("signed-out"), "{text}");
    assert!(text.contains("(not set)"), "{text}");
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
