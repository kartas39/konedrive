//! A read-write folder's cycle (`docs/design/writes.md` §9): the same
//! fetch, stage, reconcile and swap as the read phase's, with three more
//! rules around them.
//!
//! - **The tree lock.** The cycle holds the per-root tree lock — the one the
//!   outbox worker holds across each commit — from before `staging` is begun
//!   until after it is swapped in, with the reconcile's changes to the folder
//!   in between (the read-write reconcile must, item 2). `commit_staging` replaces `items` with
//!   `staging`, so a commit made in between would be reverted.
//! - **The stale-delta guard.** The cycle notes the outbox's commit count
//!   (`outbox_seq`) as its fetch starts. An entry for an item the outbox
//!   committed after that — or deleted, by its tombstone — may be older than
//!   the commit, or newer: it is read again from Graph, under the lock, and
//!   that answer is staged instead, newer than both.
//! - **What waits.** An item the reconcile leaves as it is on disk (see
//!   `materialize::Rw`) keeps its base row; its staged change is deferred and
//!   staged again at every cycle, until the disk takes it or an outbox commit
//!   supersedes it. Items the outbox committed since the last cycle, and
//!   items with no local object on record, are looked at again too, so the
//!   disk follows the base (F82 (7), (8)).
//!
//! Its reconcile then records the conflict copies it made, hands the
//! watcher what it kept or copied — the daemon's own changes raise no event
//! the watcher keeps — lets rows wait for a folder made again, and says the
//! cycle went through, so that the outbox worker sends (§4.9).

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;

use super::reconcile::{Commit, Mode, Reconciled, RwCycle, Waiting};
use super::{cancellable, drive_error, CycleError, Fetched, Listing, Turn};
use crate::folder::classify::classify;
use crate::local::Batch;
use crate::remote::materialize::Scope;
use konedrive_graph::drive::DriveError;
use konedrive_tree::outbox::OutboxRow;
use konedrive_tree::reconcile::RwStaged;
use konedrive_tree::Change;

/// A read-write folder's cycle: what it shares with the folder's outbox
/// worker and watcher.
pub struct Writes {
    /// The per-root tree lock (`SyncService::tree_lock`): held from staging
    /// to the swap, as the outbox worker holds it across each commit.
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// For conflict copies (§6): `name-<machine>.ext`.
    pub machine_name: String,
    /// The account's ignore list: an ignored name is no upload a folder that
    /// stopped being placed waits for.
    pub ignore: crate::local::ignore::SharedIgnore,
    /// Says when the watcher has examined the folder once (its Full local
    /// scan): the folder's first cycle waits for it (§3.3). `None`, or a
    /// watcher that stopped first, holds nothing back.
    pub scanned: Option<tokio::sync::watch::Receiver<bool>>,
    /// Hands the watcher places to examine: what the reconcile kept,
    /// copied or made local.
    pub examine: Arc<dyn Fn(Batch) + Send + Sync>,
    /// A cycle went through: the outbox worker may send (§4.9).
    pub cycled: Arc<dyn Fn() + Send + Sync>,
    /// A cycle cleared trouble that stopped the folder ([`CycleError::blocking`]) — it
    /// went through, or failed with trouble that is only said: the write gate that
    /// trouble closed is open again, and
    /// the outbox worker that met it closed is woken, rather than left to its own timer.
    pub reopened: Arc<dyn Fn() + Send + Sync>,
    /// A held or pending `delete` or `move-out` row was dropped because its
    /// item is already gone from OneDrive ([`TreeStore::outbox_drop_removed`]):
    /// wakes the outbox worker at once, so `HeldCount`/`PendingCount` and the
    /// bus signal count it gone without waiting for the worker's own timer,
    /// and tidies a dropped `move-out`'s placeholder outside the folder
    /// (`Tidy::dropped`, `upload::move_out`), off the runtime the
    /// reconcile's blocking task captured.
    ///
    /// [`TreeStore::outbox_drop_removed`]: konedrive_tree::TreeStore::outbox_drop_removed
    pub dropped_removed: Arc<dyn Fn(Vec<OutboxRow>) + Send + Sync>,
}

impl Writes {
    /// Waits for the watcher's first examination, unless it is done or the
    /// watcher went.
    pub(super) async fn scanned(&self, cancel: &CancellationToken) -> Result<(), CycleError> {
        let Some(mut scanned) = self.scanned.clone() else { return Ok(()) };
        let _ = cancellable(cancel, scanned.wait_for(|done| *done)).await?;
        Ok(())
    }
}

impl Listing {
    /// A read-write folder's part in uploading; `None` for a read-only folder.
    pub(crate) fn writes(&self) -> Option<&Writes> {
        self.ctx.writes.as_ref()
    }

    /// The tree lock of a read-write folder (`writes`), as the cycle waits for it.
    pub(super) async fn tree_lock(&self, writes: &Writes, cancel: &CancellationToken) -> Result<OwnedMutexGuard<()>, CycleError> {
        let lock = Arc::clone(&writes.tree_lock).lock_owned();
        #[cfg(test)]
        let lock = self.waiting_said(lock);
        cancellable(cancel, lock).await
    }

