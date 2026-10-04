use anyhow::{anyhow, bail, Context};
use konedrive_dbus::accounts::{FilesProxy, FolderProxies};
use konedrivectl::SyncAction;

use super::browser::{open_browser, spawn_browser};
use crate::cli::{DeletesCmd, IgnoreCmd, SyncCmd};
use crate::daemon::{holder, variable_set, Chosen, Daemon};
use crate::Usage;

async fn sync_status(daemon: &Daemon, option: Option<&str>) -> anyhow::Result<()> {
    let helper = daemon.manager.helper_state().await?;
    let (accounts, alone, several) = daemon.shown(option).await?;
    let variable = variable_set();
    if alone {
        let sync = daemon.sync(&accounts[0].path).await?;
        let prefix = konedrivectl::command_prefix(Some(&accounts[0].label), several, variable);
        print!("{}", konedrivectl::sync_status_text(&sync, Some(&helper), &prefix).await?);
        return Ok(());
    }
    daemon.check_loaded(&accounts).await?;
    let mut out = konedrivectl::helper_line(&helper);
    if accounts.is_empty() {
        out.push_str(&format!("{}\n", konedrivectl::NoChoice::NoAccountYet));
    }
    for account in &accounts {
        let prefix = konedrivectl::command_prefix(Some(&account.label), several, variable);
        let read = async { konedrivectl::sync_status_text(&daemon.sync(&account.path).await?, None, &prefix).await };
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
                println!(
                    "{tag}Syncing anyway ({}) until the connection, the battery or the power profile changes.",
                    konedrivectl::held_text(&held)
                );
                lifted += 1;
            }
            Ok(None) => {}
            // Removed while this ran.
            Err(e) if konedrivectl::is_gone(&e) => {}
            Err(e) => return Err(anyhow!("{}: {e}", account.label)),
        }
    }
    if lifted == 0 {
        println!("No account is paused by itself: nothing to lift.");
    }
    Ok(())
}

