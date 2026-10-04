//! The outbox of a read-write folder, from the folder's side: its worker (`upload`) started,
//! woken and stopped with the folder's sync, its rows dropped at a forced switch to read-only,
//! and what `org.konedrive.UploadQueue` shows of it (`docs/design/writes.md` §11): what waits to
//! be uploaded, the mass-delete guard's decision and what stays local. `dbus::upload_queue` is
//! the thin wrapper around the last.

use std::sync::{Arc, Mutex, OnceLock};

use konedrive_tree::outbox::{OutboxState, Reason};
use konedrive_tree::{ActivityRow, Store};

use super::folder::Stopped;
use super::running_sync::Handles;
use super::{SyncError, SyncService};
use crate::conditions::running::Running as WhatRuns;
use crate::folder::root::SyncRoot;
use crate::hydration::source::ContentSource;
use crate::remote::listing::PollHandle;
use crate::status::snapshot::OutboxNote;
use crate::upload::kept_back::SummaryRow;
use crate::upload::{self, OutboxHost, OutboxWorker, WorkerConfig, WorkerStatus};

/// One row as `Changes()` lists it: (seq, kind, full path, state, bytes sent,
/// bytes in all, reason, next try in unix seconds or 0).
pub type OutboxEntry = (u64, String, String, String, u64, u64, String, i64);

impl SyncService {
    /// The tree store of this account's OneDrive folder: refused as `Refresh`
    /// is for a folder that is not connected to OneDrive, and `NotUp`, with why,
    /// for a folder that is not up, or whose store has not been opened yet.
    pub(super) fn outbox_store(&self) -> Result<Store, SyncError> {
        self.require_onedrive()?;
        // The state first: the store of a folder that went down is still here.
        if self.view().down.is_some() {
            return Err(self.sync_not_running());
        }
        self.tree_store().ok_or_else(|| self.sync_not_running())
    }

    /// Runs `f` on the store's read-only connection, on a blocking thread:
    /// never behind a writer (issue #38).
    async fn read_outbox<T: Send + 'static>(
        &self,
        f: impl FnOnce(&konedrive_tree::ReadStore<'_>) -> Result<T, konedrive_tree::TreeError> + Send + 'static,
    ) -> Result<T, SyncError> {
        self.outbox_store()?.read(f).await.map_err(|e| SyncError::Store(e.to_string()))
    }

