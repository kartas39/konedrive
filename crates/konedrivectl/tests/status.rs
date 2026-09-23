use std::sync::Arc;
use std::time::Duration;

use konedrive_dbus::testing::TestBus;
use konedrive_dbus::Account1Proxy;
use konedrived::account::AccountService;
use konedrived::config::Paths;
use konedrived::oauth::Endpoints;
use konedrived::secret::MemoryStore;

const CLIENT_ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_state_and_client_id() {
    let bus = TestBus::start();
    let dir = tempfile::tempdir().unwrap();
    let service = AccountService::new(
        Paths::in_dir(dir.path()),
        Endpoints::microsoft(),
        Arc::new(MemoryStore::default()),
        Duration::from_secs(5),
    )
    .unwrap();
    let _server = konedrived::dbus::serve(bus.builder(), service.clone(), None).await.unwrap();
    let client = bus.connect().await;
    let proxy = Account1Proxy::new(&client).await.unwrap();

    let text = konedrivectl::status_text(&proxy).await.unwrap();
    assert!(text.contains("signed-out"), "{text}");
    assert!(text.contains("(not set)"), "{text}");
    assert!(!text.contains("Account:"), "{text}");

    proxy.set_client_id(CLIENT_ID).await.unwrap();
    for _ in 0..500 {
        if proxy.client_id().await.unwrap() == CLIENT_ID {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let text = konedrivectl::status_text(&proxy).await.unwrap();
    assert!(text.contains(CLIENT_ID), "{text}");
}
