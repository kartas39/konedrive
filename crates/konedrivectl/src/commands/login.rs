use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use konedrive_dbus::accounts::AccountProxy;
use konedrivectl::{AccountAction, AccountInfo, FIRST_LABEL};

use super::browser::{open_browser, spawn_browser};
use crate::daemon::{wanted, Daemon};

/// `login` (design §5.2): the chosen account's sign-in. With no account at all and none
/// named, it first adds one called `Personal`, so the documented setup — `set-client-id`,
/// `login`, `sync register ~/OneDrive` — keeps working word for word.
pub(crate) async fn login(daemon: &Daemon, option: Option<&str>) -> anyhow::Result<()> {
    let accounts = daemon.accounts().await?;
    // An unreadable configuration first: it also leaves the client ID unknown.
    daemon.check_loaded(&accounts).await?;
    // A name that fits no account, or several accounts and none named, before the client ID.
    let named = match (accounts.is_empty(), wanted(option)) {
        (true, None) => None,
        (_, wanted) => Some(konedrivectl::choose(&accounts, wanted)?.clone()),
    };
    // Before anything is added: every account signs in with it.
    if daemon.manager.client_id().await?.is_empty() {
        bail!(
            "no client ID is set yet. Save the Application (client) ID of your Microsoft Entra app \
             registration first: `konedrivectl set-client-id <id>` (see the README)"
        );
    }
    let chosen = match named {
        Some(chosen) => chosen,
        None => {
            let result = daemon.manager.add(FIRST_LABEL).await;
            let action = AccountAction::Add(FIRST_LABEL);
            let path = result.map_err(|e| anyhow!(konedrivectl::explain_account_error(action, &e)))?;
            let id = daemon.account(&path).await?.id().await?;
            println!("Added an account called {FIRST_LABEL} (`konedrivectl account rename {FIRST_LABEL} <label>` renames it).");
            AccountInfo { path, id, label: FIRST_LABEL.to_owned(), email: String::new() }
        }
    };
    let proxy = daemon.account(&chosen.path).await?;
    let result = proxy.begin_sign_in().await;
    let url = result.map_err(|e| anyhow!(konedrivectl::explain_account_error(AccountAction::SignIn(&chosen.label), &e)))?;
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
        result = tokio::time::timeout(Duration::from_secs(6 * 60), konedrivectl::wait_for_sign_in(&wait_proxy)) => {
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