    /// Runs `f` on the store on a blocking thread.
    async fn with_outbox<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut konedrive_tree::TreeStore) -> Result<T, konedrive_tree::TreeError> + Send + 'static,
    ) -> Result<T, SyncError> {
        self.outbox_store()?.call(f).await.map_err(|e| SyncError::Store(e.to_string()))
    }

    /// The outbox worker of the read-write folder at `root` (`docs/design/writes.md`
    /// §5), built and not started: the sync being built keeps it, beside the watcher, and
    /// starts it once its poller runs. It is linked to the parts it talks to — the poller,
    /// through `poll`, told once that runs — and to nothing of the service but what it
    /// reports to.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn outbox_worker(
        &self,
        root: &SyncRoot,
        store: &Store,
        drive: &konedrive_graph::drive::DriveClient,
        tree_lock: &Arc<tokio::sync::Mutex<()>>,
        source: &Arc<dyn ContentSource>,
        kept_back: Arc<Mutex<Option<Vec<SummaryRow>>>>,
        poll: Arc<OnceLock<PollHandle>>,
    ) -> OutboxWorker {
        OutboxWorker::new(WorkerConfig {
            root: root.clone(),
            store: store.clone(),
            drive: drive.clone(),
            locks: self.locks.clone(),
            machine_name: self.machine_name(),
            tree_lock: Arc::clone(tree_lock),
            host: Arc::new(Host {
                state: self.state.clone(),
                report: self.report.clone(),
                running: Arc::clone(&self.running),
                root: root.path.clone(),
                kept_back,
                poll,
                gate: self.gate(),
            }),
            limits: upload::Limits::default(),
            // Moves out of the folder: the helper over this account's link, fills
            // through the folder's source, and the registry's router.
            moved_out: Some(self.move_outs(Some(Arc::clone(source)))),
            quota: self.quota(),
        })
    }

    /// Wakes the worker of the sync running now, if any.
    pub(super) fn wake_outbox(&self) {
        if let Some(outbox) = self.running().as_ref().and_then(Handles::outbox) {
            outbox.wake();
        }
    }

    /// `Refresh()`'s part for the outbox: rows in backoff go now.
    pub(super) fn retry_outbox(&self) {
        if let Some(outbox) = self.running().as_ref().and_then(Handles::outbox) {
            outbox.retry_now();
        }
    }

    /// The network came back (`docs/design/writes.md` §9): the outbox waits for the delta
    /// cycle this asks for, then sends what backed off meanwhile.
    pub(super) fn outbox_after_network(&self) {
        if let Some(outbox) = self.running().as_ref().and_then(Handles::outbox) {
            outbox.wait_for_cycle(true);
        }
    }

    /// The daemon is stopping (issue #84): the outbox worker, if one runs,
    /// takes nothing more and lets the requests in flight return. The future
    /// ends when it has; the caller bounds the wait (`crate::daemon::stop`).
    pub fn close_outbox(&self) -> Option<impl std::future::Future<Output = ()> + Send + 'static> {
        self.running().as_ref().and_then(Handles::outbox).map(|outbox| outbox.close())
    }

    /// A quota just read into the account's quota, here or by the account (`RefreshInfo`):
    /// the outbox decides by it, if there is one.
    pub(super) fn quota_seen(&self, quota: &konedrive_graph::drive::DriveQuota) {
        if !upload::space::known(quota) {
            return;
        }
        if let Some(outbox) = self.running().as_ref().and_then(Handles::outbox) {
            outbox.quota_read(quota);
        }
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
    pub async fn outbox(&self, limit: u32) -> Result<Vec<OutboxEntry>, SyncError> {
        let rows = self.read_outbox(move |s| if limit == 0 { s.outbox_rows() } else { s.outbox_first(limit as usize) }).await?;
        let root = self.record().map(|record| record.root.path).unwrap_or_default();
        let state = self.state.get();
        let (paused, full) = (state.stopped(), state.outbox.quota_full);
        let uploads = state.outbox.uploads;
        tokio::task::spawn_blocking(move || entries(rows, &root, &uploads, paused, full))
            .await
            .map_err(|e| SyncError::Io(format!("the outbox task failed: {e}")))
    }

    /// `NotUploaded()`: what stays on this computer and why, as (full path,
    /// reason) — what is never uploaded (a symlink, a file from elsewhere
    /// that is not downloaded, …) and the changes that need the user
    /// (blocked: a name OneDrive refuses, OneDrive full).
    pub async fn not_uploaded(&self) -> Result<Vec<(String, String)>, SyncError> {
        let (skipped, rows) = self.read_outbox(|s| Ok((s.local_skipped()?, s.outbox_blocked()?))).await?;
        let root = self.record().map(|record| record.root.path).unwrap_or_default();
        let mut out: Vec<(String, String)> = skipped.into_iter().map(|s| (root.join(&s.rel).display().to_string(), s.reason.to_string())).collect();
        out.extend(
            rows.into_iter()
                .filter(|row| row.state == OutboxState::Blocked)
                .map(|row| (root.join(&row.rel).display().to_string(), row.reason.unwrap_or(Reason::Blocked).to_string())),
        );
        out.sort();
        Ok(out)
    }

    /// `NotUploadedSummary()`: what is kept back, one row per reason:
    /// (group, reason, count, bytes) ([`kept_back`](super::kept_back)).
    pub async fn not_uploaded_summary(&self) -> Result<Vec<crate::upload::kept_back::SummaryRow>, SyncError> {
        self.outbox_store()?;
        if let Some(kept) = self.running().and_then(|running| running.kept_back.lock().unwrap().clone()) {
            return Ok(kept);
        }
        let (skipped, groups) = self.read_outbox(|s| Ok((s.skipped_groups()?, s.outbox_groups()?))).await?;
        let full = self.state.get().outbox.quota_full;
        Ok(crate::upload::kept_back::summary(&skipped, &groups, full))
    }

    /// `NotUploadedFiles(reason, limit)`: the files kept back for `reason`,
    /// at most `limit` (0 for all), and how many there are.
    pub async fn not_uploaded_files(&self, reason: String, limit: u32) -> Result<(Vec<(String, String)>, u32), SyncError> {
        let root = self.record().map(|record| record.root.path).unwrap_or_default();
        let full = self.state.get().outbox.quota_full;
        self.read_outbox(move |s| crate::upload::kept_back::files(s, &root, full, &reason, limit)).await
    }

    /// `ConfirmDeletes()`: the removals the mass-delete guard held go ahead;
    /// how many.
    pub async fn confirm_deletes(&self) -> Result<u32, SyncError> {
        let released = self.with_outbox(|s| s.outbox_release_held()).await?;
        self.wake_outbox();
        Ok(released as u32)
    }

    /// `RestoreDeletes()`: the removals the mass-delete guard held are dropped,
    /// and their items placed again from OneDrive at once, by a cycle with a
    /// Full reconcile (the outbox on the bus); how many. Under the tree lock, as the
    /// worker's commits are, so that a cycle's swap cannot give the items
    /// their forgotten local objects back.
    pub async fn restore_deletes(&self) -> Result<u32, SyncError> {
        // The store and the tree lock as last published, and no state lock: a cycle holds
        // the tree lock while it waits for the state, so nothing that holds the state may
        // wait for the tree lock (`F198`).
        let store = self.outbox_store()?;
        let view = self.view();
        let dropped = {
            let _tree = match &view.tree_lock {
                Some(tree) => Some(tree.lock().await),
                None => None,
            };
            store.call(|s| s.outbox_drop_held()).await.map_err(|e| SyncError::Store(e.to_string()))?
        };
        if !dropped.is_empty() {
            self.nudge_full();
        }
        // What a dropped move out named outside the folder is tidied, before the
        // answer, whether or not a worker runs.
        if let Some(record) = view.record {
            self.tidy_dropped(&record.root, &store, view.source, &dropped).await;
        }
        self.wake_outbox();
        Ok(dropped.len() as u32)
    }

    /// Drops the outbox's rows (`docs/design/writes.md` §2): a forced switch to read-only, and only that.
    /// The files stay, as ordinary local changes, and lose their upload
    /// mark (`user.konedrive.sync`). A rename half-done in OneDrive under a temporary name stays:
    /// dropped, the item would stay under that name, and its local object
    /// would go.
    ///
    /// Inside a change of the folder's state, with every part of the folder's sync stopped
    /// and waited for: no cycle and no worker is there to share the store with, so no tree
    /// lock is taken. The store is the folder's, or the one on disk for a folder that is
    /// not up — a folder that waits for the helper still has its rows.
    pub(super) async fn drop_outbox(&self, stopped: &mut Stopped<'_>) {
        let Some(store) = self.store_in(stopped).await else { return };
        let Some(root) = stopped.folder().record().map(|record| record.root.clone()) else { return };
        let source = stopped.folder().source();
        let (dropping, marked) = (store.clone(), root.clone());
        let dropped = tokio::task::spawn_blocking(move || {
            let rows = dropping.call_blocking(move |s| {
                let mut rows = upload::move_out::drop_rows(s)?;
                rows.extend(s.outbox_drop_all()?);
                Ok(rows)
            })?;
            upload::clear_marks(&marked, &rows);
            Ok::<_, konedrive_tree::TreeError>(rows)
        })
        .await;
        // The upload sessions of the rows dropped are given up: cancelled now, so that no
        // empty placeholder keeps a name in OneDrive (issue #47). One that fails stays listed
        // for the worker of a later read-write start.
        if let Some(drive) = self.drive() {
            upload::cancel_given_up(&store, drive, DROPPED_CANCELS).await;
        }
        self.clear_outbox_counts();
        // What moves out of the folder left outside it is tidied.
        self.forget_moved_out();
        if let Ok(Ok(rows)) = &dropped {
            self.tidy_dropped(&root, &store, source, rows).await;
        }
        match dropped.map(|rows| rows.map(|rows| rows.len())) {
            Ok(Ok(0)) => {}
            Ok(Ok(n)) => tracing::info!("{n} change(s) waiting to upload were dropped; the files stay as they are"),
            Ok(Err(e)) => tracing::warn!("cannot drop the changes waiting to upload: {e}"),
            Err(e) => tracing::warn!("the task dropping the changes waiting to upload failed: {e}"),
        }
    }

    /// The tree store of the recorded OneDrive folder, inside a change, for what the change
    /// does to what waits in it: the one kept with the folder, or else the one on disk,
    /// opened now and kept ([`kept_store`](Self::kept_store)). `None` for a folder that has
    /// none: a local one, or one whose store was never made or cannot be opened.
    pub(super) async fn store_in(&self, stopped: &mut Stopped<'_>) -> Option<Store> {
        if let Some(store) = stopped.folder().store() {
            return Some(store);
        }
        if stopped.folder().record()?.source != super::RootSource::OneDrive {
            return None;
        }
        let paths = self.sync_paths()?.clone();
        if !paths.tree_db.exists() {
            return None;
        }
        self.kept_store(stopped, &paths).await.map_err(|e| tracing::warn!("{e}")).ok()
    }

    /// The one connection to the recorded folder's tree store: the one kept with the
    /// folder's record, or the store on disk, opened now — made if there is none — and
    /// kept with the record from here on. Nothing else opens it, so there is never a
    /// second connection for one folder.
    pub(super) async fn kept_store(&self, stopped: &mut Stopped<'_>, paths: &super::SyncPaths) -> Result<Store, String> {
        if let Some(store) = stopped.folder().store() {
            return Ok(store);
        }
        let store = super::start_stop::open_store(paths).await?;
        match stopped.folder_mut().record_mut() {
            Some(record) => record.kept.store = Some(store.clone()),
            None => return Err("no folder is recorded".into()),
        }
        Ok(store)
    }

    /// How many changes wait in `store`, the folder's tree store while its sync runs or
    /// inside a change — or, with none, in the one on disk its next sync opens — for a
    /// Forget and `Accounts.Remove`, which would delete them with the store. Only read. A
    /// store that is open and cannot be read refuses; one on disk that cannot be opened
    /// holds nothing a sync could send (it is rebuilt empty).
    pub(super) async fn changes_waiting(&self, store: Option<Store>) -> Result<u64, SyncError> {
        if let Some(store) = store {
            return store
                .call(|s| s.outbox_len())
                .await
                .map(|n| n as u64)
                .map_err(|e| SyncError::Store(format!("cannot tell whether changes wait to be uploaded: {e}")));
        }
        let Some(tree_db) = self.sync_paths().map(|p| p.tree_db.clone()) else { return Ok(0) };
        if !tree_db.exists() {
            return Ok(0);
        }
        let counted = tokio::task::spawn_blocking(move || konedrive_tree::ReadStore::at(&tree_db, |s| s.outbox_len()))
            .await
            .map_err(|e| e.to_string())
            .and_then(|rows| rows.map_err(|e| e.to_string()));
        Ok(counted.map_or_else(
            |e| {
                tracing::warn!("cannot read the folder's tree store to count its waiting changes: {e}");
                0
            },
            |n| n as u64,
        ))
    }
}


