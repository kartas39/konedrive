//! "Immediately when the network comes back" (Poller):
//! NetworkManager's `StateChanged` on the system bus. Without NetworkManager,
//! the poller's retry schedule alone brings the folder up to date.

use std::sync::Arc;

use futures_util::StreamExt;

use super::SyncService;

/// `NM_STATE_CONNECTED_GLOBAL`.
const CONNECTED_GLOBAL: u32 = 70;

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager",
    gen_blocking = false
)]
trait NetworkManager {
    #[zbus(signal)]
    fn state_changed(&self, state: u32) -> zbus::Result<()>;
}

/// Asks the OneDrive folder's sync for a cycle each time the machine is
/// online again. For the life of the daemon; never in a test (it is the
/// system bus).
pub async fn watch(service: Arc<SyncService>) {
    match zbus::Connection::system().await {
        Ok(connection) => watch_on(&connection, move || service.refresh_now()).await,
        Err(e) => tracing::info!("no system bus ({e}); a lost network is noticed by retrying"),
    }
}

/// Calls `on_connected` each time the state becomes "connected" from
/// anything else.
pub async fn watch_on(connection: &zbus::Connection, on_connected: impl Fn()) {
    let proxy = match NetworkManagerProxy::new(connection).await {
        Ok(proxy) => proxy,
        Err(e) => {
            return tracing::info!(
                "NetworkManager is not reachable ({e}); a lost network is noticed by retrying"
            )
        }
    };
    let mut changes = match proxy.receive_state_changed().await {
        Ok(changes) => changes,
        Err(e) => {
            return tracing::info!(
                "cannot follow NetworkManager ({e}); a lost network is noticed by retrying"
            )
        }
    };
    // The state before the first signal is taken as unknown rather than
    // read: a read after subscribing could return a "connected" whose signal
    // is already in the stream, and that return would be missed. NetworkManager
    // signals only a change, so an unknown start costs no extra cycle.
    let mut last = None;
    while let Some(signal) = changes.next().await {
        let Ok(args) = signal.args() else { continue };
        let now = *args.state();
        if now == CONNECTED_GLOBAL && last != Some(CONNECTED_GLOBAL) {
            on_connected();
        }
        last = Some(now);
    }
    tracing::info!("NetworkManager's signals ended; a lost network is noticed by retrying");
}

#[cfg(test)]
mod tests {
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
}
