//! The `sync` commands that take a path: `Files` finds the account by it.

use konedrive_dbus::accounts::FilesProxy;
use konedrivectl::text::files::{folders_unread_text, free_text, pin_text, unpin_text, DOWNLOADED, FREED_UP, PLAIN_PREFIX};
use konedrivectl::text::formats::warning_text;

use super::explain::{absolute_str, explained_paths, fail_if_holders_unhealthy, PathAction};
use crate::cli::PathCmd;
use crate::commands::browser::{open_browser, spawn_browser};
use crate::daemon::Daemon;

pub(super) async fn path_command(daemon: &Daemon, command: PathCmd) -> anyhow::Result<()> {
    match command {
        PathCmd::Hydrate { path } => {
            let call = async |files: &FilesProxy<'_>, paths: &[&str]| files.hydrate(paths[0]).await;
            change(daemon, PathAction::Hydrate, &[path], call, |(), _| DOWNLOADED.to_owned()).await
        }
        PathCmd::Dehydrate { path } => {
            let call = async |files: &FilesProxy<'_>, paths: &[&str]| files.dehydrate(paths[0]).await;
            change(daemon, PathAction::Dehydrate, &[path], call, |(), _| FREED_UP.to_owned()).await
        }
        PathCmd::Pin { paths } => {
            let call = async |files: &FilesProxy<'_>, paths: &[&str]| files.pin(paths).await;
            change(daemon, PathAction::Pin, &paths, call, pin_text).await
        }
        PathCmd::Unpin { paths } => {
            let call = async |files: &FilesProxy<'_>, paths: &[&str]| files.unpin(paths).await;
            change(daemon, PathAction::Unpin, &paths, call, |unpinned, _| unpin_text(unpinned)).await
        }
        PathCmd::Free { paths } => {
            let call = async |files: &FilesProxy<'_>, paths: &[&str]| files.free_up(paths).await;
            change(daemon, PathAction::Free, &paths, call, |freed, _| free_text(&freed)).await
        }
        PathCmd::State { path } => {
            let files = FilesProxy::new(&daemon.connection).await?;
            println!("{}", files.item_state(&absolute_str(&path)?).await?);
            Ok(())
        }
        PathCmd::Open { path, print } => {
            let absolute = absolute_str(&path)?;
            let files = FilesProxy::new(&daemon.connection).await?;
            let result = files.web_url(&absolute).await;
            let url = explained_paths(daemon, PathAction::Open, std::slice::from_ref(&absolute), result).await?;
            println!("{url}");
            // Only an address of the web is handed to the opener, whatever answered.
            if !print && open_browser() && url.starts_with("https://") {
                spawn_browser(&url);
            }
            Ok(())
        }
    }
}

/// The one sequence of the commands that change what is here, by path: the paths `given`
/// are made absolute, `call` is made with them, a refusal is explained against the folder
/// that holds the path it is about, what was done is said (`said`, which is given how a
/// command suggested about these paths starts), and the command fails after all when a
/// folder that holds one of the paths needs attention. Once the call went through, what was
/// done is always said.
async fn change<T>(
    daemon: &Daemon,
    action: PathAction,
    given: &[String],
    call: impl AsyncFnOnce(&FilesProxy<'static>, &[&str]) -> zbus::Result<T>,
    said: impl FnOnce(T, &str) -> String,
) -> anyhow::Result<()> {
    let paths = given.iter().map(|path| absolute_str(path)).collect::<anyhow::Result<Vec<String>>>()?;
    let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    let files = FilesProxy::new(&daemon.connection).await?;
    let result = call(&files, &refs).await;
    let done = explained_paths(daemon, action, &paths, result).await?;
    // The call went through: what was done is said whatever happens next. The folders are
    // read only for the account a suggested command names and for the check afterwards; if
    // they cannot be read, the command still succeeded, and says so with a warning.
    let folders = match daemon.folders().await {
        Ok(folders) => folders,
        Err(error) => {
            println!("{}", said(done, PLAIN_PREFIX));
            eprintln!("{}", warning_text(&folders_unread_text(&error.to_string())));
            return Ok(());
        }
    };
    // A command suggested about the paths (`sync transfers`) is one account's: the one the
    // paths are in, if they are in one.
    let mut holders: Vec<_> = paths.iter().filter_map(|path| folders.holder(path)).collect();
    holders.dedup_by(|a, b| a.root == b.root);
    let one = match holders.as_slice() {
        [one] => Some(*one),
        _ => None,
    };
    println!("{}", said(done, &folders.prefix(one)));
    fail_if_holders_unhealthy(&folders, &paths).await
}
