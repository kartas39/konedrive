use anyhow::{anyhow, bail};
use futures_util::StreamExt;
use konedrivectl::text::accounts::added_text;
use konedrivectl::text::refusals::{explain_account_error, AccountAction};

use super::browser::{open_browser, spawn_browser};
use super::login::SIGN_IN_WAIT;
use crate::daemon::Daemon;

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

    let ended = async {
        while let Some(signal) = finished.next().await {
            let said = signal.args()?;
            if said.account == draft {
                return Ok((said.outcome.clone(), said.message.clone()));
            }
        }
        bail!("the daemon went away before the sign-in ended")
    };
    let cancel = || async {
        // The draft may have ended by itself by now: then there is nothing to cancel.
        let _ = daemon.account(&draft).await?.cancel_sign_in().await;
        anyhow::Ok(())
    };
    let (outcome, message) = tokio::select! {
        result = tokio::time::timeout(SIGN_IN_WAIT, ended) => match result {
            Ok(ended) => ended?,
            Err(_) => {
                cancel().await?;
                bail!("timed out");
            }
        },
        _ = tokio::signal::ctrl_c() => {
            cancel().await?;
            bail!("cancelled");
        }
    };
    match added_text(&outcome, &message) {
        Ok(said) => print!("{said}"),
        Err(why) => bail!("{why}"),
    }
    Ok(())
}