pub(crate) async fn sync(daemon: &Daemon, option: Option<&str>, command: SyncCmd) -> anyhow::Result<()> {
    match command {
        SyncCmd::Status => return sync_status(daemon, option).await,
        SyncCmd::Anyway { all: true } => return sync_anyway_all(daemon).await,
        SyncCmd::Hydrate { path } => {
            let absolute = absolute_str(&path)?;
            let files = FilesProxy::new(&daemon.connection).await?;
            let paths = [absolute.clone()];
            let result = files.hydrate(&absolute).await;
            explained_paths(daemon, PathAction::Hydrate, &paths, result).await?;
            println!("Downloaded.");
            fail_if_holders_unhealthy(daemon, &paths).await?;
        }
        SyncCmd::Dehydrate { path } => {
            let absolute = absolute_str(&path)?;
            let files = FilesProxy::new(&daemon.connection).await?;
            let paths = [absolute.clone()];
            let result = files.dehydrate(&absolute).await;
            explained_paths(daemon, PathAction::Dehydrate, &paths, result).await?;
            println!("Freed up.");
            fail_if_holders_unhealthy(daemon, &paths).await?;
        }
        SyncCmd::State { path } => {
            let files = FilesProxy::new(&daemon.connection).await?;
            println!("{}", files.item_state(&absolute_str(&path)?).await?);
        }
        SyncCmd::Pin { paths } => {
            let absolute = absolute_all(&paths)?;
            let refs: Vec<&str> = absolute.iter().map(String::as_str).collect();
            let files = FilesProxy::new(&daemon.connection).await?;
            let result = files.pin(&refs).await;
            let queued = explained_paths(daemon, PathAction::Pin, &absolute, result).await?;
            // `sync transfers` is one account's: the one the paths are in, if they are in one.
            let (folders, accounts) = daemon.folders().await?;
            let mut labels: Vec<&str> =
                absolute.iter().filter_map(|path| holder(&folders, path)).map(|f| f.label.as_str()).collect();
            labels.dedup();
            let label = match labels.as_slice() {
                [one] => Some(*one),
                _ => None,
            };
            let prefix = konedrivectl::command_prefix(label, accounts > 1, variable_set());
            println!("{}", konedrivectl::pin_text(queued, &prefix));
            fail_if_holders_unhealthy(daemon, &absolute).await?;
        }
        SyncCmd::Unpin { paths } => {
            let absolute = absolute_all(&paths)?;
            let refs: Vec<&str> = absolute.iter().map(String::as_str).collect();
            let files = FilesProxy::new(&daemon.connection).await?;
            let result = files.unpin(&refs).await;
            let unpinned = explained_paths(daemon, PathAction::Unpin, &absolute, result).await?;
            println!("{}", konedrivectl::unpin_text(unpinned));
            fail_if_holders_unhealthy(daemon, &absolute).await?;
        }
        SyncCmd::Free { paths } => {
            let absolute = absolute_all(&paths)?;
            let refs: Vec<&str> = absolute.iter().map(String::as_str).collect();
            let files = FilesProxy::new(&daemon.connection).await?;
            let result = files.free_up(&refs).await;
            let (freed, bytes, busy, pinned) = explained_paths(daemon, PathAction::Free, &absolute, result).await?;
            println!("{}", konedrivectl::free_text(freed, bytes, busy, pinned));
            fail_if_holders_unhealthy(daemon, &absolute).await?;
        }
        SyncCmd::Open { path, print } => {
            let absolute = absolute_str(&path)?;
            let files = FilesProxy::new(&daemon.connection).await?;
            let result = files.web_url(&absolute).await;
            let url = explained_paths(daemon, PathAction::Open, std::slice::from_ref(&absolute), result).await?;
            println!("{url}");
            // Only an address of the web is handed to the opener, whatever answered.
            if !print && open_browser() && url.starts_with("https://") {
                spawn_browser(&url);
            }
        }
        command => {
            let chosen = daemon.chosen(option).await?;
            let proxy = daemon.sync(&chosen.account.path).await?;
            folder_command(daemon, &chosen, &proxy, command).await?;
        }
    }
    Ok(())
}

