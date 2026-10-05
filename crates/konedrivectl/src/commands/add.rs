use std::time::Duration;

use anyhow::{anyhow, bail};
use futures_util::StreamExt;
use konedrive_dbus::accounts::{AccountProxy, SignInFinishedStream};
use konedrive_dbus::sign_in::CANCELLED;
use konedrive_dbus::Refusal;
use konedrivectl::text::accounts::added_text;
use konedrivectl::text::refusals::{explain_account_error, AccountAction};
use zbus::zvariant::OwnedObjectPath;

use super::browser::{open_browser, spawn_browser};
use super::login::SIGN_IN_WAIT;
use crate::daemon::Daemon;

/// How often the wait asks whether the draft, and the daemon, are still there.
const STILL_THERE: Duration = Duration::from_millis(500);

/// How long the command waits for its draft's `SignInFinished` once the draft is known to
/// have ended, or was asked to end: the signal is sent after the draft has left the bus.
const LAST_WORD: Duration = Duration::from_secs(5);

/// How a draft ended: the outcome and the message of its `SignInFinished`.
type End = (String, String);

/// `account add`: a new account, added by signing in (`Accounts.SignIn`). The daemon keeps
/// the account as a draft nobody lists until the sign-in succeeds, names it by its email,
/// and says how it ended in `SignInFinished`; nothing is left of a sign-in that did not
/// succeed.
pub(crate) async fn add(daemon: &Daemon) -> anyhow::Result<()> {
    // An unreadable configuration first: a sign-in would be refused for it too.
    let accounts = daemon.accounts().await?;
    daemon.check_loaded(&accounts).await?;
    // Before the call: the daemon may say how the sign-in ended before its answer is read.
    let mut finished = daemon.manager.receive_sign_in_finished().await?;
    let result = daemon.manager.sign_in().await;
    let (draft, url) = result.map_err(|e| anyhow!(explain_account_error(AccountAction::Add, &e)))?;
    if open_browser() {
        println!("Opening the Microsoft sign-in page in your browser. If it does not open, visit:\n\n  {url}\n");
        spawn_browser(&url);
    } else {
        println!("Sign in on the Microsoft sign-in page to add the account. Open this address in a browser:\n\n  {url}\n");
    }

    // Uncached: every question is asked of the daemon that is there now.
    let watched = AccountProxy::builder(&daemon.connection)
        .path(draft.clone())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    // `Err`: the command gives the sign-in up, and what it then ends with.
    let ended = tokio::select! {
        result = tokio::time::timeout(SIGN_IN_WAIT, wait_for_end(&mut finished, &watched, &draft)) => match result {
            Ok(end) => Ok(end?),
            Err(_) => Err("timed out"),
        },
        _ = tokio::signal::ctrl_c() => Err("cancelled"),
    };
    let (outcome, message) = match ended {
        Ok(end) => end,
        Err(why) => {
            // The draft may have ended by itself by now: then there is nothing to cancel,
            // and how it ended is said all the same.
            let _ = watched.cancel_sign_in().await;
            match tokio::time::timeout(LAST_WORD, own_end(&mut finished, &draft)).await {
                // A sign-in that got through before the cancel: the account is there.
                Ok(Ok(Some((outcome, message)))) if outcome != CANCELLED => (outcome, message),
                _ => bail!("{why}"),
            }
        }
    };
    match added_text(&outcome, &message) {
        Ok(said) => print!("{said}"),
        Err(why) => bail!("{why}"),
    }
    Ok(())
}

/// The next `SignInFinished` of `draft`; `None` when the signals have ended.
async fn own_end(finished: &mut SignInFinishedStream, draft: &OwnedObjectPath) -> anyhow::Result<Option<End>> {
    while let Some(signal) = finished.next().await {
        let said = signal.args()?;
        if said.account == *draft {
            return Ok(Some((said.outcome.clone(), said.message.clone())));
        }
    }
    Ok(None)
}

/// How `draft` ended. A daemon that stops sends no `SignInFinished`, and the signals do not
/// end with it, so the draft is asked for while waiting: once the daemon has left the bus,
/// or the draft's object is gone and no signal follows it, the wait ends with an error.
async fn wait_for_end(finished: &mut SignInFinishedStream, watched: &AccountProxy<'_>, draft: &OwnedObjectPath) -> anyhow::Result<End> {
    let stopped = || anyhow!("the daemon stopped before the sign-in ended. Nothing was added");
    let mut ask = tokio::time::interval(STILL_THERE);
    loop {
        tokio::select! {
            end = own_end(finished, draft) => return end?.ok_or_else(stopped),
            _ = ask.tick() => match watched.state().await {
                Ok(_) => {}
                // The draft ended: the daemon says how right after taking it off the bus.
                Err(e) if Refusal::says_gone(&e) => {
                    return match tokio::time::timeout(LAST_WORD, own_end(finished, draft)).await {
                        Ok(Ok(Some(end))) => Ok(end),
                        _ => Err(stopped()),
                    };
                }
                Err(_) => return Err(stopped()),
            },
        }
    }
}