/// A row's state in `Changes()` while the account is paused, whatever it
/// waited for before: blocked and held rows keep theirs.
const PAUSED_STATE: &str = "paused";

/// `Changes()`'s entries for `rows`; a waiting file's size is read from the
/// disk (`lstat`), off the runtime. While `paused`, every row that would
/// otherwise wait, retry or run reads `paused`, with no reason and no next
/// try: a pause is no failure, and nothing is tried before it ends (an upload
/// in fragments still sending shows its bytes until it stops at the next).
/// Otherwise, while OneDrive is `full`, a change that sends content and says
/// nothing else says it waits for space (issue #2).
pub(crate) fn entries(rows: Vec<konedrive_tree::outbox::OutboxRow>, root: &std::path::Path, uploads: &[(String, u64, u64)], paused: bool, full: bool) -> Vec<OutboxEntry> {
    rows
        .into_iter()
        .map(|row| {
            let path = root.join(&row.rel).display().to_string();
            let (done, total) = match uploads.iter().find(|(p, _, _)| *p == path) {
                Some((_, sent, total)) => (*sent, *total),
                None if row.kind.sends_content() => {
                    let size = row
                        .snapshot_size()
                        .or_else(|| std::fs::symlink_metadata(root.join(&row.rel)).ok().filter(|m| m.is_file()).map(|m| m.len()));
                    (0, size.unwrap_or(0))
                }
                None => (0, 0),
            };
            if paused && !matches!(row.state, OutboxState::Blocked | OutboxState::Held) {
                return (row.seq as u64, row.kind.as_str().to_owned(), path, PAUSED_STATE.to_owned(), done, total, String::new(), 0);
            }
            (
                row.seq as u64,
                row.kind.as_str().to_owned(),
                path,
                row.state.as_str().to_owned(),
                done,
                total,
                match row.reason {
                    None if full && row.kind.sends_content() && !matches!(row.state, OutboxState::Blocked | OutboxState::Held) => {
                        Reason::WaitingForSpace.to_string()
                    }
                    reason => reason.map(|r| r.to_string()).unwrap_or_default(),
                },
                row.next_try.unwrap_or(0),
            )
        })
        .collect()
}

