use anyhow::{anyhow, bail};
use konedrivectl::text::refusals::{explain_account_error, AccountAction};
use konedrivectl::text::settings::{on_battery_text, on_metered_text};

use crate::cli::SettingsCmd;
use crate::daemon::Daemon;

pub(crate) async fn set_client_id(daemon: &Daemon, id: &str) -> anyhow::Result<()> {
    if let Err(error) = daemon.manager.set_client_id(id).await {
        // The daemon refuses while any account uses the old id: name them.
        let mut busy = Vec::new();
        for account in daemon.accounts().await.unwrap_or_default() {
            let state = match daemon.account(&account.path).await {
                Ok(proxy) => proxy.state().await.unwrap_or_default(),
                Err(_) => continue,
            };
            if state == "signed-in" || state == "signing-in" {
                busy.push(account.label);
            }
        }
        bail!("{}", explain_account_error(AccountAction::SetClientId(id, &busy), &error));
    }
    println!("Client ID saved.");
    Ok(())
}

/// `settings`: the manager's `PauseOnMetered` and `OnBattery`, one pair for every account.
pub(crate) async fn settings(daemon: &Daemon, command: SettingsCmd) -> anyhow::Result<()> {
    let manager = &daemon.manager;
    match command {
        SettingsCmd::OnMetered { choice: None } => println!("{}", on_metered_text(manager.pause_on_metered().await?)),
        SettingsCmd::OnMetered { choice: Some(choice) } => {
            let pause = choice == "pause";
            manager.set_pause_on_metered(pause).await.map_err(|e| anyhow!(explain_account_error(AccountAction::Settings, &e)))?;
            println!("{}", on_metered_text(pause));
        }
        SettingsCmd::OnBattery { choice: None } => println!("{}", on_battery_text(&manager.on_battery().await?)),
        SettingsCmd::OnBattery { choice: Some(choice) } => {
            manager.set_on_battery(&choice).await.map_err(|e| anyhow!(explain_account_error(AccountAction::Settings, &e)))?;
            println!("{}", on_battery_text(&choice));
        }
    }
    Ok(())
}
