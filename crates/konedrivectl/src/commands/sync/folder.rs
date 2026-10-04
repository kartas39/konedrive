//! The `sync` commands that act on the chosen account's folder, through its interfaces. With
//! several accounts, each success line starts with the account's label.

use anyhow::{bail, Context as _};
use konedrive_dbus::accounts::FolderProxies;
use konedrivectl::text::files::{conflicts_text, free_up_text};
use konedrivectl::text::folder as said;
use konedrivectl::text::formats::{parse_duration, warning_text, NOT_ABSOLUTE, NOT_UTF8_PATH};
use konedrivectl::text::refusals::SyncAction;
use konedrivectl::text::transfers::{transfers_text, uploading_line};
use konedrivectl::text::uploads::{not_uploaded_text, outbox_text, quota_text, PER_FILE_SHOWN};

use super::explain::{absolute_str, explained, fail_if_folder_unhealthy, folder_trouble};
use crate::cli::{DeletesCmd, FolderCmd, IgnoreCmd};
use crate::daemon::{Chosen, Daemon};
use crate::{read, Usage};

/// How many changes `sync outbox` shows without `--all`.
const OUTBOX_SHOWN: u32 = 50;

pub(super) async fn folder_command(
    daemon: &Daemon,
    chosen: &Chosen,
    proxy: &FolderProxies<'_>,
    command: FolderCmd,
) -> anyhow::Result<()> {
    let tag = chosen.tag();
    let prefix = chosen.prefix();
    match command {
        FolderCmd::Register { path } => {
            let absolute = absolute_str(&path)?;
            let result = proxy.folder.register(&absolute).await;
            explained(daemon, chosen, proxy, SyncAction::Register(&absolute), result).await?;
            fail_if_not_recovered(proxy, &absolute).await?;
            println!("{tag}{}", said::registered_text(&absolute, true));
        }
        FolderCmd::RegisterWithoutInterception { path } => {
            let absolute = absolute_str(&path)?;
            let result = proxy.folder.register_without_interception(&absolute).await;
            explained(daemon, chosen, proxy, SyncAction::RegisterWithoutInterception(&absolute), result).await?;
            fail_if_not_recovered(proxy, &absolute).await?;
            println!("{tag}{}", said::registered_text(&absolute, false));
            // Shown every time: a placeholder nobody intercepts reads as zeros, and that is
            // never left to be inferred.
            let last_error = proxy.folder.last_error().await?;
            if !last_error.is_empty() {
                eprintln!("{}", warning_text(&last_error));
            }
        }
        FolderCmd::Forget => {
            explained(daemon, chosen, proxy, SyncAction::Forget, proxy.folder.unregister().await).await?;
            println!("{tag}{}", said::FORGOTTEN);
            fail_if_folder_unhealthy(proxy).await?;
        }
        FolderCmd::PopulateFrom { source_dir } => {
            let absolute = absolute_str(&source_dir)?;
            let result = proxy.folder.populate_from_directory(&absolute).await;
            let created = explained(daemon, chosen, proxy, SyncAction::PopulateFrom(&absolute), result).await?;
            println!("{tag}{}", said::populated_text(created));
            fail_if_folder_unhealthy(proxy).await?;
        }
        FolderCmd::Skipped => {
            if proxy.folder.path().await?.is_empty() {
                println!("{}", said::NO_FOLDER);
            } else if proxy.folder.source().await? != "onedrive" {
                println!("{}", said::NOT_ONEDRIVE);
            } else {
                // Read before `Skipped()` itself: a listing that finishes in between means
                // only that the list is a little more complete than the note says.
                let listing = proxy.folder.state().await? == "listing";
                let skipped = explained(daemon, chosen, proxy, SyncAction::Skipped, proxy.folder.skipped().await).await?;
                print!("{}", said::skipped_text(&skipped, listing));
            }
        }
        FolderCmd::Refresh => {
            explained(daemon, chosen, proxy, SyncAction::Refresh, proxy.folder.refresh().await).await?;
            println!("{tag}{}", said::REFRESHED);
            // `Refresh` read the quota again, the account's one: what it read.
            let account = daemon.account(&chosen.account.path).await?;
            let (state, free) = (account.quota_state().await?, account.quota_remaining().await?);
            print!("{}", quota_text(&state, free, proxy.queue.quota_full().await?));
        }
        FolderCmd::Activity { limit } => {
            let events = explained(daemon, chosen, proxy, SyncAction::Activity, proxy.activity.recent(limit).await).await?;
            print!("{}", said::activity_text(&events));
        }
        FolderCmd::Transfers => {
            let summary = read::transfer_summary(proxy).await?;
            let (downloads, uploads) = (proxy.transfers.downloads().await?, proxy.transfers.uploads().await?);
            print!("{}", transfers_text(&summary, &downloads, &uploads));
        }
        FolderCmd::Outbox { all } => {
            let limit = if all { 0 } else { OUTBOX_SHOWN + 1 };
            let mut rows = explained(daemon, chosen, proxy, SyncAction::Outbox, proxy.queue.changes(limit).await).await?;
            let more = !all && rows.len() > OUTBOX_SHOWN as usize;
            if more {
                rows.truncate(OUTBOX_SHOWN as usize);
            }
            println!("{}", uploading_line(&read::transfer_summary(proxy).await?));
            print!("{}", outbox_text(&rows, more, &prefix));
        }
        FolderCmd::Pause { duration } => {
            let seconds = match duration.as_deref() {
                None => 0,
                Some(text) => parse_duration(text).ok_or_else(|| Usage(said::not_a_duration_text(text)))?,
            };
            explained(daemon, chosen, proxy, SyncAction::Pause, proxy.folder.pause(seconds).await).await?;
            let until = if seconds == 0 { None } else { Some(proxy.folder.paused_until().await?) };
            println!("{tag}{}", said::paused_now_text(until, &prefix));
        }
        FolderCmd::Resume => {
            explained(daemon, chosen, proxy, SyncAction::Resume, proxy.folder.resume().await).await?;
            println!("{tag}{}", said::RESUMED);
        }
        FolderCmd::Anyway { .. } => {
            let held = proxy.folder.held_back().await?;
            explained(daemon, chosen, proxy, SyncAction::Anyway, proxy.folder.sync_anyway().await).await?;
            println!("{tag}{}", said::anyway_text(&held));
        }
        FolderCmd::Thumbnails { state } => {
            let on = match state.as_deref() {
                None => proxy.folder.thumbnails().await?,
                Some(state) => {
                    let on = state == "on";
                    explained(daemon, chosen, proxy, SyncAction::Settings, proxy.folder.set_thumbnails(on).await).await?;
                    on
                }
            };
            println!("{tag}{}", said::thumbnails_text(on));
        }
        FolderCmd::Ignore { action } => ignore(daemon, chosen, proxy, action.unwrap_or(IgnoreCmd::List)).await?,
        FolderCmd::NotUploaded { all } => {
            let action = SyncAction::NotUploaded;
            let summary = explained(daemon, chosen, proxy, action, proxy.queue.not_uploaded_summary().await).await?;
            let limit = if all { 0 } else { PER_FILE_SHOWN };
            let mut files = Vec::new();
            for row in summary.iter().filter(|row| all || row.group == "per-file") {
                let result = proxy.queue.not_uploaded_files(&row.reason, limit).await;
                files.push((row.reason.clone(), explained(daemon, chosen, proxy, action, result).await?));
            }
            print!("{}", not_uploaded_text(&summary, &files, &prefix));
        }
        FolderCmd::Deletes { action: DeletesCmd::Confirm } => {
            let count = explained(daemon, chosen, proxy, SyncAction::Deletes, proxy.queue.confirm_deletes().await).await?;
            println!("{tag}{}", said::deletes_confirmed_text(count));
        }
        FolderCmd::Deletes { action: DeletesCmd::Restore } => {
            let count = explained(daemon, chosen, proxy, SyncAction::Deletes, proxy.queue.restore_deletes().await).await?;
            println!("{tag}{}", said::deletes_restored_text(count));
        }
        FolderCmd::Conflicts => {
            let conflicts = explained(daemon, chosen, proxy, SyncAction::Conflicts, proxy.conflicts.list().await).await?;
            print!("{}", conflicts_text(&conflicts));
        }
        FolderCmd::Dismiss { path } => {
            // As the daemon recorded it: made absolute, never resolved. The file may be gone,
            // and a link on the way to it must not change which conflict this names.
            let absolute = std::path::absolute(&path).context(NOT_ABSOLUTE)?;
            let absolute = absolute.to_str().context(NOT_UTF8_PATH)?;
            let result = proxy.conflicts.dismiss(absolute).await;
            explained(daemon, chosen, proxy, SyncAction::Dismiss(absolute), result).await?;
            println!("{tag}{}", said::DISMISSED);
        }
        FolderCmd::FreeUpSpace => {
            let result = proxy.folder.free_up_space().await;
            let freed = explained(daemon, chosen, proxy, SyncAction::FreeUpSpace, result).await?;
            println!("{tag}{}", free_up_text(&freed));
            if proxy.folder.pinned_count().await? > 0 {
                println!("{}", said::PINS_LEFT);
            }
        }
    }
    Ok(())
}

