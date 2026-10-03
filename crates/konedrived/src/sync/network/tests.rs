use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use konedrive_dbus::testing::TestBus;
use zbus::object_server::SignalEmitter;

struct FakeNetworkManager {
    state: u32,
}

#[zbus::interface(name = "org.freedesktop.NetworkManager")]
impl FakeNetworkManager {
    #[zbus(property)]
    fn state(&self) -> u32 {
        self.state
    }

    /// NetworkManager's `StateChanged`, under another Rust name: the
    /// property's own `state_changed` (its `PropertiesChanged`) takes that.
    #[zbus(signal, name = "StateChanged")]
    async fn announce_state(emitter: &SignalEmitter<'_>, state: u32) -> zbus::Result<()>;
}

#[tokio::test]
async fn coming_back_online_asks_for_a_cycle_once() {
    let bus = TestBus::start();
    let server = bus
        .builder()
        .name("org.freedesktop.NetworkManager")
        .unwrap()
        .serve_at("/org/freedesktop/NetworkManager", FakeNetworkManager { state: 20 })
        .unwrap()
        .build()
        .await
        .unwrap();
    let client = bus.connect().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    tokio::spawn(async move {
        super::watch_on(&client, move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let iface = server
        .object_server()
        .interface::<_, FakeNetworkManager>("/org/freedesktop/NetworkManager")
        .await
        .unwrap();
    for state in [20, 70, 70, 20, 70] {
        FakeNetworkManager::announce_state(iface.signal_emitter(), state).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2, "each return to connected counts once");
}
