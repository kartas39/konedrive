//! What `org.konedrive.UploadQueue` asks of an account's folder: what waits to be uploaded,
//! the mass-delete guard's decision and what stays local. `dbus::upload_queue` is the thin wrapper around it. The answers are
//! `crate::upload_queue`'s, over the plan's answers, the step memory and the state of sending.

use std::collections::BTreeMap;
use std::path::PathBuf;

use konedrive_tree::LocalItemId;

use super::{SyncError, SyncService};
use crate::sync_parts::published_plan::ItemAnswer;
use crate::sync_parts::running_steps::RunningStep;
use crate::sync_parts::sync_plan_wake::WakeCause;
use crate::sync_tasks::start_order::KeptFolderParts;
use crate::upload_queue::{self, QueueConditions};

/// One change waiting to be uploaded, as `Changes()` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub seq: u64,
    /// What it does, as the outbox spells it.
    pub kind: String,
    /// The full path.
    pub path: String,
    /// The row's state as stored, or `paused` while the account is.
    pub state: String,
    /// Bytes sent, and bytes in all.
    pub sent: u64,
    pub total: u64,
    /// Why it waits, as stored; empty when nothing holds it.
    pub reason: String,
    /// Unix seconds of the next try; 0 when none is planned.
    pub next_try: i64,
}

/// A file kept back from OneDrive, and why (`NotUploaded()`, `NotUploadedFiles()`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct KeptBack {
    /// The full path.
    pub path: String,
    /// The reason as stored.
    pub reason: String,
}

/// What an answer about the queue is made over, taken once, before the one read of the store.
struct QueueSources {
    /// The parts of the folder's sync, running or stopped: the store, and the plan's wake.
    parts: KeptFolderParts,
    /// The last plan's answers and its counts of held removals; empty with no running sync.
    answers: BTreeMap<LocalItemId, ItemAnswer>,
    held_removal_counts: BTreeMap<LocalItemId, u64>,
    /// The steps that run now; none with no running sync.
    running: Vec<RunningStep>,
    conditions: QueueConditions,
    /// The folder's path.
    folder: PathBuf,
}

impl SyncService {
    /// The sources of an answer about the queue: of the running sync, the published answers, the running steps
    /// and the state of sending now; of a folder whose sync does not run, its store with empty answers, no step
    /// and `upload_queue::describe_stopped_sync`. Refused for an account with no folder, and `NotUp`, with why,
    /// for a folder that keeps no open store.
    fn read_queue_sources(&self) -> Result<QueueSources, SyncError> {
        let folder = self.require_record()?.root.path;
        let view = self.view();
        let Some(parts) = view.parts else { return Err(self.sync_not_running()) };
        let sources = match (&view.sending_holds, &view.onedrive_space) {
            (Some(sending_holds), Some(onedrive_space)) => QueueSources {
                answers: parts.published_plan.list_answers(),
                held_removal_counts: parts.published_plan.list_held_removal_counts(),
                running: parts.running_steps.list_steps(),
                conditions: upload_queue::read_queue_conditions(sending_holds, onedrive_space, self.clock.now()),
                parts,
                folder,
            },
            _ => QueueSources {
                answers: BTreeMap::new(),
                held_removal_counts: BTreeMap::new(),
                running: Vec::new(),
                conditions: upload_queue::describe_stopped_sync(),
                parts,
                folder,
            },
        };
        Ok(sources)
    }

    /// `Changes(limit)`: the rows waiting to be uploaded, oldest first, at most
    /// `limit` (0 for all): (seq, kind, full path, state, bytes sent, bytes
    /// in all, reason, next try).
    pub async fn outbox(&self, limit: u32) -> Result<Vec<Change>, SyncError> {
        let QueueSources { parts, answers, held_removal_counts, running, conditions, folder } = self.read_queue_sources()?;
        let limit = limit as usize;
        let changes = parts
            .store
            .read_items(move |items| {
                let entries = upload_queue::read_unsent_entries(items, &answers, &running, &held_removal_counts, limit)?;
                let paths = upload_queue::read_entry_paths(items, &entries.iter().collect::<Vec<_>>())?;
                // An entry with no path is left out; so is one that agrees by now, and one the plan leaves out.
                let described = entries.iter().filter_map(|entry| upload_queue::describe_queue_change(entry, paths.get(&entry.item.local_item_id)?, &conditions, &folder));
                Ok(described.collect::<Vec<_>>())
            })
            .await
            .map_err(|e| SyncError::Store(e.to_string()))?;
        Ok(changes
            .into_iter()
            .map(|c| Change { seq: c.seq, kind: c.kind, path: c.path, state: c.state, sent: c.sent, total: c.total, reason: c.reason, next_try: c.next_try })
            .collect())
    }

