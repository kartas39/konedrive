use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use konedrive_dbus::{Account1Proxy, Sync1Proxy};
use konedrivectl::SyncAction;

#[derive(Parser)]
#[command(name = "konedrivectl", version, about = "Control the KOneDrive daemon")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Save the Application (client) ID of your Microsoft Entra app registration
    SetClientId { id: String },
    /// Sign in with your Microsoft account in the browser
    Login,
    /// Sign out and delete the stored token
    Logout,
    /// Show the account state
    Status,
    /// Work with the sync folder
    Sync {
        #[command(subcommand)]
        command: SyncCmd,
    },
}

#[derive(Subcommand)]
enum SyncCmd {
    /// Bind an empty folder to the signed-in account
    Register { path: String },
    /// Bind a folder with NOTHING intercepting opens inside it: placeholders
    /// read as zeros until you `hydrate` them by hand. Needs no privileged
    /// helper, so this is the path that works on your own machine — the
    /// real helper only ever runs inside a VM. Named after the D-Bus method
    /// it calls (`RegisterRootWithoutInterception`) rather than something
    /// shorter, on purpose: the cost this mode carries belongs in the word
    /// you type, not just in a warning you might scroll past.
    RegisterWithoutInterception { path: String },
    /// Forget the folder (local files are left as they are)
    Forget,
    /// Fill the folder with placeholders mirroring a local directory
    PopulateFrom { source_dir: String },
    /// Download one file now
    Hydrate { path: String },
    /// Free up space for one file
    Dehydrate { path: String },
    /// Print one file's state
    State { path: String },
    /// Print the folder's state
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let connection = zbus::Connection::session()
        .await
        .context("cannot connect to the session bus")?;
    let proxy = Account1Proxy::new(&connection).await?;
    match cli.command {
        Cmd::SetClientId { id } => {
            proxy.set_client_id(&id).await?;
            println!("Client ID saved.");
        }
        Cmd::Login => login(&connection, &proxy).await?,
        Cmd::Logout => {
            proxy.sign_out().await?;
            println!("Signed out.");
        }
        Cmd::Status => print!("{}", konedrivectl::status_text(&proxy).await?),
        Cmd::Sync { command } => sync(&connection, command).await?,
    }
    Ok(())
}

async fn sync(connection: &zbus::Connection, command: SyncCmd) -> anyhow::Result<()> {
    let proxy = Sync1Proxy::new(connection).await?;
    match command {
        SyncCmd::Register { path } => {
            let absolute = absolute_str(&path)?;
            let action = SyncAction::Register(&absolute);
            explained(&proxy, action, proxy.register_root(&absolute).await).await?;
            match root_trouble(&proxy).await? {
                None => println!("Folder registered: {absolute}"),
                // `SyncService::register_root`'s own doc comment says the
                // call still returns `Ok(())` here (the root itself is
                // usable) — but printing "registered" with nothing else
                // would be C1: the one thing this interface exists to make
                // visible, silently. This is a hard failure (bail, non-zero
                // exit, no bare "registered" line) rather than a warning
                // alongside a success line, because a script checking only
                // the exit status must see the same trouble a human reading
                // stdout would.
                Some(detail) => bail!(
                    "the folder at {absolute} is registered, but was not fully recovered: {detail}"
                ),
            }
        }
        SyncCmd::RegisterWithoutInterception { path } => {
            let absolute = absolute_str(&path)?;
            let action = SyncAction::RegisterWithoutInterception(&absolute);
            let result = proxy.register_root_without_interception(&absolute).await;
            explained(&proxy, action, result).await?;
            match root_trouble(&proxy).await? {
                Some(detail) => bail!(
                    "the folder at {absolute} is registered, but was not fully recovered: {detail}"
                ),
                None => {
                    println!("Folder registered without interception: {absolute}");
                    // Always shown, success or not: the entire point of this
                    // mode is that a placeholder nobody intercepts reads as
                    // zeros, and that must never be left to be inferred.
                    let last_error = proxy.last_error().await?;
                    if !last_error.is_empty() {
                        eprintln!("warning: {last_error}");
                    }
                }
            }
        }
        SyncCmd::Forget => {
            explained(&proxy, SyncAction::Forget, proxy.unregister_root().await).await?;
            println!("Folder forgotten. Local files were left untouched.");
            fail_if_root_unhealthy(&proxy).await?;
        }
        SyncCmd::PopulateFrom { source_dir } => {
            let absolute = absolute_str(&source_dir)?;
            let action = SyncAction::PopulateFrom(&absolute);
            let created =
                explained(&proxy, action, proxy.populate_from_directory(&absolute).await).await?;
            println!("Created {created} placeholders.");
            fail_if_root_unhealthy(&proxy).await?;
        }
        SyncCmd::Hydrate { path } => {
            let absolute = absolute_str(&path)?;
            let action = SyncAction::Hydrate(&absolute);
            explained(&proxy, action, proxy.hydrate(&absolute).await).await?;
            println!("Downloaded.");
            fail_if_root_unhealthy(&proxy).await?;
        }
        SyncCmd::Dehydrate { path } => {
            let absolute = absolute_str(&path)?;
            let action = SyncAction::Dehydrate(&absolute);
            explained(&proxy, action, proxy.dehydrate(&absolute).await).await?;
            println!("Freed up.");
            fail_if_root_unhealthy(&proxy).await?;
        }
        SyncCmd::State { path } => println!("{}", proxy.item_state(&absolute_str(&path)?).await?),
        SyncCmd::Status => print!("{}", konedrivectl::sync_status_text(&proxy).await?),
    }
    Ok(())
}

