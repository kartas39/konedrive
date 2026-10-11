//! The `sync` commands that act on the chosen account's folder, through its interfaces. With
//! several accounts, each success line starts with the account's label.

use anyhow::{bail, Context as _};
use konedrive_dbus::accounts::FolderProxies;
use konedrivectl::text::files::{conflicts_text, free_up_text};
use konedrivectl::text::folder as said;
use konedrivectl::text::formats::{parse_duration, NOT_ABSOLUTE, NOT_UTF8_PATH};
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
            explained(chosen, proxy, SyncAction::Register(&absolute), result).await?;
            fail_if_not_recovered(proxy, &absolute).await?;
            println!("{tag}{}", said::registered_text(&absolute));
        }
        FolderCmd::Forget { yes } => {
            // Never refused for what is unsent: the user is told what stays only here, and may leave it.
            if !warn_of_unsent_changes(proxy, yes).await? {
                return Ok(());
            }
            explained(chosen, proxy, SyncAction::Forget, proxy.folder.unregister().await).await?;
            println!("{tag}{}", said::FORGOTTEN);
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
                let skipped = explained(chosen, proxy, SyncAction::Skipped, proxy.folder.skipped().await).await?;
                print!("{}", said::skipped_text(&skipped, listing));
            }
        }
        FolderCmd::Refresh => {
            explained(chosen, proxy, SyncAction::Refresh, proxy.folder.refresh().await).await?;
            println!("{tag}{}", said::REFRESHED);
            // `Refresh` read the quota again, the account's one: what it read.
            let account = daemon.account(&chosen.account.path).await?;
            let (state, free) = (account.quota_state().await?, account.quota_remaining().await?);
            print!("{}", quota_text(&state, free, proxy.queue.quota_full().await?));
        }
        FolderCmd::Activity { limit } => {
            let events = explained(chosen, proxy, SyncAction::Activity, proxy.activity.recent(limit).await).await?;
            print!("{}", said::activity_text(&events));
        }
        FolderCmd::Transfers => {
            let summary = read::transfer_summary(proxy).await?;
            let (downloads, uploads) = (proxy.transfers.downloads().await?, proxy.transfers.uploads().await?);
            print!("{}", transfers_text(&summary, &downloads, &uploads));
        }
        FolderCmd::Outbox { all } => {
            let limit = if all { 0 } else { OUTBOX_SHOWN + 1 };
            let mut rows = explained(chosen, proxy, SyncAction::Outbox, proxy.queue.changes(limit).await).await?;
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
            explained(chosen, proxy, SyncAction::Pause, proxy.folder.pause(seconds).await).await?;
            let until = if seconds == 0 { None } else { Some(proxy.folder.paused_until().await?) };
            println!("{tag}{}", said::paused_now_text(until, &prefix));
        }
        FolderCmd::Resume => {
            explained(chosen, proxy, SyncAction::Resume, proxy.folder.resume().await).await?;
            println!("{tag}{}", said::RESUMED);
        }
        FolderCmd::Anyway { .. } => {
            let held = proxy.folder.held_back().await?;
            explained(chosen, proxy, SyncAction::Anyway, proxy.folder.sync_anyway().await).await?;
            println!("{tag}{}", said::anyway_text(&held));
        }
        FolderCmd::Thumbnails { state } => {
            let on = match state.as_deref() {
                None => proxy.folder.thumbnails().await?,
                Some(state) => {
                    let on = state == "on";
                    explained(chosen, proxy, SyncAction::Settings, proxy.folder.set_thumbnails(on).await).await?;
                    on
                }
            };
            println!("{tag}{}", said::thumbnails_text(on));
        }
        FolderCmd::Ignore { action } => ignore(chosen, proxy, action.unwrap_or(IgnoreCmd::List)).await?,
        FolderCmd::NotUploaded { all } => {
            let action = SyncAction::NotUploaded;
            let summary = explained(chosen, proxy, action, proxy.queue.not_uploaded_summary().await).await?;
            let limit = if all { 0 } else { PER_FILE_SHOWN };
            let mut files = Vec::new();
            for row in summary.iter().filter(|row| all || row.group == "per-file") {
                let result = proxy.queue.not_uploaded_files(&row.reason, limit).await;
                files.push((row.reason.clone(), explained(chosen, proxy, action, result).await?));
            }
            print!("{}", not_uploaded_text(&summary, &files, &prefix));
        }
        FolderCmd::Deletes { action: DeletesCmd::Confirm } => {
            let count = explained(chosen, proxy, SyncAction::Deletes, proxy.queue.confirm_deletes().await).await?;
            println!("{tag}{}", said::deletes_confirmed_text(count));
        }
        FolderCmd::Deletes { action: DeletesCmd::Restore } => {
            let count = explained(chosen, proxy, SyncAction::Deletes, proxy.queue.restore_deletes().await).await?;
            println!("{tag}{}", said::deletes_restored_text(count));
        }
        FolderCmd::Conflicts => {
            let conflicts = explained(chosen, proxy, SyncAction::Conflicts, proxy.conflicts.list().await).await?;
            print!("{}", conflicts_text(&conflicts));
        }
        FolderCmd::Dismiss { path } => {
            // As the daemon recorded it: made absolute, never resolved. The file may be gone,
            // and a link on the way to it must not change which conflict this names.
            let absolute = std::path::absolute(&path).context(NOT_ABSOLUTE)?;
            let absolute = absolute.to_str().context(NOT_UTF8_PATH)?;
            let result = proxy.conflicts.dismiss(absolute).await;
            explained(chosen, proxy, SyncAction::Dismiss(absolute), result).await?;
            println!("{tag}{}", said::DISMISSED);
        }
        FolderCmd::FreeUpSpace => {
            let result = proxy.folder.free_up_space().await;
            let freed = explained(chosen, proxy, SyncAction::FreeUpSpace, result).await?;
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
async fn ignore(chosen: &Chosen, proxy: &FolderProxies<'_>, action: IgnoreCmd) -> anyhow::Result<()> {
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
    explained(chosen, proxy, SyncAction::Ignore, proxy.folder.set_ignore_patterns(&refs).await).await?;
    println!("{tag}{done}");
    Ok(())
}

/// Before a Forget and before the removal of an account, neither of which is refused for
/// what is unsent: reads how many changes made here are not sent
/// (`UploadQueue.UnsentCount()`, answered for a folder in any state) and, when it is not
/// 0, says "N changes were not sent to OneDrive and will stay only on this computer" and
/// asks whether to go on; with `is_asking_skipped` — the command's `--yes` — it says it
/// and goes on unasked, as a script needs. Whether the caller goes on; `true` at once for
/// 0. A count that cannot be read is said, and asked about too. The question is asked of
/// a terminal alone: with an input that is none (a script, a pipe, a loop that reads a
/// list) nothing is read from it and the command fails, naming `--yes`; so does an input
/// that ends with no answer. The texts and the two decisions are `text::folder`'s.
pub(crate) async fn warn_of_unsent_changes(proxy: &FolderProxies<'_>, is_asking_skipped: bool) -> anyhow::Result<bool> {
    use std::io::{IsTerminal as _, Write as _};
    let count = proxy.queue.unsent_count().await.map_err(|error| error.to_string());
    let Some(warning) = said::unsent_warning_text(count.as_ref().map(|count| *count).map_err(String::as_str)) else { return Ok(true) };
    eprintln!("{warning}");
    match said::decide_unsent_question(is_asking_skipped, std::io::stdin().is_terminal()) {
        said::UnsentQuestion::Skipped => return Ok(true),
        said::UnsentQuestion::NoTerminal => bail!("{}", said::NO_TERMINAL_TO_ASK),
        said::UnsentQuestion::Asked => {}
    }
    eprint!("{}", said::GO_ON_QUESTION);
    std::io::stderr().flush().context("cannot write the question")?;
    let mut line = String::new();
    let is_line_read = std::io::stdin().read_line(&mut line).context("cannot read the answer")? > 0;
    match said::decide_unsent_answer(is_line_read.then_some(line.as_str())) {
        said::UnsentAnswer::GoOn => Ok(true),
        said::UnsentAnswer::NotAnswered => bail!("{}", said::QUESTION_NOT_ANSWERED),
        said::UnsentAnswer::Declined => {
            // TODO: a "no" ends the command with exit code 0, like one that had nothing to do: whether a command the user declined is to exit with another code is the owner's to say; check and fix if needed.
            eprintln!("{}", said::NOTHING_CHANGED);
            Ok(false)
        }
    }
}
