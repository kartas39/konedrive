use std::time::Duration;

use anyhow::{anyhow, bail};
use futures_util::StreamExt;
use konedrive_dbus::accounts::SignInFinishedStream;
use konedrive_dbus::sign_in::CANCELLED;
use konedrive_dbus::SERVICE_NAME;
use konedrivectl::text::accounts::added_text;
use konedrivectl::text::refusals::{explain_account_error, AccountAction};
use zbus::fdo::NameOwnerChangedStream;

use super::browser::{open_browser, spawn_browser};
use super::login::SIGN_IN_WAIT;
use crate::daemon::Daemon;

/// How long the command waits for its sign-in's `SignInFinished` once it has asked for the
/// sign-in to be cancelled: one that got through first says so.
const LAST_WORD: Duration = Duration::from_secs(5);

/// How a sign-in ended: the outcome and the message of its `SignInFinished`.
type End = (String, String);

/// `account add`: a new account, added by signing in (`Accounts.SignIn`). The daemon makes
/// the account only once the sign-in has succeeded, names it by its email, and says how the
/// sign-in ended in `SignInFinished`; a sign-in that did not succeed made nothing.
pub(crate) async fn add(daemon: &Daemon) -> anyhow::Result<()> {
    // An unreadable configuration first: a sign-in would be refused for it too.
    let accounts = daemon.accounts().await?;
    daemon.check_loaded(&accounts).await?;
    // Both before the call: the daemon may say how the sign-in ended before its answer is
    // read, and may leave the bus at any time.
    let mut finished = daemon.manager.receive_sign_in_finished().await?;
    let bus = zbus::fdo::DBusProxy::new(&daemon.connection).await?;
    let mut owners = bus.receive_name_owner_changed_with_args(&[(0, SERVICE_NAME)]).await?;
    let result = daemon.manager.sign_in().await;
    let (sign_in, url) = result.map_err(|e| anyhow!(explain_account_error(AccountAction::Add, &e)))?;
    if open_browser() {
        println!("Opening the Microsoft sign-in page in your browser. If it does not open, visit:\n\n  {url}\n");
        spawn_browser(&url);
    } else {
        println!("Sign in on the Microsoft sign-in page to add the account. Open this address in a browser:\n\n  {url}\n");
    }

    // `Err`: the command gives the sign-in up, and what it then ends with.
    let ended = tokio::select! {
        result = tokio::time::timeout(SIGN_IN_WAIT, wait_for_end(&mut finished, &mut owners, sign_in)) => match result {
            Ok(end) => Ok(end?),
            Err(_) => Err("timed out"),
        },
        _ = tokio::signal::ctrl_c() => Err("cancelled"),
    };
    let (outcome, message) = match ended {
        Ok(end) => end,
        Err(why) => {
            // The sign-in may have ended by itself by now: then there is nothing to cancel,
            // and how it ended is said all the same.
            let _ = daemon.manager.cancel_sign_in(sign_in).await;
            match tokio::time::timeout(LAST_WORD, own_end(&mut finished, sign_in)).await {
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

/// The next `SignInFinished` of the sign-in `sign_in`; `None` when the signals have ended.
async fn own_end(finished: &mut SignInFinishedStream, sign_in: u32) -> anyhow::Result<Option<End>> {
    while let Some(signal) = finished.next().await {
        let said = signal.args()?;
        if said.sign_in == sign_in {
            return Ok(Some((said.outcome.clone(), said.message.clone())));
        }
    }
    Ok(None)
}

/// How the sign-in `sign_in` ended. A daemon that stops sends no `SignInFinished`, and the
/// signals do not end with it: the wait ends with an error once the daemon's name has left
/// the bus, or has gone to another daemon, which knows nothing of the sign-in.
async fn wait_for_end(finished: &mut SignInFinishedStream, owners: &mut NameOwnerChangedStream, sign_in: u32) -> anyhow::Result<End> {
    let stopped = || anyhow!("the daemon stopped before the sign-in ended. Nothing was added");
    tokio::select! {
        // An outcome that was said before the daemon left is the answer.
        biased;
        end = own_end(finished, sign_in) => end?.ok_or_else(stopped),
        _ = owners.next() => Err(stopped()),
    }
}
