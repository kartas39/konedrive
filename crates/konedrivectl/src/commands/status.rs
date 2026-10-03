use konedrivectl::FIRST_LABEL;

use crate::daemon::Daemon;

pub(crate) async fn status(daemon: &Daemon, option: Option<&str>) -> anyhow::Result<()> {
    let client_id = daemon.manager.client_id().await?;
    let trouble = daemon.manager.last_error().await?;
    let trouble = if trouble.is_empty() { String::new() } else { format!("{:<12}{trouble}\n", "Problem:") };
    let (accounts, alone, _) = daemon.shown(option).await?;
    if alone {
        let proxy = daemon.account(&accounts[0].path).await?;
        print!("{}{trouble}", konedrivectl::status_text(&proxy, Some(&client_id)).await?);
        return Ok(());
    }
    let mut out = format!("{}{trouble}", konedrivectl::client_id_line(&client_id));
    if accounts.is_empty() {
        out.push_str(&format!(
            "{:<12}none yet: `konedrivectl login` adds one called {FIRST_LABEL} and signs it in\n",
            "Accounts:"
        ));
    }
    for account in &accounts {
        let read = async { konedrivectl::status_text(&daemon.account(&account.path).await?, None).await };
        match read.await {
            Ok(block) => out.push_str(&format!("\n{}\n{}", account.label, konedrivectl::indented(&block))),
            // Removed while this ran.
            Err(e) if konedrivectl::is_gone(&e) => {}
            Err(e) => return Err(e.into()),
        }
    }
    print!("{out}");
    Ok(())
}
