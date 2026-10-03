use anyhow::{anyhow, Context};
use konedrive_dbus::accounts::TokenExportProxy;

use crate::cli::DevCmd;
use crate::daemon::Daemon;

/// `dev`: only in a development build, as the daemon's `TokenExport` is (limitations log W11).
#[cfg(feature = "dev-tools")]
pub(crate) async fn dev(daemon: &Daemon, option: Option<&str>, command: DevCmd) -> anyhow::Result<()> {
    match command {
        DevCmd::ExportAccessToken { out, read_write } => {
            let chosen = daemon.chosen(option).await?;
            let export = TokenExportProxy::new(&daemon.connection, chosen.account.path.clone()).await?;
            // Read-only unless asked; the daemon refuses a read-write token for any account
            // the development gate does not let through (`docs/design/writes.md` §8.2; SECURITY.md).
            let token = if read_write { export.read_write().await } else { export.read_only().await };
            let token = token.map_err(|e| anyhow!("{}", konedrivectl::explain_dev_error(&e, &chosen.prefix())))?;
            // I1: `write_secret_atomically` never opens `out`
            // itself, so a symlink there is replaced rather than followed
            // and truncated, and anyone who already had the old file open
            // keeps reading its old content undisturbed.
            konedrivectl::write_secret_atomically(&out, token.as_bytes())
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