/// The `sync` commands that act on the chosen account's folder, through its interfaces. With
/// several accounts, each success line starts with the account's label.
async fn folder_command(daemon: &Daemon, chosen: &Chosen, proxy: &FolderProxies<'_>, command: SyncCmd) -> anyhow::Result<()> {
    let tag = chosen.tag();
    match command {
        SyncCmd::Register { path } => {
            let absolute = absolute_str(&path)?;
            let action = SyncAction::Register(&absolute);
            explained(daemon, chosen, proxy, action, proxy.folder.register(&absolute).await).await?;
            match root_trouble(proxy).await? {
                None => println!("{tag}Folder registered: {absolute}"),
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
            let result = proxy.folder.register_without_interception(&absolute).await;
            explained(daemon, chosen, proxy, action, result).await?;
            match root_trouble(proxy).await? {
                Some(detail) => bail!(
                    "the folder at {absolute} is registered, but was not fully recovered: {detail}"
                ),
                None => {
                    println!("{tag}Folder registered without interception: {absolute}");
                    // Always shown, success or not: the entire point of this
                    // mode is that a placeholder nobody intercepts reads as
                    // zeros, and that must never be left to be inferred.
                    let last_error = proxy.folder.last_error().await?;
                    if !last_error.is_empty() {
                        eprintln!("warning: {last_error}");
                    }
                }
            }
        }
        SyncCmd::Forget => {
            explained(daemon, chosen, proxy, SyncAction::Forget, proxy.folder.unregister().await).await?;
            println!("{tag}Folder forgotten. Local files were left untouched.");
            fail_if_root_unhealthy(proxy).await?;
        }
        SyncCmd::PopulateFrom { source_dir } => {
            let absolute = absolute_str(&source_dir)?;
            let action = SyncAction::PopulateFrom(&absolute);
            let created = explained(daemon, chosen, proxy, action, proxy.folder.populate_from_directory(&absolute).await).await?;
            println!("{tag}Created {created} placeholders.");
            fail_if_root_unhealthy(proxy).await?;
        }
        SyncCmd::Skipped => {
            let root_path = proxy.folder.path().await?;
            if root_path.is_empty() {
                println!("No folder is registered.");
            } else if proxy.folder.source().await? != "onedrive" {
                println!("This folder is not connected to OneDrive.");
            } else {
                // Read before `Skipped()` itself: a listing that finishes in
                // between just means the list this call gets back is a
                // little more complete than the note says, never less.
                let still_listing = proxy.folder.state().await? == "listing";
                let skipped = explained(daemon, chosen, proxy, SyncAction::Skipped, proxy.folder.skipped().await).await?;
                if still_listing {
                    println!(
                        "The folder is still being filled from OneDrive; this list may be partial."
                    );
                }
                if skipped.is_empty() {
                    println!("Nothing is skipped.");
                }
                for (path, reason) in skipped {
                    println!("{path}\n    {}", konedrivectl::skip_reason_text(&reason));
                }
            }
        }
        SyncCmd::Refresh => {
            explained(daemon, chosen, proxy, SyncAction::Refresh, proxy.folder.refresh().await).await?;
            println!("{tag}Asked OneDrive for changes.");
            // `Refresh` read the quota again, the account's one: what it read.
            let account = daemon.account(&chosen.account.path).await?;
            let (state, free, full) = (account.quota_state().await?, account.quota_remaining().await?, proxy.queue.quota_full().await?);
            print!("{}", konedrivectl::quota_text(&state, free, full));
        }
        SyncCmd::Activity { limit } => {
            let events = explained(daemon, chosen, proxy, SyncAction::Activity, proxy.activity.recent(limit).await).await?;
            print!("{}", konedrivectl::activity_text(&events));
        }
        SyncCmd::Transfers => {
            let summary = konedrivectl::transfer_summary(proxy).await?;
            print!("{}", konedrivectl::transfers_text(&summary, &proxy.transfers.downloads().await?, &proxy.transfers.uploads().await?));
        }
        SyncCmd::Outbox { all } => {
            const SHOWN: u32 = 50;
            let limit = if all { 0 } else { SHOWN + 1 };
            let mut rows = explained(daemon, chosen, proxy, SyncAction::Outbox, proxy.queue.changes(limit).await).await?;
            let more = !all && rows.len() > SHOWN as usize;
            rows.truncate(if all { rows.len() } else { SHOWN as usize });
            println!("{}", konedrivectl::uploading_line(&konedrivectl::transfer_summary(proxy).await?));
            print!("{}", konedrivectl::outbox_text(&rows, more, &chosen.prefix()));
        }
        SyncCmd::Pause { duration } => {
            let seconds = match duration.as_deref() {
                None => 0,
                Some(text) => konedrivectl::parse_duration(text)
                    .ok_or_else(|| Usage(format!("`{text}` is not a duration: write it as 30m, 2h, 1d or 1h30m")))?,
            };
            explained(daemon, chosen, proxy, SyncAction::Pause, proxy.folder.pause(seconds).await).await?;
            match seconds {
                0 => println!("{tag}Paused until `{} sync resume`.", chosen.prefix()),
                _ => println!("{tag}Paused until {}.", konedrivectl::local_time(proxy.folder.paused_until().await?)),
            }
        }
        SyncCmd::Resume => {
            explained(daemon, chosen, proxy, SyncAction::Resume, proxy.folder.resume().await).await?;
            println!("{tag}Resumed.");
        }
        SyncCmd::Anyway { .. } => {
            let held = proxy.folder.held_back().await?;
            explained(daemon, chosen, proxy, SyncAction::Anyway, proxy.folder.sync_anyway().await).await?;
            match held.as_str() {
                "" => println!("{tag}Not paused by itself: nothing to lift."),
                reason => println!(
                    "{tag}Syncing anyway ({}) until the connection, the battery or the power profile changes.",
                    konedrivectl::held_text(reason)
                ),
            }
        }
        SyncCmd::Thumbnails { state } => match state.as_deref() {
            None => println!("{tag}{}", konedrivectl::thumbnails_text(proxy.folder.thumbnails().await?)),
            Some(state) => {
                let on = state == "on";
                explained(daemon, chosen, proxy, SyncAction::Settings, proxy.folder.set_thumbnails(on).await).await?;
                println!("{tag}{}", konedrivectl::thumbnails_text(on));
            }
        },
        SyncCmd::Ignore { action } => {
            let patterns = proxy.folder.ignore_patterns().await?;
            let changed = match action {
                None | Some(IgnoreCmd::List) => {
                    for pattern in &patterns {
                        println!("{pattern}");
                    }
                    None
                }
                Some(IgnoreCmd::Add { pattern }) if patterns.contains(&pattern) => {
                    println!("{tag}`{pattern}` is on the list already.");
                    None
                }
                Some(IgnoreCmd::Add { pattern }) => {
                    let mut new = patterns.clone();
                    new.push(pattern.clone());
                    Some((new, format!("{tag}Added `{pattern}`: local files named so are not uploaded.")))
                }
                Some(IgnoreCmd::Remove { pattern }) => {
                    if !patterns.contains(&pattern) {
                        return Err(Usage(format!("`{pattern}` is not on the list (`{} sync ignore list`)", chosen.prefix())).into());
                    }
                    let new: Vec<String> = patterns.iter().filter(|p| **p != pattern).cloned().collect();
                    Some((new, format!("{tag}Removed `{pattern}`: local files named so are uploaded from now on.")))
                }
            };
            if let Some((new, said)) = changed {
                let refs: Vec<&str> = new.iter().map(String::as_str).collect();
                explained(daemon, chosen, proxy, SyncAction::Ignore, proxy.folder.set_ignore_patterns(&refs).await).await?;
                println!("{said}");
            }
        }
        SyncCmd::NotUploaded { all } => {
            let summary = explained(daemon, chosen, proxy, SyncAction::NotUploaded, proxy.queue.not_uploaded_summary().await).await?;
            let limit = if all { 0 } else { konedrivectl::PER_FILE_SHOWN };
            let mut files = Vec::new();
            for (group, reason, _, _) in &summary {
                if all || group == "per-file" {
                    let (items, total) =
                        explained(daemon, chosen, proxy, SyncAction::NotUploaded, proxy.queue.not_uploaded_files(reason, limit).await).await?;
                    files.push((reason.clone(), items, total));
                }
            }
            print!("{}", konedrivectl::not_uploaded_text(&summary, &files, &chosen.prefix()));
        }
        SyncCmd::Deletes { action } => match action {
            DeletesCmd::Confirm => {
                let n = explained(daemon, chosen, proxy, SyncAction::Deletes, proxy.queue.confirm_deletes().await).await?;
                match n {
                    0 => println!("{tag}No delete is waiting for confirmation."),
                    n => println!("{tag}Confirmed: {n} change(s) go to OneDrive's recycle bin."),
                }
            }
            DeletesCmd::Restore => {
                let n = explained(daemon, chosen, proxy, SyncAction::Deletes, proxy.queue.restore_deletes().await).await?;
                match n {
                    0 => println!("{tag}No delete is waiting for confirmation."),
                    n => println!("{tag}Restored: {n} change(s) dropped; the items come back from OneDrive."),
                }
            }
        },
        SyncCmd::Conflicts => {
            let conflicts = explained(daemon, chosen, proxy, SyncAction::Conflicts, proxy.conflicts.list().await).await?;
            print!("{}", konedrivectl::conflicts_text(&conflicts));
        }
        SyncCmd::Dismiss { path } => {
            // As the daemon recorded it: made absolute, never resolved — the
            // file may be gone, and a link on the way to it must not change
            // which conflict this names.
            let absolute = std::path::absolute(&path).context("cannot make the path absolute")?;
            let absolute = absolute.to_str().context("non-UTF-8 path")?;
            let action = SyncAction::Dismiss(absolute);
            explained(daemon, chosen, proxy, action, proxy.conflicts.dismiss(absolute).await).await?;
            println!("{tag}Dismissed. The file was left where it is.");
        }
        SyncCmd::FreeUpSpace => {
            let (files, bytes, busy) =
                explained(daemon, chosen, proxy, SyncAction::FreeUpSpace, proxy.folder.free_up_space().await).await?;
            println!("{tag}{}", konedrivectl::free_up_text(files, bytes, busy));
            if proxy.folder.pinned_count().await? > 0 {
                println!("Files kept on this device (`konedrivectl sync pin`) were left as they are.");
            }
        }
        SyncCmd::Status
        | SyncCmd::Hydrate { .. }
        | SyncCmd::Dehydrate { .. }
        | SyncCmd::State { .. }
        | SyncCmd::Pin { .. }
        | SyncCmd::Unpin { .. }
        | SyncCmd::Free { .. }
        | SyncCmd::Open { .. } => unreachable!("handled by `sync`"),
    }
    Ok(())
}

/// [`absolute_str`] for each of `paths`.
fn absolute_all(paths: &[String]) -> anyhow::Result<Vec<String>> {
    paths.iter().map(|path| absolute_str(path)).collect()
}

/// Passes a folder call's result through, turning a refusal into what the
/// person running this should read (`konedrivectl::explain_sync_error`):
/// matched by the D-Bus error name, and said in terms of their own file.
async fn explained<T>(
    daemon: &Daemon,
    chosen: &Chosen,
    proxy: &FolderProxies<'_>,
    action: SyncAction<'_>,
    result: zbus::Result<T>,
) -> anyhow::Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            // Two refusals name the registered folder, one depends on what
            // it shows, and one ends with how to start the helper. If even
            // reading them fails, they simply do not; the refusal is the
            // thing to report.
            let root = proxy.folder.path().await.unwrap_or_default();
            let source = proxy.folder.source().await.unwrap_or_default();
            let helper = daemon.manager.helper_state().await.unwrap_or_default();
            let foreign = match action {
                SyncAction::Register(path) | SyncAction::RegisterWithoutInterception(path) => carries_a_drive(path),
                _ => false,
            };
            let prefix = chosen.prefix();
            let context = konedrivectl::Context {
                root: &root,
                source: &source,
                helper: &helper,
                folders: &[],
                foreign,
                prefix: &prefix,
            };
            Err(anyhow!("{}", konedrivectl::explain_sync_error_in(action, &error, context)))
        }
    }
}

