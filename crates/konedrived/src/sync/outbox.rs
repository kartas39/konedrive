//! The outbox of a read-write folder, from the folder's side: its worker (`upload`) started,
//! woken and stopped with the folder's sync, its rows dropped at a forced switch to read-only,
//! and what `org.konedrive.UploadQueue` shows of it (`docs/design/writes.md` §11): what waits to
//! be uploaded, the mass-delete guard's decision and what stays local. `dbus::upload_queue` is
//! the thin wrapper around the last.

use std::sync::{Arc, Weak};

use konedrive_tree::outbox::{OutboxState, Reason};
use konedrive_tree::{ActivityRow, Store};

use super::{SyncError, SyncService};
use crate::config::Mode;
use crate::folder::root::SyncRoot;
use crate::upload::{self, OutboxHost, OutboxWorker, WorkerConfig, WorkerStatus};

/// One row as `Changes()` lists it: (seq, kind, full path, state, bytes sent,
/// bytes in all, reason, next try in unix seconds or 0).
pub type OutboxEntry = (u64, String, String, String, u64, u64, String, i64);

impl SyncService {
    /// The tree store of this account's OneDrive folder: refused as `Refresh`
    /// is for a folder that is not connected to OneDrive, and `NoRoot`
    /// before its sync has opened one.
    pub(super) fn outbox_store(&self) -> Result<Store, SyncError> {
        self.require_onedrive()?;
        self.store.lock().unwrap().clone().ok_or(SyncError::NoRoot)
    }

