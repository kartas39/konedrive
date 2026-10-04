//! `sync`: `status`, the commands on the chosen account's folder (`folder.rs`) and the
//! commands that take a path (`path.rs`).

mod explain;
mod folder;
mod path;

use anyhow::anyhow;
use konedrive_dbus::Refusal;
use konedrivectl::choice::NoChoice;
use konedrivectl::text::folder::{anyway_text, NOTHING_TO_LIFT};
use konedrivectl::text::formats::unix_now;
use konedrivectl::text::status::{account_block, helper_line, sync_status_text};

use crate::cli::{FolderCmd, SyncCmd};
use crate::daemon::Daemon;
use crate::read;

pub(crate) async fn sync(daemon: &Daemon, option: Option<&str>, command: SyncCmd) -> anyhow::Result<()> {
    match command {
        SyncCmd::Status => sync_status(daemon, option).await,
        SyncCmd::Folder(FolderCmd::Anyway { all: true }) => sync_anyway_all(daemon).await,
        SyncCmd::Folder(command) => {
            let chosen = daemon.chosen(option).await?;
            let proxy = daemon.sync(&chosen.account.path).await?;
            folder::folder_command(daemon, &chosen, &proxy, command).await
        }
        SyncCmd::Path(command) => path::path_command(daemon, command).await,
    }
}

async fn sync_status(daemon: &Daemon, option: Option<&str>) -> anyhow::Result<()> {
    let helper = daemon.manager.helper_state().await?;
    let shown = daemon.shown(option).await?;
    if shown.alone {
        let account = &shown.accounts[0];
        let status = read::folder_status(daemon, &account.path).await?;
        print!("{}", sync_status_text(&status, Some(&helper), &shown.prefix(account), unix_now()));
        return Ok(());
    }
    daemon.check_loaded(&shown.accounts).await?;
    let mut out = helper_line(&helper);
    if shown.accounts.is_empty() {
        out.push_str(&format!("{}\n", NoChoice::NoAccountYet));
    }
    for account in &shown.accounts {
        match read::folder_status(daemon, &account.path).await {
            Ok(status) => {
                let block = sync_status_text(&status, None, &shown.prefix(account), unix_now());
                out.push_str(&account_block(&account.label, &block));
            }
            // Removed while this ran.
            Err(e) if Refusal::says_gone(&e) => {}
            Err(e) => return Err(e.into()),
        }
    }
    print!("{out}");
    Ok(())
}

/// `sync anyway --all`: the hold of every account that holds back by itself and is not paused
/// by the user is lifted — the tray's Sync Anyway (`docs/design/writes.md` §11).
async fn sync_anyway_all(daemon: &Daemon) -> anyhow::Result<()> {
    let accounts = daemon.accounts().await?;
    let several = accounts.len() > 1;
    let mut lifted = 0;
    for account in &accounts {
        let lift = async {
            let proxy = daemon.sync(&account.path).await?;
            let held = proxy.folder.held_back().await?;
            if held.is_empty() || proxy.folder.paused().await? {
                return zbus::Result::Ok(None);
            }
            proxy.folder.sync_anyway().await?;
            Ok(Some(held))
        };
        match lift.await {
            Ok(Some(held)) => {
                let tag = if several { format!("{}: ", account.label) } else { String::new() };
                println!("{tag}{}", anyway_text(&held));
                lifted += 1;
            }
            Ok(None) => {}
            // Removed while this ran.
            Err(e) if Refusal::says_gone(&e) => {}
            Err(e) => return Err(anyhow!("{}: {e}", account.label)),
        }
    }
    if lifted == 0 {
        println!("{NOTHING_TO_LIFT}");
    }
    Ok(())
}