    /// `NotUploaded()`: what stays on this computer and why, by path — what is never uploaded (a symlink, a file from elsewhere
    /// that is not downloaded, …) and the changes that need the user
    /// (blocked: a name OneDrive refuses, OneDrive full).
    pub async fn not_uploaded(&self) -> Result<Vec<KeptBack>, SyncError> {
        let QueueSources { parts, answers, held_removal_counts, running, conditions, folder } = self.read_queue_sources()?;
        let kept_back = parts
            .store
            .read_items(move |items| {
                let entries = upload_queue::read_kept_back_entries(items, &answers, &running, &held_removal_counts)?;
                // The paths of those the list names, and of no other.
                let paths = upload_queue::read_entry_paths(items, &upload_queue::list_blocked_entries(&entries, &conditions))?;
                Ok(upload_queue::list_not_uploaded(&entries, &paths, &conditions, &folder))
            })
            .await
            .map_err(|e| SyncError::Store(e.to_string()))?;
        Ok(kept_back.into_iter().map(|kept| KeptBack { path: kept.path, reason: kept.reason }).collect())
    }

    /// `NotUploadedSummary()`: what is kept back, one row per reason:
    /// (group, reason, count, bytes).
    pub async fn not_uploaded_summary(&self) -> Result<Vec<(String, String, u32, u64)>, SyncError> {
        let QueueSources { parts, answers, held_removal_counts, running, conditions, .. } = self.read_queue_sources()?;
        let summary = parts
            .store
            .read_items(move |items| {
                let entries = upload_queue::read_kept_back_entries(items, &answers, &running, &held_removal_counts)?;
                Ok(upload_queue::summarize_kept_back(&entries, &conditions))
            })
            .await
            .map_err(|e| SyncError::Store(e.to_string()))?;
        Ok(summary.into_iter().map(|row| (row.group, row.reason, row.count, row.bytes)).collect())
    }

    /// `NotUploadedFiles(reason, limit)`: the files kept back for `reason`,
    /// at most `limit` (0 for all), and how many there are.
    pub async fn not_uploaded_files(&self, reason: String, limit: u32) -> Result<(Vec<KeptBack>, u32), SyncError> {
        let QueueSources { parts, answers, held_removal_counts, running, conditions, folder } = self.read_queue_sources()?;
        let files = parts
            .store
            .read_items(move |items| {
                let entries = upload_queue::read_kept_back_entries(items, &answers, &running, &held_removal_counts)?;
                // The paths of the first `limit` of them (of all for 0), and of no other.
                let mut listed = upload_queue::list_entries_kept_back_for(&entries, &conditions, &reason);
                if limit > 0 {
                    listed.truncate(limit as usize);
                }
                let paths = upload_queue::read_entry_paths(items, &listed)?;
                Ok(upload_queue::list_kept_back_files(&entries, &paths, &conditions, &reason, limit, &folder))
            })
            .await
            .map_err(|e| SyncError::Store(e.to_string()))?;
        Ok((files.items.into_iter().map(|kept| KeptBack { path: kept.path, reason: kept.reason }).collect(), files.total))
    }

    /// `ConfirmDeletes()`: the removals the mass-delete guard held go ahead;
    /// how many.
    pub async fn confirm_deletes(&self) -> Result<u32, SyncError> {
        let QueueSources { parts, answers, .. } = self.read_queue_sources()?;
        let is_removal_held = upload_queue::is_removal_held(&answers);
        let confirmed = parts.store.call(move |store| upload_queue::confirm_removals(store, is_removal_held)).await;
        // Also after an error: it may have written some rows, each of which is valid alone.
        parts.sync_plan_wake.wake(WakeCause::UserAnswered);
        confirmed.map(|confirmed| u32::try_from(confirmed).unwrap_or(u32::MAX)).map_err(|e| SyncError::Store(e.to_string()))
    }

    /// `RestoreDeletes()`: the removals the mass-delete guard held are taken back: each
    /// row is left as an item of OneDrive the folder does not show, and the plan shows
    /// it again, not on disk; how many. No listing is asked for it. Under the tree lock,
    /// as the commits of the steps are.
    pub async fn restore_deletes(&self) -> Result<u32, SyncError> {
        let QueueSources { parts, answers, .. } = self.read_queue_sources()?;
        let is_removal_held = upload_queue::is_removal_held(&answers);
        let restored = {
            // Under the tree lock, as the commits of the steps are; the lock is the folder's own, held for the one job.
            let tree_lock = self.view().tree_lock;
            let _tree = match &tree_lock {
                Some(tree_lock) => Some(tree_lock.lock().await),
                None => None,
            };
            parts.store.call(move |store| upload_queue::restore_removals(store, is_removal_held)).await
        };
        // Also after an error: it may have written some rows, each of which is valid alone.
        parts.sync_plan_wake.wake(WakeCause::UserAnswered);
        restored.map(|restored| u32::try_from(restored).unwrap_or(u32::MAX)).map_err(|e| SyncError::Store(e.to_string()))
    }
}