/// The upload sessions a forced switch to read-only cancels at most, beyond what the pool
/// runs at once (issue #47).
const DROPPED_CANCELS: usize = 256;

/// What the outbox worker reports to and asks of: the activity log, the published state,
/// the poller of its own sync for a cycle, and the write gate. It holds no way back to the
/// service.
pub(super) struct Host {
    state: crate::status::snapshot::SyncStateHandle,
    report: crate::status::report::Report,
    /// The account's one place that decides what runs, and its clock.
    running: Arc<WhatRuns>,
    /// The folder, for the full paths of what is uploading.
    root: std::path::PathBuf,
    kept_back: Arc<Mutex<Option<Vec<SummaryRow>>>>,
    /// The poller of the worker's sync, once it runs.
    poll: Arc<OnceLock<PollHandle>>,
    gate: super::mode::Gate,
}

impl OutboxHost for Host {
    /// `ActivityLog.Added`: the worker has written the event into the store with
    /// its commit.
    fn activity(&self, event: &ActivityRow) {
        self.report.activity.announce(event.clone());
    }

    /// `NotUploadedSummary()`'s answer from now on.
    fn kept_back(&self, summary: &[SummaryRow]) {
        *self.kept_back.lock().unwrap() = Some(summary.to_vec());
    }

    /// `PendingCount`, `PendingBytes`, `BlockedCount` and `Uploads`; and the folder's note
    /// while OneDrive asked the uploads to wait or the worker cannot open the folder
    /// (`OutboxNote::after_worker`, in `LastError`).
    fn status(&self, status: &WorkerStatus) {
        let uploads: Vec<(String, u64, u64)> =
            status.uploads.iter().map(|u| (self.root.join(&u.rel).display().to_string(), u.sent, u.total)).collect();
        self.state.update(|s| {
            s.outbox.pending_count = status.counts.pending;
            s.outbox.pending_bytes = status.counts.pending_bytes;
            s.outbox.blocked_count = status.counts.blocked;
            s.outbox.held_count = status.counts.held;
            s.outbox.uploads = uploads;
            s.outbox.quota_full = status.quota_full;
            s.outbox.space_waiting_count = status.counts.space_waiting;
            s.outbox.space_waiting_bytes = status.counts.space_waiting_bytes;
            s.outbox.too_big_count = status.counts.too_big;
            s.outbox.too_big_bytes = status.counts.too_big_bytes;
            let now = crate::status::activity::unix_now();
            if let Some(note) = OutboxNote::after_worker(&s.outbox.note, status.folder_closed.as_deref(), status.throttled_until, now) {
                s.outbox.note = note;
            }
        });
    }

    /// OneDrive changed under a row (`docs/design/writes.md` §7), or a folder a row
    /// needs is gone there: the next cycle comes now, and its delta carries
    /// the change (the outbox on the bus: no Full reconcile).
    fn cycle_wanted(&self) {
        if let Some(poll) = self.poll.get() {
            poll.refresh();
            poll.wake_live();
        }
    }

    /// An item's local object was forgotten (delete × edit, a folder deleted
    /// only in part): the next cycle comes now, with a Full reconcile, so
    /// that it is placed again though the delta may have carried it already.
    fn full_cycle_wanted(&self) {
        if let Some(poll) = self.poll.get() {
            poll.refresh_full();
        }
    }

    /// The one place that decides what runs (`running`).
    fn stopped(&self, store: &Store) -> bool {
        self.running.stopped(store)
    }

    /// The account's clock: a pause is over for the worker when it is for the poll.
    fn now(&self) -> i64 {
        self.running.clock().now()
    }

    /// The write gate, asked again before each row.
    fn may_write(&self) -> Result<(), String> {
        self.gate.check()
    }
}

#[cfg(test)]
mod tests;
