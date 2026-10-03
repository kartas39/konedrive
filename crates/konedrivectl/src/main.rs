mod cli;
mod commands;
mod daemon;

use std::process::ExitCode;

use anyhow::anyhow;
use clap::Parser;
use konedrivectl::AccountAction;

use cli::{AccountCmd, Cli, Cmd, SyncCmd};
use commands::account::account;
#[cfg(feature = "dev-tools")]
use commands::dev::dev;
use commands::login::login;
use commands::settings::{set_client_id, settings};
use commands::status::status;
use commands::sync::sync;
use commands::version::print_version;
use daemon::Daemon;

/// A command line that has to change: exits with status 2, as clap's own usage errors do.
#[derive(Debug)]
pub(crate) struct Usage(pub(crate) String);

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Usage {}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:?}");
            if error.downcast_ref::<Usage>().is_some() {
                ExitCode::from(2)
            } else if let Some(no_choice) = error.downcast_ref::<konedrivectl::NoChoice>() {
                ExitCode::from(no_choice.exit_status())
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

/// Why `command` takes no `--account`, if it takes none: it names its account itself, acts on
/// every account, or (a path command) acts on the account whose folder holds the path. Given
/// anyway, `--account` is refused rather than ignored, so a mistaken option never silently
/// does something else. `KONEDRIVE_ACCOUNT`, a default for a whole shell, is ignored by them.
fn takes_no_account(command: &Cmd) -> Option<&'static str> {
    match command {
        Cmd::SetClientId { .. } => Some("the client ID is one for every account"),
        Cmd::Settings { .. } => Some("the settings are one for every account"),
        Cmd::Account { command: AccountCmd::List } => Some("`account list` shows every account"),
        Cmd::Account { command: AccountCmd::Add { .. } } => Some("`account add` adds a new account"),
        Cmd::Account { command: AccountCmd::Rename { .. } | AccountCmd::Remove { .. } } => {
            Some("`account rename` and `account remove` take the account as their first argument")
        }
        Cmd::Sync {
            command:
                SyncCmd::Hydrate { .. }
                | SyncCmd::Dehydrate { .. }
                | SyncCmd::State { .. }
                | SyncCmd::Pin { .. }
                | SyncCmd::Unpin { .. }
                | SyncCmd::Free { .. }
                | SyncCmd::Open { .. },
        } => Some("the path decides the account"),
        Cmd::Sync { command: SyncCmd::Anyway { all: true } } => Some("`sync anyway --all` acts on every account"),
        _ => None,
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    if cli.version {
        print_version().await;
        return Ok(());
    }
    let Some(command) = cli.command else {
        return Err(Usage("a command is needed: see konedrivectl --help".to_owned()).into());
    };
    if let (Some(_), Some(why)) = (&cli.account, takes_no_account(&command)) {
        return Err(Usage(format!("{why}: leave out --account")).into());
    }
    let daemon = Daemon::connect().await?;
    let option = cli.account.as_deref();
    match command {
        Cmd::Account { command } => account(&daemon, option, command).await,
        Cmd::SetClientId { id } => set_client_id(&daemon, &id).await,
        Cmd::Settings { command } => settings(&daemon, command).await,
        Cmd::Login => login(&daemon, option).await,
        Cmd::Logout => {
            let chosen = daemon.chosen(option).await?.account;
            let proxy = daemon.account(&chosen.path).await?;
            let result = proxy.sign_out().await;
            result.map_err(|e| anyhow!(konedrivectl::explain_account_error(AccountAction::SignOut(&chosen.label), &e)))?;
            println!("Signed out of {}.", chosen.label);
            Ok(())
        }
        Cmd::Status => status(&daemon, option).await,
        Cmd::Sync { command } => sync(&daemon, option, command).await,
        #[cfg(feature = "dev-tools")]
        Cmd::Dev { command } => dev(&daemon, option, command).await,
    }
}
