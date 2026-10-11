//! What `org.konedrive.UploadQueue` asks of an account's folder: what waits to be uploaded,
//! the mass-delete guard's decision and what stays local. `dbus::upload_queue` is the thin wrapper around it. The answers are
//! `crate::upload_queue`'s, over the plan's answers, the step memory and the state of sending.
// The skeleton of the switch: bodies that drove the old mechanism are `todo!()`.
#![allow(dead_code, unused_variables, unused_imports, unused_mut)]

use super::{SyncError, SyncService};

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

impl SyncService {

    /// `Changes(limit)`: the rows waiting to be uploaded, oldest first, at most
    /// `limit` (0 for all): (seq, kind, full path, state, bytes sent, bytes
    /// in all, reason, next try).
    pub async fn outbox(&self, limit: u32) -> Result<Vec<Change>, SyncError> {
        // TODO: `upload_queue::read_unsent_entries` and `describe_queue_change`, in one read of the folder's store (`View::parts`), over the plan's answers, its counts of held removals (`PublishedPlan::list_answers`, `list_held_removal_counts`) and the running steps, with `upload_queue::read_queue_conditions` of the running sync (`View::sending_holds`, `onedrive_space`) — or, with no running sync, `upload_queue::describe_stopped_sync`.
        todo!()
    }

    /// `NotUploaded()`: what stays on this computer and why, by path — what is never uploaded (a symlink, a file from elsewhere
    /// that is not downloaded, …) and the changes that need the user
    /// (blocked: a name OneDrive refuses, OneDrive full).
    pub async fn not_uploaded(&self) -> Result<Vec<KeptBack>, SyncError> {
        // TODO: `upload_queue::read_kept_back_entries` and `list_not_uploaded`, in one read of the folder's store (`View::parts`), over the plan's answers, its counts of held removals (`PublishedPlan::list_answers`, `list_held_removal_counts`) and the running steps, with `upload_queue::read_queue_conditions` of the running sync (`View::sending_holds`, `onedrive_space`) — or, with no running sync, `upload_queue::describe_stopped_sync`.
        todo!()
    }

    /// `NotUploadedSummary()`: what is kept back, one row per reason:
    /// (group, reason, count, bytes).
    pub async fn not_uploaded_summary(&self) -> Result<Vec<(String, String, u32, u64)>, SyncError> {
        // TODO: `upload_queue::read_kept_back_entries` and `summarize_kept_back`, in one read of the folder's store (`View::parts`), over the plan's answers, its counts of held removals (`PublishedPlan::list_answers`, `list_held_removal_counts`) and the running steps, with `upload_queue::read_queue_conditions` of the running sync (`View::sending_holds`, `onedrive_space`) — or, with no running sync, `upload_queue::describe_stopped_sync`.
        todo!()
    }

    /// `NotUploadedFiles(reason, limit)`: the files kept back for `reason`,
    /// at most `limit` (0 for all), and how many there are.
    pub async fn not_uploaded_files(&self, reason: String, limit: u32) -> Result<(Vec<KeptBack>, u32), SyncError> {
        // TODO: `upload_queue::read_kept_back_entries` and `list_kept_back_files`, in one read of the folder's store (`View::parts`), over the plan's answers, its counts of held removals (`PublishedPlan::list_answers`, `list_held_removal_counts`) and the running steps, with `upload_queue::read_queue_conditions` of the running sync (`View::sending_holds`, `onedrive_space`) — or, with no running sync, `upload_queue::describe_stopped_sync`.
        todo!()
    }

    /// `ConfirmDeletes()`: the removals the mass-delete guard held go ahead;
    /// how many.
    pub async fn confirm_deletes(&self) -> Result<u32, SyncError> {
        // TODO: `upload_queue::confirm_removals` in a job of the store's writer, with `upload_queue::is_removal_held` of the published answers; then the plan woken (`KeptFolderParts::sync_plan_wake`, `WakeCause::UserAnswered`).
        todo!()
    }

    /// `RestoreDeletes()`: the removals the mass-delete guard held are taken back: each
    /// row is left as an item of OneDrive the folder does not show, and the plan shows
    /// it again, not on disk; how many. No listing is asked for it. Under the tree lock,
    /// as the commits of the steps are.
    pub async fn restore_deletes(&self) -> Result<u32, SyncError> {
        // TODO: `upload_queue::restore_removals` in a job of the store's writer under the folder's tree lock, with `upload_queue::is_removal_held` of the published answers; then the plan woken (`KeptFolderParts::sync_plan_wake`, `WakeCause::UserAnswered`). No full listing is asked for.
        todo!()
    }

}
