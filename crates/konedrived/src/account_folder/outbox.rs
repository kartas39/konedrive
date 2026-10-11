//! What `org.konedrive.UploadQueue` asks of an account's folder: what waits to be uploaded,
//! the mass-delete guard's decision and what stays local, and the end of sending at the
//! daemon's stop. `dbus::upload_queue` is the thin wrapper around it. The answers are
//! `crate::upload_queue`'s, over the plan's answers, the step memory and the state of sending.
// The skeleton of the switch: bodies that drove the old mechanism are `todo!()`.
#![allow(dead_code, unused_variables, unused_imports, unused_mut)]

use super::{SyncError, SyncService};
use crate::status::snapshot::OutboxNote;

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

    /// The daemon is stopping: the outbox worker, if one runs,
    /// takes nothing more and lets the requests in flight return. The future
    /// ends when it has; the caller bounds the wait (`crate::daemon::stop`).
    pub fn close_outbox(&self) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>> {
        // TODO: the daemon's end: `sync_tasks::stop_order::stop_folder_sync` (after `SendingHolds::set_stopping`),
        // under the daemon's stop bound.
        todo!()
    }

    /// The outbox's counts on the bus are 0: its worker stopped, or its rows
    /// were dropped (the outbox on the bus). A worker that starts counts again.
    pub(super) fn clear_outbox_counts(&self) {
        self.state.update(|s| {
            s.outbox.pending_count = 0;
            s.outbox.pending_bytes = 0;
            s.outbox.blocked_count = 0;
            s.outbox.held_count = 0;
            s.outbox.uploads.clear();
            s.outbox.quota_full = false;
            s.outbox.space_waiting_count = 0;
            s.outbox.space_waiting_bytes = 0;
            s.outbox.too_big_count = 0;
            s.outbox.too_big_bytes = 0;
            // What the worker said of itself went with the worker.
            if let Some(note) = OutboxNote::after_worker(&s.outbox.note, None, None, 0) {
                s.outbox.note = note;
            }
        });
    }

    /// `Changes(limit)`: the rows waiting to be uploaded, oldest first, at most
    /// `limit` (0 for all): (seq, kind, full path, state, bytes sent, bytes
    /// in all, reason, next try).
    pub async fn outbox(&self, limit: u32) -> Result<Vec<Change>, SyncError> {
        // TODO: `upload_queue::read_unsent_entries` and `describe_queue_change`, over the plan's answers (`PublishedPlan::list_answers`) and the running steps, in one read of the folder's store.
        todo!()
    }

    /// `NotUploaded()`: what stays on this computer and why, by path — what is never uploaded (a symlink, a file from elsewhere
    /// that is not downloaded, …) and the changes that need the user
    /// (blocked: a name OneDrive refuses, OneDrive full).
    pub async fn not_uploaded(&self) -> Result<Vec<KeptBack>, SyncError> {
        // TODO: `upload_queue::read_kept_back_entries` and `list_not_uploaded`, over the plan's answers (`PublishedPlan::list_answers`) and the running steps, in one read of the folder's store.
        todo!()
    }

    /// `NotUploadedSummary()`: what is kept back, one row per reason:
    /// (group, reason, count, bytes).
    pub async fn not_uploaded_summary(&self) -> Result<Vec<(String, String, u32, u64)>, SyncError> {
        // TODO: `upload_queue::read_kept_back_entries` and `summarize_kept_back`, over the plan's answers (`PublishedPlan::list_answers`) and the running steps, in one read of the folder's store.
        todo!()
    }

    /// `NotUploadedFiles(reason, limit)`: the files kept back for `reason`,
    /// at most `limit` (0 for all), and how many there are.
    pub async fn not_uploaded_files(&self, reason: String, limit: u32) -> Result<(Vec<KeptBack>, u32), SyncError> {
        // TODO: `upload_queue::read_kept_back_entries` and `list_kept_back_files`, over the plan's answers (`PublishedPlan::list_answers`) and the running steps, in one read of the folder's store.
        todo!()
    }

    /// `ConfirmDeletes()`: the removals the mass-delete guard held go ahead;
    /// how many.
    pub async fn confirm_deletes(&self) -> Result<u32, SyncError> {
        // TODO: `upload_queue::confirm_removals`, with `upload_queue::is_removal_held`; the plan is woken (`SyncPlanWake`).
        todo!()
    }

    /// `RestoreDeletes()`: the removals the mass-delete guard held are dropped,
    /// and their items placed again from OneDrive at once, by a cycle with a
    /// Full reconcile (the outbox on the bus); how many. Under the tree lock, as the
    /// worker's commits are, so that a cycle's swap cannot give the items
    /// their forgotten local objects back.
    pub async fn restore_deletes(&self) -> Result<u32, SyncError> {
        // TODO: `upload_queue::restore_removals`, with `upload_queue::is_removal_held`; the plan is woken (`SyncPlanWake`).
        todo!()
    }

}