    /// Runs `f` on the store's read-only connection, on a blocking thread:
    /// never behind a writer (issue #38).
    async fn read_outbox<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut konedrive_tree::TreeStore) -> Result<T, konedrive_tree::TreeError> + Send + 'static,
    ) -> Result<T, SyncError> {
        self.outbox_store()?.read(f).await.map_err(|e| SyncError::Io(e.to_string()))
    }

    /// Runs `f` on the store on a blocking thread.
    async fn with_outbox<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut konedrive_tree::TreeStore) -> Result<T, konedrive_tree::TreeError> + Send + 'static,
    ) -> Result<T, SyncError> {
        self.outbox_store()?.call(f).await.map_err(|e| SyncError::Io(e.to_string()))
    }

    /// Starts the outbox worker of the read-write folder at `root` (`docs/design/writes.md`
    /// §5): the sync starting it keeps it, beside the watcher, and stops it with the
    /// watcher, without the lifecycle lock. Called in the critical section that publishes
    /// the sync: it spawns and returns, taking no lock. Rows a previous run left `running`
    /// are replayed first; the rest go as the watcher's examination records them.
    pub(super) fn start_outbox(&self, root: &SyncRoot, store: &Store, drive: &konedrive_graph::drive::DriveClient) -> Option<OutboxWorker> {
        if self.mode() != Mode::ReadWrite {
            return None;
        }
        let worker = OutboxWorker::new(WorkerConfig {
            root: root.clone(),
            store: store.clone(),
            drive: drive.clone(),
            locks: self.locks.clone(),
            machine_name: self.machine_name(),
            tree_lock: Arc::clone(&self.tree_lock),
            host: Arc::new(Host::new(self.me.clone())),
            limits: upload::Limits::default(),
            // Moves out of the folder: the helper over this account's link, fills
            // through its source, and the hub's router.
            moved_out: Some(self.move_outs()),
            quota: self.quota(),
        });
        // The folder's first delta cycle runs before the outbox (`docs/design/writes.md` §3).
        worker.wait_for_cycle(false);
        worker.start();
        Some(worker)
    }

    /// Stops the running sync's outbox worker, if any, and waits for it: a request under way
    /// finishes, nothing more is taken. The rest of the sync goes on.
    pub(super) async fn stop_outbox(&self) {
        let outbox = self.syncing.lock().unwrap().as_mut().and_then(|s| s.outbox.take());
        if let Some(outbox) = outbox {
            outbox.stop().await;
            self.clear_outbox_counts();
        }
    }

    /// Wakes the worker of the sync running now, if any.
    pub(super) fn wake_outbox(&self) {
        if let Some(outbox) = self.syncing.lock().unwrap().as_ref().and_then(|s| s.outbox.as_ref()) {
            outbox.wake();
        }
    }

    /// What the watcher's examination calls when it recorded rows: wakes the outbox worker
    /// of the sync running now, if any.
    pub(super) fn outbox_waker(&self) -> Arc<dyn Fn() + Send + Sync> {
        let me = self.me.clone();
        Arc::new(move || {
            let Some(service) = me.upgrade() else { return };
            let syncing = service.syncing.lock().unwrap();
            if let Some(outbox) = syncing.as_ref().and_then(|s| s.outbox.as_ref()) {
                outbox.wake();
            }
        })
    }

    /// What a read-write cycle calls when it went through: the outbox worker of the sync
    /// running now, which waits for the folder's first delta cycle, may go.
    pub(super) fn cycled_hook(&self) -> Arc<dyn Fn() + Send + Sync> {
        let me = self.me.clone();
        Arc::new(move || {
            let Some(service) = me.upgrade() else { return };
            let syncing = service.syncing.lock().unwrap();
            if let Some(outbox) = syncing.as_ref().and_then(|s| s.outbox.as_ref()) {
                outbox.cycle_done();
            }
        })
    }

    /// `Refresh()`'s part for the outbox: rows in backoff go now.
    pub(super) fn retry_outbox(&self) {
        let syncing = self.syncing.lock().unwrap();
        if let Some(outbox) = syncing.as_ref().and_then(|s| s.outbox.as_ref()) {
            outbox.retry_now();
        }
    }

    /// The network came back (`docs/design/writes.md` §9): the outbox waits for the delta
    /// cycle this asks for, then sends what backed off meanwhile.
    pub(super) fn outbox_after_network(&self) {
        let syncing = self.syncing.lock().unwrap();
        if let Some(outbox) = syncing.as_ref().and_then(|s| s.outbox.as_ref()) {
            outbox.wait_for_cycle(true);
        }
    }

    /// The daemon is stopping (issue #84): the outbox worker, if one runs,
    /// takes nothing more and lets the requests in flight return. The future
    /// ends when it has; the caller bounds the wait (`crate::daemon::stop`).
    pub fn close_outbox(&self) -> Option<impl std::future::Future<Output = ()> + Send + 'static> {
        self.syncing.lock().unwrap().as_ref().and_then(|s| s.outbox.as_ref()).map(|outbox| outbox.close())
    }

    /// A quota just read into the account's quota, here or by the account (`RefreshInfo`):
    /// the outbox decides by it, if there is one.
    pub(super) fn quota_seen(&self, quota: &konedrive_graph::drive::DriveQuota) {
        if !upload::space::known(quota) {
            return;
        }
        let syncing = self.syncing.lock().unwrap();
        if let Some(outbox) = syncing.as_ref().and_then(|s| s.outbox.as_ref()) {
            outbox.quota_read(quota);
        }
    }

    /// The outbox's counts on the bus are 0: its worker stopped, or its rows
    /// were dropped (the outbox on the bus). A worker that starts counts again.
    pub(super) fn clear_outbox_counts(&self) {
        *self.kept_back.lock().unwrap() = None;
        self.state.update(|s| {
            s.pending_count = 0;
            s.pending_bytes = 0;
            s.blocked_count = 0;
            s.held_count = 0;
            s.uploads.clear();
            s.quota_full = false;
            s.space_waiting_count = 0;
            s.space_waiting_bytes = 0;
            s.too_big_count = 0;
            s.too_big_bytes = 0;
        });
    }

    /// `Changes(limit)`: the rows waiting to be uploaded, oldest first, at most
    /// `limit` (0 for all): (seq, kind, full path, state, bytes sent, bytes
    /// in all, reason, next try).
    pub async fn outbox(&self, limit: u32) -> Result<Vec<OutboxEntry>, SyncError> {
        let rows = self.read_outbox(move |s| if limit == 0 { s.outbox_rows() } else { s.outbox_first(limit as usize) }).await?;
        let root = self.registration().map(|reg| reg.root.path).unwrap_or_default();
        let state = self.state.get();
        let (paused, full) = (state.stopped(), state.quota_full);
        let uploads = state.uploads;
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
        let root = self.registration().map(|reg| reg.root.path).unwrap_or_default();
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
        self.require_onedrive()?;
        if let Some(kept) = self.kept_back.lock().unwrap().clone() {
            return Ok(kept);
        }
        let (skipped, groups) = self.read_outbox(|s| Ok((s.skipped_groups()?, s.outbox_groups()?))).await?;
        let full = self.state.get().quota_full;
        Ok(crate::upload::kept_back::summary(&skipped, &groups, full))
    }

    /// `NotUploadedFiles(reason, limit)`: the files kept back for `reason`,
    /// at most `limit` (0 for all), and how many there are.
    pub async fn not_uploaded_files(&self, reason: String, limit: u32) -> Result<(Vec<(String, String)>, u32), SyncError> {
        let root = self.registration().map(|reg| reg.root.path).unwrap_or_default();
        let full = self.state.get().quota_full;
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
        let dropped = {
            let _tree = self.tree_lock.lock().await;
            self.with_outbox(|s| s.outbox_drop_held()).await?
        };
        if !dropped.is_empty() {
            self.nudge_full();
        }
        // What a dropped move out named outside the folder is tidied, before the
        // answer, whether or not a worker runs.
        let store = self.store.lock().unwrap().clone();
        if let (Some(reg), Some(store)) = (self.registration(), store) {
            self.tidy_dropped(&reg.root, &store, &dropped).await;
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
    /// The caller holds `lifecycle` for writing, with the folder's tasks stopped: this takes
    /// the tree lock, and only a cycle, which those stops end, holds the tree lock while it
    /// waits for `lifecycle` (`docs/design/writes.md` §9).
    pub(super) async fn drop_outbox(&self) {
        let store = self.store.lock().unwrap().clone();
        let root = self.registration().map(|reg| reg.root);
        let (Some(store), Some(root)) = (store, root) else { return };
        let (dropping, marked) = (store.clone(), root.clone());
        // Under the tree lock: a cycle's swap must not give a moved-out item back the object
        // it forgets here.
        let tree = self.tree_lock.lock().await;
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
        drop(tree);
        // The upload sessions of the rows dropped are given up: cancelled now, so that no
        // empty placeholder keeps a name in OneDrive (issue #47). One that fails stays listed
        // for the worker of a later read-write start.
        let drive = self.drive.lock().unwrap().clone();
        if let Some(drive) = drive {
            upload::cancel_given_up(&store, &drive, DROPPED_CANCELS).await;
        }
        self.clear_outbox_counts();
        // What moves out of the folder left outside it is tidied.
        self.forget_moved_out();
        if let Ok(Ok(rows)) = &dropped {
            self.tidy_dropped(&root, &store, rows).await;
        }
        match dropped.map(|rows| rows.map(|rows| rows.len())) {
            Ok(Ok(0)) => {}
            Ok(Ok(n)) => tracing::info!("{n} change(s) waiting to upload were dropped; the files stay as they are"),
            Ok(Err(e)) => tracing::warn!("cannot drop the changes waiting to upload: {e}"),
            Err(e) => tracing::warn!("the task dropping the changes waiting to upload failed: {e}"),
        }
    }

    /// How many changes wait in this folder's tree store — the running
    /// sync's, or with none running, the one on disk its next sync opens — for a Forget and
    /// `Accounts.Remove`, which would delete them with the store. Only read, so it needs no
    /// lock. A running store that cannot be read refuses; one on disk that cannot be opened
    /// holds nothing a sync could send (it is rebuilt empty).
    pub(super) async fn changes_in_store(&self) -> Result<u64, SyncError> {
        let running = self.store.lock().unwrap().clone();
        if let Some(store) = running {
            return store
                .call(|s| s.outbox_len())
                .await
                .map(|n| n as u64)
                .map_err(|e| SyncError::Io(format!("cannot tell whether changes wait to be uploaded: {e}")));
        }
        let Some(tree_db) = self.sync_paths.lock().unwrap().as_ref().map(|p| p.tree_db.clone()) else { return Ok(0) };
        if !tree_db.exists() {
            return Ok(0);
        }
        let counted = tokio::task::spawn_blocking(move || konedrive_tree::TreeStore::open_read_only(&tree_db).and_then(|s| s.outbox_len()))
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

/// The outbox worker's view of its account's sync: live activity, its
/// status in the published state, and a cycle when OneDrive changed under a
/// row.
pub(super) struct Host {
    sync: Weak<SyncService>,
}

impl Host {
    pub(super) fn new(sync: Weak<SyncService>) -> Self {
        Self { sync }
    }
}

impl OutboxHost for Host {
    /// `ActivityLog.Added`: the worker has written the event into the store with
    /// its commit.
    fn activity(&self, event: &ActivityRow) {
        if let Some(service) = self.sync.upgrade() {
            service.report.activity.announce(event.clone());
        }
    }

    /// `NotUploadedSummary()`'s answer from now on.
    fn kept_back(&self, summary: &[crate::upload::kept_back::SummaryRow]) {
        if let Some(service) = self.sync.upgrade() {
            *service.kept_back.lock().unwrap() = Some(summary.to_vec());
        }
    }

    /// `PendingCount`, `PendingBytes`, `BlockedCount` and `Uploads`.
    fn status(&self, status: &WorkerStatus) {
        let Some(service) = self.sync.upgrade() else { return };
        let root = service.registration().map(|reg| reg.root.path).unwrap_or_default();
        let uploads: Vec<(String, u64, u64)> =
            status.uploads.iter().map(|u| (root.join(&u.rel).display().to_string(), u.sent, u.total)).collect();
        service.state.update(|s| {
            s.pending_count = status.counts.pending;
            s.pending_bytes = status.counts.pending_bytes;
            s.blocked_count = status.counts.blocked;
            s.held_count = status.counts.held;
            s.uploads = uploads;
            s.quota_full = status.quota_full;
            s.space_waiting_count = status.counts.space_waiting;
            s.space_waiting_bytes = status.counts.space_waiting_bytes;
            s.too_big_count = status.counts.too_big;
            s.too_big_bytes = status.counts.too_big_bytes;
        });
    }

    /// OneDrive changed under a row (`docs/design/writes.md` §7), or a folder a row
    /// needs is gone there: the next cycle comes now, and its delta carries
    /// the change (the outbox on the bus: no Full reconcile).
    fn cycle_wanted(&self) {
        if let Some(service) = self.sync.upgrade() {
            service.nudge();
        }
    }

    /// An item's local object was forgotten (delete × edit, a folder deleted
    /// only in part): the next cycle comes now, with a Full reconcile, so
    /// that it is placed again though the delta may have carried it already.
    fn full_cycle_wanted(&self) {
        if let Some(service) = self.sync.upgrade() {
            service.nudge_full();
        }
    }

    /// The one place that decides what runs (`running`).
    fn stopped(&self, store: &Store) -> bool {
        match self.sync.upgrade() {
            Some(service) => service.running.stopped(store),
            None => crate::conditions::running::user_pause(store).is_some(),
        }
    }

    /// The write gate, asked again before each row.
    fn may_write(&self) -> Result<(), String> {
        match self.sync.upgrade() {
            Some(service) => service.write_gate(),
            None => Err("the folder's sync is gone".into()),
        }
    }
}

#[cfg(test)]
mod tests;
