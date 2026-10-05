use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use konedrive_dbus::accounts::AccountProxy;
use konedrivectl::text::refusals::{explain_account_error, AccountAction};

use super::browser::{open_browser, spawn_browser};
use crate::daemon::Daemon;
use crate::wait;

/// How long a command waits for a sign-in in the browser.
pub(crate) const SIGN_IN_WAIT: Duration = Duration::from_secs(6 * 60);

/// `login` (design §5.2): the chosen account's sign-in, for an account that is signed out. It
/// adds none: with no account at all it is refused, and the refusal names `account add`.
pub(crate) async fn login(daemon: &Daemon, option: Option<&str>) -> anyhow::Result<()> {
    // An unreadable configuration first, then a name that fits no account, several accounts
    // and none named, or no account at all: before the client ID.
    let chosen = daemon.chosen(option).await?.account;
    if daemon.manager.client_id().await?.is_empty() {
        bail!(
            "no client ID is set yet. Save the Application (client) ID of your Microsoft Entra app \
             registration first: `konedrivectl set-client-id <id>` (see the README)"
        );
    }
    let proxy = daemon.account(&chosen.path).await?;
    let result = proxy.begin_sign_in().await;
    let url = result.map_err(|e| anyhow!(explain_account_error(AccountAction::SignIn(&chosen.label), &e)))?;
    if open_browser() {
        println!(
            "Opening the Microsoft sign-in page for {} in your browser. If it does not open, visit:\n\n  {url}\n",
            chosen.label
        );
        spawn_browser(&url);
    } else {
        println!("Sign {} in on the Microsoft sign-in page. Open this address in a browser:\n\n  {url}\n", chosen.label);
    }

    // An uncached proxy: the wait loop polls `State` directly rather than watching
    // `StateChanged`, so it never misses a transition the signal stream coalesced away.
    let wait_proxy = AccountProxy::builder(&daemon.connection)
        .path(chosen.path.clone())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;

    tokio::select! {
        result = tokio::time::timeout(SIGN_IN_WAIT, wait::wait_for_sign_in(&wait_proxy)) => {
            result.context("timed out")??;
        }
        _ = tokio::signal::ctrl_c() => {
            proxy.cancel_sign_in().await?;
            bail!("cancelled");
        }
    }
    match wait_proxy.email().await.unwrap_or_default() {
        email if email.is_empty() => println!("Signed in to {}.", chosen.label),
        email => println!("Signed in to {} as {email}.", chosen.label),
    }
    Ok(())
}