    /// Tests only: `lock`, with [`waits_for_tree`](Self::waits_for_tree) true from the poll
    /// that left it queued until it is taken or given up.
    #[cfg(test)]
    async fn waiting_said<T>(&self, lock: impl std::future::Future<Output = T>) -> T {
        let mut lock = std::pin::pin!(lock);
        let _over = super::OnDrop(Some(|| self.waits_for_tree.store(false, Ordering::SeqCst)));
        std::future::poll_fn(|cx| {
            let polled = lock.as_mut().poll(cx);
            self.waits_for_tree.store(polled.is_pending(), Ordering::SeqCst);
            polled
        })
        .await
    }

    /// Tests only: whether a cycle is queued for the tree lock right now, behind whoever
    /// holds it.
    #[cfg(test)]
    pub(crate) fn waits_for_tree(&self) -> bool {
        self.waits_for_tree.load(Ordering::SeqCst)
    }

    /// What was fetched, staged and reconciled in read-write mode (the
    /// folder's `writes`); with the number of entries the delta had.
    pub(super) async fn reconcile_rw_fetched(
        &self,
        turn: &Turn,
        writes: &Writes,
        fetched: Fetched,
        fetch_seq: i64,
        full_requested: bool,
        cancel: &CancellationToken,
    ) -> Result<(Reconciled, usize), CycleError> {
        match fetched {
            Fetched::Placed(placed) => Ok((placed, 0)),
            Fetched::Listed { link, upload_differences } => {
                let tree = self.tree_lock(writes, cancel).await?;
                // `staging` holds the whole new listing: what the outbox
                // committed since the listing began is read again.
                let since = self.on_store(turn, move |s| s.committed_since(fetch_seq)).await?;
                let mut fresh = Vec::new();
                for id in since.into_keys() {
                    fresh.push(self.fresh(&id, cancel).await?);
                }
                // The listing is newer than anything deferred.
                let consumed = self.on_store(turn, move |s| {
                    s.stage_over(&fresh)?;
                    s.deferred_ids()
                })
                .await?;
                let rw = RwCycle { writes, tree, upload_differences, waiting: Waiting { fetch_seq, consumed, whole_listing: true, brought: Vec::new() } };
                Ok((self.reconcile(turn, Mode::ReadWrite(rw), Scope::Full, Commit::Swap { link, listing: false }, cancel).await?, 0))
            }
            Fetched::Changes { changes, link } => {
                let count = changes.len();
                let tree = self.tree_lock(writes, cancel).await?;
                let changes = self.guard_delta(turn, changes, fetch_seq, cancel).await?;
                let since = self.revisit_from.load(Ordering::SeqCst);
                let brought: Vec<String> = changes.iter().map(|c| c.id().to_owned()).collect();
                let staged = self.on_store(turn, move |s| s.stage_rw(&changes, since, full_requested)).await?;
                let Some(RwStaged { ids, consumed }) = staged else {
                    self.on_store(turn, move |s| s.set_delta_link(&link)).await?;
                    return Ok((Reconciled::default(), count));
                };
                let scope = if full_requested || count > self.ctx.full_threshold { Scope::Full } else { Scope::Changed(ids) };
                let rw = RwCycle { writes, tree, upload_differences: false, waiting: Waiting { fetch_seq, consumed, whole_listing: false, brought } };
                Ok((self.reconcile(turn, Mode::ReadWrite(rw), scope, Commit::Swap { link, listing: false }, cancel).await?, count))
            }
        }
    }

    /// The stale-delta guard (§3.7): each entry for an item the outbox
    /// committed after `fetch_seq` is read again from Graph, unless it is the
    /// commit itself (the same eTag).
    async fn guard_delta(&self, turn: &Turn, changes: Vec<Change>, fetch_seq: i64, cancel: &CancellationToken) -> Result<Vec<Change>, CycleError> {
        let committed = self.on_store(turn, move |s| s.committed_since(fetch_seq)).await?;
        if committed.is_empty() {
            return Ok(changes);
        }
        let mut out = Vec::with_capacity(changes.len());
        for change in changes {
            let stale = match (committed.get(change.id()), &change) {
                (None, _) => false,
                (Some(commit), Change::Upsert(row)) => commit.gone || row.etag.is_none() || row.etag != commit.etag,
                (Some(_), _) => true,
            };
            if stale {
                tracing::debug!("{} was committed while the delta was fetched: it is read again", change.id());
                out.push(self.fresh(change.id(), cancel).await?);
            } else {
                out.push(change);
            }
        }
        Ok(out)
    }

    /// Item `id` as Graph has it now: an upsert, or a delete.
    async fn fresh(&self, id: &str, cancel: &CancellationToken) -> Result<Change, CycleError> {
        match cancellable(cancel, self.ctx.drive.item(id)).await? {
            Ok(item) => Ok(classify(&item)),
            Err(DriveError::NotFound) => Ok(Change::Delete(id.to_owned())),
            Err(e) => Err(drive_error(e)),
        }
    }
}

#[cfg(test)]
mod tests;