/// A registration the daemon answered `Ok` to can still leave the folder in `error` (it is
/// usable; its recovery did not finish). Then the command fails, with no "registered" line.
async fn fail_if_not_recovered(proxy: &FolderProxies<'_>, path: &str) -> anyhow::Result<()> {
    if let Some(detail) = folder_trouble(proxy).await? {
        bail!("{}", said::not_recovered_text(path, &detail));
    }
    Ok(())
}

/// `sync ignore`: the list, or one pattern added to it or taken off it.
async fn ignore(daemon: &Daemon, chosen: &Chosen, proxy: &FolderProxies<'_>, action: IgnoreCmd) -> anyhow::Result<()> {
    let tag = chosen.tag();
    let mut patterns = proxy.folder.ignore_patterns().await?;
    let done = match action {
        IgnoreCmd::List => {
            for pattern in &patterns {
                println!("{pattern}");
            }
            return Ok(());
        }
        IgnoreCmd::Add { pattern } if patterns.contains(&pattern) => {
            println!("{tag}{}", said::ignored_already_text(&pattern));
            return Ok(());
        }
        IgnoreCmd::Add { pattern } => {
            let done = said::ignore_added_text(&pattern);
            patterns.push(pattern);
            done
        }
        IgnoreCmd::Remove { pattern } => {
            if !patterns.contains(&pattern) {
                return Err(Usage(said::not_ignored_text(&pattern, &chosen.prefix())).into());
            }
            patterns.retain(|p| *p != pattern);
            said::ignore_removed_text(&pattern)
        }
    };
    let refs: Vec<&str> = patterns.iter().map(String::as_str).collect();
    explained(daemon, chosen, proxy, SyncAction::Ignore, proxy.folder.set_ignore_patterns(&refs).await).await?;
    println!("{tag}{done}");
    Ok(())
}
