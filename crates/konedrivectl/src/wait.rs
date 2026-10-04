//! The waits for a sign-in to end: `login`'s and `account mode read-write`'s.

use std::time::Duration;

use konedrive_dbus::accounts::AccountProxy;

/// Polls `proxy` until `SetMode("read-write")`'s sign-in has ended: `Ok` once `Mode` is
/// `read-write`, `Err` with `LastError` when the switch did not go through. The account stays
/// `signed-in` throughout, so `State` cannot tell; it is polled for the same reason
/// [`wait_for_sign_in`] polls.
pub(crate) async fn wait_for_read_write(proxy: &AccountProxy<'_>) -> anyhow::Result<()> {
    loop {
        if proxy.mode().await? == "read-write" {
            return Ok(());
        }
        // `SetMode` clears `LastError` before it answers, so anything in it now is why the
        // switch did not go through. Cancelled, it says nothing: the caller stops waiting.
        let last_error = proxy.last_error().await?;
        if !last_error.is_empty() {
            anyhow::bail!("the account stays read-only: {last_error}");
        }
        if proxy.state().await? != "signed-in" {
            anyhow::bail!("the account was signed out; it stays read-only");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Polls `proxy` until the account leaves the `signing-in` state, then reports the outcome.
///
/// Polling (rather than watching the `StateChanged` signal) sidesteps a coalescing hazard:
/// the daemon publishes state through a `tokio::watch` channel, so a fast transition (e.g.
/// `signing-in` -> `signed-in` completing almost immediately, as with an SSO session) can be
/// collapsed and observed only as its final value. A waiter that only starts counting once it
/// has *seen* `signing-in` on the signal stream could then wait forever for a change that
/// already happened. `BeginSignIn` sets the state to `signing-in` before it replies, so the
/// very first poll here is guaranteed to observe either `signing-in` or the terminal state -
/// there is no window in which the relevant transition can be missed.
pub(crate) async fn wait_for_sign_in(proxy: &AccountProxy<'_>) -> anyhow::Result<()> {
    loop {
        let state = proxy.state().await?;
        if state != "signing-in" {
            return if state == "signed-in" {
                Ok(())
            } else {
                let last_error = proxy.last_error().await?;
                if last_error.is_empty() {
                    anyhow::bail!("sign-in was cancelled");
                } else {
                    anyhow::bail!("sign-in did not complete: {last_error}");
                }
            };
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
