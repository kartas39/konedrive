use konedrive_dbus::Refusal;
use konedrivectl::text::status::{account_block, client_id_line, no_account_line, problem_line, status_text};

use crate::daemon::Daemon;
use crate::read;

pub(crate) async fn status(daemon: &Daemon, option: Option<&str>) -> anyhow::Result<()> {
    let client_id = daemon.manager.client_id().await?;
    let trouble = problem_line(&daemon.manager.last_error().await?);
    let shown = daemon.shown(option).await?;
    if shown.alone {
        let account = read::account_status(daemon, &shown.accounts[0].path).await?;
        print!("{}{trouble}", status_text(&account, Some(&client_id)));
        return Ok(());
    }
    let mut out = format!("{}{trouble}", client_id_line(&client_id));
    if shown.accounts.is_empty() {
        out.push_str(&no_account_line());
    }
    for account in &shown.accounts {
        match read::account_status(daemon, &account.path).await {
            Ok(status) => out.push_str(&account_block(&account.label, &status_text(&status, None))),
            // Removed while this ran.
            Err(e) if Refusal::says_gone(&e) => {}
            Err(e) => return Err(e.into()),
        }
    }
    print!("{out}");
    Ok(())
}