/// Passes a `Files` call's result on `paths` through, as [`explained`] does. `Files` finds
/// the account by the path, so the refusal is explained against the folder that holds the
/// path it is about — the one a `NotAllowed` names, else the first in no folder, else the
/// first given — and, for a path in none, against every account's folder.
async fn explained_paths<T>(
    daemon: &Daemon,
    action: PathAction,
    paths: &[String],
    result: zbus::Result<T>,
) -> anyhow::Result<T> {
    let error = match result {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    let (folders, accounts) = daemon.folders().await.unwrap_or_default();
    let refused = konedrivectl::refused_path(&error).map(str::to_owned).or_else(|| {
        let outside = konedrive_dbus::Refusal::from_error(&error) == Some(konedrive_dbus::Refusal::OutsideRoot);
        paths.iter().find(|path| outside && holder(&folders, path).is_none()).cloned()
    });
    let named = refused.clone().unwrap_or_else(|| paths.join(", "));
    let about = refused.as_deref().or(paths.first().map(String::as_str)).unwrap_or_default();
    let held_by = holder(&folders, about);
    let root = held_by.map(|f| f.root.clone()).unwrap_or_default();
    let source = match held_by {
        Some(folder) => folder.sync.folder.source().await.unwrap_or_default(),
        None => String::new(),
    };
    let helper = daemon.manager.helper_state().await.unwrap_or_default();
    let roots: Vec<String> = folders.iter().map(|f| f.root.clone()).collect();
    // A command suggested about the folder that holds the path names its account; about
    // a path in none, `<account>` stands in.
    let prefix = konedrivectl::command_prefix(held_by.map(|f| f.label.as_str()), accounts > 1, variable_set());
    let context = konedrivectl::Context {
        root: &root,
        source: &source,
        helper: &helper,
        folders: &roots,
        foreign: false,
        prefix: &prefix,
    };
    Err(anyhow!("{}", konedrivectl::explain_sync_error_in(action.about(&named), &error, context)))
}

/// A `Files` call, for [`explained_paths`].
#[derive(Clone, Copy)]
enum PathAction {
    Hydrate,
    Dehydrate,
    Pin,
    Unpin,
    Free,
    Open,
}

impl PathAction {
    /// The call, about `named`: the path its refusal is about, or the paths given.
    fn about(self, named: &str) -> SyncAction<'_> {
        match self {
            PathAction::Hydrate => SyncAction::Hydrate(named),
            PathAction::Dehydrate => SyncAction::Dehydrate(named),
            PathAction::Pin => SyncAction::Pin(named),
            PathAction::Unpin => SyncAction::Unpin(named),
            PathAction::Free => SyncAction::Free(named),
            PathAction::Open => SyncAction::Open(named),
        }
    }
}

