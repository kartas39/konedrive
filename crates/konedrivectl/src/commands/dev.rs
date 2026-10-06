use anyhow::{anyhow, Context};
use konedrive_dbus::accounts::{DevToolsProxy, TokenExportProxy};
use konedrivectl::text::formats::shell_word;
use konedrivectl::text::refusals::{explain_account_error, AccountAction};

use crate::cli::DevCmd;
use crate::daemon::Daemon;

/// `dev`: only in a development build, as the daemon's `TokenExport` is.
#[cfg(feature = "dev-tools")]
pub(crate) async fn dev(daemon: &Daemon, option: Option<&str>, command: DevCmd) -> anyhow::Result<()> {
    match command {
        DevCmd::AddAccount { label } => {
            let result = DevToolsProxy::new(&daemon.connection).await?.add_account(&label).await;
            let path = result.map_err(|e| anyhow!(explain_account_error(AccountAction::AddNamed(&label), &e)))?;
            let added = daemon.account(&path).await?;
            let (id, label) = (added.id().await?, added.label().await?);
            println!("Added the account {label} ({id}), signed out and with no folder yet.");
            println!("Sign it in with: konedrivectl --account {} login", shell_word(&label));
        }
        DevCmd::ExportAccessToken { out, read_write } => {
            let chosen = daemon.chosen(option).await?;
            let export = TokenExportProxy::new(&daemon.connection, chosen.account.path.clone()).await?;
            // Read-only unless asked; the daemon refuses a read-write token for any account
            // whose drive `write_test_drive_ids` does not list (`docs/design/writes.md` §2.3; SECURITY.md).
            let token = if read_write { export.read_write().await } else { export.read_only().await };
            let token = token.map_err(|e| anyhow!("{}", konedrivectl::text::refusals::explain_dev_error(&e, &chosen.prefix())))?;
            // `write_secret_atomically` never opens `out` itself, so a symlink there is
            // replaced rather than followed and truncated, and anyone who already had the
            // old file open keeps reading its old content.
            konedrivectl::secret_file::write_secret_atomically(&out, token.as_bytes())
                .with_context(|| format!("cannot write the access token to {}", out.display()))?;
            let what = if read_write { "that can CHANGE its files in OneDrive" } else { "that can only read" };
            println!(
                "Wrote an access token of {} {what}, valid for about an hour, to {}. It is not the \
                 refresh token. Delete the file when the test is done.",
                chosen.account.label,
                out.display()
            );
        }
    }
    Ok(())
}
