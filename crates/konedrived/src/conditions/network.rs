//! "Immediately when the network comes back" (Poller):
//! NetworkManager's `StateChanged` on the system bus. Without NetworkManager,
//! the poller's retry schedule alone brings the folder up to date.

use std::sync::Arc;

use futures_util::StreamExt;

use crate::conditions::Accounts;

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

/// Asks every account's OneDrive folder for a cycle each time the machine is
/// online again: one watcher for the daemon, whose hub knows every account.
/// For the life of the daemon; never in a test (it is the system bus).
pub async fn watch(hub: Arc<impl Accounts>) {
    let refresh_all = move || hub.refresh_now();
    match zbus::Connection::system().await {
        Ok(connection) => watch_on(&connection, refresh_all).await,
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
mod tests;