/// Passes a `Sync1` call's result through, turning a refusal into what the
/// person running this should read (`konedrivectl::explain_sync_error`):
/// matched by the D-Bus error name, and said in terms of their own file.
async fn explained<T>(
    proxy: &Sync1Proxy<'_>,
    action: SyncAction<'_>,
    result: zbus::Result<T>,
) -> anyhow::Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            // Two refusals name the registered folder. If even reading it
            // fails, they simply do not; the refusal is the thing to report.
            let root = proxy.root_path().await.unwrap_or_default();
            Err(anyhow::anyhow!("{}", konedrivectl::explain_sync_error(action, &error, &root)))
        }
    }
}

/// `Some(detail)` when the D-Bus call just made left (or found) the root in
/// `RootState = error` — `LastError` is the detail. Every mutating command
/// re-checks this after its own call succeeds: any of them can run while the
/// root is already unhealthy (a helper that dropped, a recovery that could
/// not finish), and that must not be left out of the command's own exit
/// status just because the specific thing it asked for went through.
async fn root_trouble(proxy: &Sync1Proxy<'_>) -> anyhow::Result<Option<String>> {
    if proxy.root_state().await? == "error" {
        Ok(Some(proxy.last_error().await?))
    } else {
        Ok(None)
    }
}

/// Bails with the detail when [`root_trouble`] finds one. Used after the
/// four commands whose own success message is still true regardless (a file
/// really was hydrated, a folder really was forgotten) but where the root as
/// a whole may still need attention.
async fn fail_if_root_unhealthy(proxy: &Sync1Proxy<'_>) -> anyhow::Result<()> {
    if let Some(detail) = root_trouble(proxy).await? {
        bail!("the sync root needs attention: {detail}");
    }
    Ok(())
}

/// The daemon only accepts absolute paths; resolve here so relative ones work.
fn absolute_str(path: &str) -> anyhow::Result<String> {
    // The final review's m8: the directory the name is in is resolved, the
    // name itself is not. Canonicalising the whole path resolved a symbolic
    // link given as the last component, so `sync register <link>` silently
    // registered the link's target, while spec §10 refuses a link as a
    // root; the daemon opens every path it is handed with `O_NOFOLLOW`, and
    // it has to be handed the link to refuse it.
    let given = std::path::Path::new(path);
    std::fs::symlink_metadata(given).with_context(|| format!("no such path: {path}"))?;
    let absolute = match (given.parent(), given.file_name()) {
        (Some(parent), Some(name)) => {
            let parent = if parent.as_os_str().is_empty() { std::path::Path::new(".") } else { parent };
            std::fs::canonicalize(parent)
                .with_context(|| format!("no such path: {path}"))?
                .join(name)
        }
        // `/`, `.`, `..` and the like: no last name to keep.
        _ => std::fs::canonicalize(given).with_context(|| format!("no such path: {path}"))?,
    };
    absolute.to_str().map(str::to_owned).context("non-UTF-8 path")
}

async fn login(connection: &zbus::Connection, proxy: &Account1Proxy<'_>) -> anyhow::Result<()> {
    let url = proxy.begin_sign_in().await?;
    println!("Opening the Microsoft sign-in page in your browser. If it does not open, visit:\n\n  {url}\n");
    let _ = Command::new("xdg-open")
        .arg(&url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    // An uncached proxy: the wait loop polls `State` directly rather than watching
    // `StateChanged`, so it never misses a transition the signal stream coalesced away.
    let wait_proxy = Account1Proxy::builder(connection)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;

    tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(6 * 60), konedrivectl::wait_for_sign_in(&wait_proxy)) => {
            result.context("timed out")??;
        }
        _ = tokio::signal::ctrl_c() => {
            proxy.cancel_sign_in().await?;
            bail!("cancelled");
        }
    }
    println!("Signed in.");
    Ok(())
}
