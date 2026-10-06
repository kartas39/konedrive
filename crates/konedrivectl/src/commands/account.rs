use std::time::Duration;

use anyhow::{anyhow, bail, Context as _};
use konedrive_dbus::accounts::AccountProxy;
use konedrive_dbus::Refusal;
use konedrivectl::choice::{choose, Source};
use konedrivectl::text::accounts::{account_list_text, removed_text};
use konedrivectl::text::formats::warning_text;
use konedrivectl::text::refusals::{explain_account_error, explain_sync_error_in, AccountAction, Context, SyncAction};
use konedrivectl::text::status::mode_shown_text;

use super::browser::{open_browser, spawn_browser};
use crate::cli::AccountCmd;
use crate::daemon::Daemon;
use crate::{read, wait};

pub(crate) async fn account(daemon: &Daemon, option: Option<&str>, command: AccountCmd) -> anyhow::Result<()> {
    match command {
        AccountCmd::Mode { mode, force } => return account_mode(daemon, option, mode.as_deref(), force).await,
        AccountCmd::List => {
            let mut rows = Vec::new();
            for path in daemon.manager.list().await? {
                match read::account_row(daemon, &path).await {
                    Ok(row) => rows.push(row),
                    // Removed while this ran.
                    Err(e) if Refusal::says_gone(&e) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            print!("{}", account_list_text(&rows));
            let trouble = daemon.manager.last_error().await?;
            if !trouble.is_empty() {
                eprintln!("{}", warning_text(&trouble));
            }
        }
        AccountCmd::Add => return super::add::add(daemon).await,
        AccountCmd::Rename { named, label } => {
            let accounts = daemon.accounts().await?;
            daemon.check_loaded(&accounts).await?;
            let target = choose(&accounts, Some((&named, Source::Argument)))?;
            let proxy = daemon.account(&target.path).await?;
            let result = proxy.set_label(&label).await;
            let action = AccountAction::Rename(&target.label, &label);
            result.map_err(|e| anyhow!(explain_account_error(action, &e)))?;
            println!("Renamed {} to {}.", target.label, label.trim());
        }
        AccountCmd::Remove { named } => {
            let accounts = daemon.accounts().await?;
            daemon.check_loaded(&accounts).await?;
            let target = choose(&accounts, Some((&named, Source::Argument)))?;
            let sync = daemon.sync(&target.path).await?;
            let folder = sync.folder.path().await.unwrap_or_default();
            // Read first: the list goes with the account. Where each listed file was rescued
            // to is the one thing about rescues this can know (issue #231).
            let conflicts = sync.conflicts.list().await.unwrap_or_default();
            let result = daemon.manager.remove(&target.path.as_ref()).await;
            if let Err(error) = result {
                let helper = daemon.manager.helper_state().await.unwrap_or_default();
                let source = sync.folder.source().await.unwrap_or_default();
                let context = Context { root: &folder, source: &source, helper: &helper, ..Default::default() };
                let action = SyncAction::Remove(&target.label);
                bail!("{}", explain_sync_error_in(action, &error, context));
            }
            print!("{}", removed_text(&target.label, &folder, &conflicts));
        }
    }
    Ok(())
}

/// `account mode` (`docs/design/writes.md` §2.2): the chosen account's mode, or a switch. A switch to
/// read-write opens the sign-in the daemon answers with, as `login` does, and waits until the
/// account is read-write or says why it is not.
async fn account_mode(daemon: &Daemon, option: Option<&str>, mode: Option<&str>, force: bool) -> anyhow::Result<()> {
    let chosen = daemon.chosen(option).await?;
    let (label, tag, prefix) = (chosen.account.label.clone(), chosen.tag(), chosen.prefix());
    let proxy = daemon.account(&chosen.account.path).await?;
    let Some(mode) = mode else {
        print!("{}", mode_shown_text(&tag, &proxy.mode().await?, &proxy.last_error().await?));
        return Ok(());
    };
    let result = proxy.set_mode(mode, force).await;
    let url = result.map_err(|e| anyhow!(explain_account_error(AccountAction::SetMode(&label, mode, &prefix), &e)))?;
    if mode == "read-only" {
        println!("{tag}Read-only: the folder's files are read-only again, and nothing changed in it is uploaded.");
        return Ok(());
    }
    if url.is_empty() {
        println!("{tag}Already read-write.");
        return Ok(());
    }
    if open_browser() {
        println!(
            "Opening the Microsoft sign-in page in your browser, to allow konedrive to change the files \
             of {label} in OneDrive. If it does not open, visit:\n\n  {url}\n"
        );
        spawn_browser(&url);
    } else {
        println!(
            "To allow konedrive to change the files of {label} in OneDrive, sign in on the Microsoft \
             sign-in page. Open this address in a browser:\n\n  {url}\n"
        );
    }
    // Uncached, as `login`'s: the wait polls `Mode` directly.
    let wait_proxy = AccountProxy::builder(&daemon.connection)
        .path(chosen.account.path.clone())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(6 * 60), wait::wait_for_read_write(&wait_proxy)) => {
            result.context("timed out")??;
        }
        _ = tokio::signal::ctrl_c() => {
            proxy.cancel_sign_in().await?;
            bail!("cancelled; {label} stays read-only");
        }
    }
    println!("{tag}Read-write: the folder's files can be changed.");
    Ok(())
}