/// Whether the folder at `path` carries a drive (`user.konedrive.drive`, design §8.3): a
/// registration of it refused `NotEmpty` was refused because it is another account's
/// folder. An empty value is no drive, as the daemon reads it. The link itself, if it is
/// one: the daemon refuses links anyway.
fn carries_a_drive(path: &str) -> bool {
    let (Ok(path), Ok(name)) = (std::ffi::CString::new(path), std::ffi::CString::new("user.konedrive.drive")) else {
        return false;
    };
    // SAFETY: both are NUL-terminated strings that live across the call; a null buffer of
    // size 0 asks only for the value's size.
    unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) > 0 }
}

/// `Some(detail)` when the D-Bus call just made left (or found) the root in
/// `Folder.State = error` — `LastError` is the detail. Every mutating command
/// re-checks this after its own call succeeds: any of them can run while the
/// root is already unhealthy (a helper that dropped, a recovery that could
/// not finish), and that must not be left out of the command's own exit
/// status just because the specific thing it asked for went through.
async fn root_trouble(proxy: &FolderProxies<'_>) -> anyhow::Result<Option<String>> {
    if proxy.folder.state().await? == "error" {
        Ok(Some(proxy.folder.last_error().await?))
    } else {
        Ok(None)
    }
}

/// Bails with the detail when [`root_trouble`] finds one. Used after the
/// commands whose own success message is still true regardless (a file
/// really was hydrated, a folder really was forgotten) but where the root as
/// a whole may still need attention.
async fn fail_if_root_unhealthy(proxy: &FolderProxies<'_>) -> anyhow::Result<()> {
    if let Some(detail) = root_trouble(proxy).await? {
        bail!("the sync root needs attention: {detail}");
    }
    Ok(())
}

/// [`fail_if_root_unhealthy`] for every folder that holds one of `paths`.
async fn fail_if_holders_unhealthy(daemon: &Daemon, paths: &[String]) -> anyhow::Result<()> {
    let (folders, _) = daemon.folders().await?;
    let mut checked: Vec<&str> = Vec::new();
    for folder in paths.iter().filter_map(|path| holder(&folders, path)) {
        if checked.contains(&folder.root.as_str()) {
            continue;
        }
        checked.push(&folder.root);
        if let Some(detail) = root_trouble(&folder.sync).await? {
            bail!("the sync folder {} needs attention: {detail}", folder.root);
        }
    }
    Ok(())
}

/// The daemon only accepts absolute paths; resolve here so relative ones work.
fn absolute_str(path: &str) -> anyhow::Result<String> {
    // the directory the name is in is resolved, the
    // name itself is not. Canonicalising the whole path resolved a symbolic
    // link given as the last component, so `sync register <link>` silently
    // registered the link's target, while refuses a link as a
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
