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

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;

use super::{applying, cancellable, drive_error, record, record_drive, CycleError, Commit, Fetched, Listing, Reconciled, Said, Turn};
use crate::drive::DriveError;
use crate::sync::activity::{self, Kind as EventKind};
use crate::sync::disk::{rescue_base, rescue_stamp, Disk};
use crate::sync::local::Batch;
use crate::sync::materialize::{Applied, ApplyError, Materializer, Rw, Scope};
use crate::tree::outbox::OutboxRow;
use crate::tree::{classify, Change, Table};

/// A read-write folder's cycle: what it shares with the folder's outbox
/// worker and watcher.
pub struct Writes {
    /// The per-root tree lock (`SyncService::tree_lock`): held from staging
    /// to the swap, as the outbox worker holds it across each commit.
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// For conflict copies (§6): `name-<machine>.ext`.
    pub machine_name: String,
    /// The account's ignore list: an ignored name is no local work that
    /// makes a folder removed in OneDrive come back.
    pub ignore: crate::sync::local::ignore::SharedIgnore,
    /// Says when the watcher has examined the folder once (its Full local
    /// scan): the folder's first cycle waits for it (§3.3). `None`, or a
    /// watcher that stopped first, holds nothing back.
    pub scanned: Option<tokio::sync::watch::Receiver<bool>>,
    /// Hands the watcher places to examine: what the reconcile kept,
    /// copied or made local.
    pub examine: Arc<dyn Fn(Batch) + Send + Sync>,
    /// A cycle went through: the outbox worker may send (§4.9).
    pub cycled: Arc<dyn Fn() + Send + Sync>,
    /// A held or pending `delete` or `move-out` row was dropped because its
    /// item is already gone from OneDrive ([`TreeStore::outbox_drop_removed`]):
    /// wakes the outbox worker at once, so `HeldCount`/`PendingCount` and the
    /// bus signal count it gone without waiting for the worker's own timer,
    /// and tidies a dropped `move-out`'s placeholder outside the folder
    /// (`Tidy::dropped`, `sync::upload::move_out`), off the runtime the
    /// reconcile's blocking task captured.
    ///
    /// [`TreeStore::outbox_drop_removed`]: crate::tree::TreeStore::outbox_drop_removed
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

/// What a read-write reconcile carries from its staging to its swap.
pub(super) struct RwCycle {
    /// The tree lock, taken before `staging` was begun.
    pub tree: OwnedMutexGuard<()>,
    /// `outbox_seq` as the fetch started: what is deferred is dated by it.
    pub fetch_seq: i64,
    /// The deferred changes staged again at the start: done with at the swap,
    /// or deferred anew.
    pub consumed: Vec<String>,
    pub upload_differences: bool,
}

impl Listing {
    pub(super) fn writes(&self) -> &Writes {
        self.ctx.writes.as_ref().expect("a read-write folder's cycle")
    }

    /// The tree lock, as the cycle waits for it.
    pub(super) async fn tree_lock(&self, cancel: &CancellationToken) -> Result<OwnedMutexGuard<()>, CycleError> {
        cancellable(cancel, Arc::clone(&self.writes().tree_lock).lock_owned()).await
    }

    /// What was fetched, staged and reconciled in read-write mode; with the
    /// number of entries the delta had.
    pub(super) async fn reconcile_rw_fetched(
        &self,
        turn: &Turn,
        fetched: Fetched,
        fetch_seq: i64,
        full_requested: bool,
        cancel: &CancellationToken,
    ) -> Result<(Reconciled, usize), CycleError> {
        match fetched {
            Fetched::Placed(placed) => Ok((placed, 0)),
            Fetched::Listed { link, upload_differences } => {
                let tree = self.tree_lock(cancel).await?;
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
                let rw = RwCycle { tree, fetch_seq, consumed, upload_differences };
                Ok((self.reconcile_rw(turn, Scope::Full, Commit::Swap { link, listing: false }, rw, cancel).await?, 0))
            }
            Fetched::Changes { changes, link } => {
                let count = changes.len();
                let tree = self.tree_lock(cancel).await?;
                let changes = self.guard_delta(turn, changes, fetch_seq, cancel).await?;
                let since = self.revisit_from.load(Ordering::SeqCst);
                let staged = self
                    .on_store(turn, move |s| {
                        let deferred = s.live_deferred()?;
                        let rows: BTreeSet<String> = s.outbox_rows()?.into_iter().filter_map(|row| row.item_id).collect();
                        let revisit = s.committed_items_since(since)?;
                        let unplaced = s.unplaced(Table::Items)?;
                        let waiting = deferred.iter().all(|c| rows.contains(c.id()));
                        if !full_requested && changes.is_empty() && waiting && revisit.is_empty() && unplaced.is_empty() {
                            return Ok(None);
                        }
                        let consumed: Vec<String> = deferred.iter().map(|c| c.id().to_owned()).collect();
                        s.begin_staging(true)?;
                        s.stage(&deferred)?;
                        s.stage(&changes)?;
                        let mut ids: BTreeSet<String> = s.changed_ids()?.into_iter().collect();
                        ids.extend(revisit);
                        ids.extend(s.unplaced(Table::Staging)?);
                        Ok(Some((ids.into_iter().collect::<Vec<_>>(), consumed)))
                    })
                    .await?;
                let Some((ids, consumed)) = staged else {
                    self.on_store(turn, move |s| s.set_meta("delta_link", Some(&link))).await?;
                    return Ok((Reconciled::default(), count));
                };
                let scope = if full_requested || count > self.ctx.full_threshold { Scope::Full } else { Scope::Changed(ids) };
                let rw = RwCycle { tree, fetch_seq, consumed, upload_differences: false };
                Ok((self.reconcile_rw(turn, scope, Commit::Swap { link, listing: false }, rw, cancel).await?, count))
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

    /// [`reconcile`](Listing::reconcile) for a read-write folder: the
    /// materializer with read-write mode's rules, and the tree lock held
    /// until the swap. What the reconcile left as it is on disk is deferred
    /// at the swap; then the conflict copies are recorded, the watcher told
    /// what to examine, and rows let wait for a folder made again.
    pub(super) async fn reconcile_rw(&self, turn: &Turn, scope: Scope, commit: Commit, rw: RwCycle, cancel: &CancellationToken) -> Result<Reconciled, CycleError> {
        let lifecycle = cancellable(cancel, Arc::clone(&self.ctx.lifecycle).read_owned()).await?;
        let link = self.ctx.link.lock().unwrap().clone().filter(|_| self.ctx.intercepted);
        if link.is_none() {
            return Err(CycleError::NoHelper);
        }
        let RwCycle { tree, fetch_seq, consumed, upload_differences } = rw;
        let held = (Arc::clone(turn), lifecycle, tree);
        let (root, preferred, store) = (self.ctx.root.clone(), self.ctx.rescue_dir.clone(), self.ctx.store.clone());
        let (locks, cancel, locked) = (self.ctx.locks.clone(), cancel.clone(), self.ctx.locked);
        let report = self.ctx.report.clone();
        let runtime = tokio::runtime::Handle::current();
        let drive = self.pending_drive.lock().unwrap().take();
        let writes = self.writes();
        let (machine, examine) = (writes.machine_name.clone(), Arc::clone(&writes.examine));
        let dropped_removed = Arc::clone(&writes.dropped_removed);
        let ignore = writes.ignore.read().unwrap_or_else(|p| p.into_inner()).clone();
        tokio::task::spawn_blocking(move || {
            let _held = held;
            if let Some((record, id)) = drive {
                record_drive(&record, &id);
                if let Err(e) = crate::sync::root::mark_drive(&root, &id) {
                    tracing::warn!("cannot record the drive on {}: {e}", root.path.display());
                }
            }
            let Some(root_item_id) = store.with(|s| s.root_item_id()).map_err(|e| applying(e.into()))? else {
                return match commit {
                    Commit::Page { changes, next } => {
                        store.with(|s| s.commit_page(&changes, &next))?;
                        Ok(Reconciled::default())
                    }
                    Commit::Swap { .. } => Err(CycleError::Apply("the drive's listing has no root".into())),
                };
            };
            let plan = store.with(|s| Rw::read(s, machine, upload_differences, ignore))?;
            let materializer = Materializer {
                disk: Disk::open(&root, locked).map_err(|e| applying(e.into()))?,
                store: store.clone(),
                link,
                runtime,
                locks,
                root_item_id,
                rescue_into: rescue_base(&root.path, &preferred).join(rescue_stamp(SystemTime::now())),
                cancel,
                rw: Some(plan.clone()),
                // Read-write mode removes no object whose id the base does not know (F115).
                claimed: None,
            };
            let changed = matches!(scope, Scope::Changed(_));
            let mut first = Applied::default();
            // Where what the first pass moved to the holding directory came
            // from: the second pass puts back there what it does not place.
            let mut moved_from = std::collections::HashMap::new();
            let (mut applied, full) = match materializer.apply_handing_over(scope, &mut first, &mut moved_from) {
                Err(ApplyError::NeedFull(why) | ApplyError::Io(why)) if changed => {
                    tracing::info!("{why}; reconciling the whole folder");
                    (materializer.apply_handing_over(Scope::Full, &mut first, &mut moved_from).map_err(applying)?, true)
                }
                other => (other.map_err(applying)?, !changed),
            };
            // What the first pass did on disk stands whatever the second did.
            first.rescued.append(&mut applied.rescued);
            applied.rescued = first.rescued;
            first.copies.append(&mut applied.copies);
            applied.copies = first.copies;
            first.examine.append(&mut applied.examine);
            applied.examine = first.examine;
            first.recreated.append(&mut applied.recreated);
            applied.recreated = first.recreated;
            let said = match commit {
                Commit::Swap { link, listing } => {
                    // What the disk does not show yet keeps its base; its
                    // change waits (the read-write reconcile must, items 3 and 4).
                    let changed = store.with(|s| s.changed_ids())?;
                    let defer: Vec<String> = changed
                        .iter()
                        .filter(|id| !plan.removing.contains(*id) && (plan.held.contains(*id) || applied.unsettled.contains(*id)))
                        .cloned()
                        .collect();
                    // Only the content waits where the disk took the rest.
                    let content: Vec<String> = changed
                        .iter()
                        .filter(|id| !plan.removing.contains(*id) && !defer.contains(id) && applied.content_waits.contains(*id))
                        .cloned()
                        .collect();
                    if !defer.is_empty() || !content.is_empty() {
                        tracing::debug!("{} change(s) wait for the folder to take them", defer.len() + content.len());
                    }
                    store.with(|s| s.commit_staging_deferring(&link, &consumed, &defer, &content, fetch_seq))?;
                    if listing || full {
                        Said::Listed
                    } else {
                        Said::EachChange
                    }
                }
                Commit::Page { changes, next } => {
                    store.with(|s| s.commit_page(&changes, &next))?;
                    Said::Nothing
                }
            };
            if !applied.recreated.is_empty() {
                // Rows into a folder made again wait for its `mkdir` (F82 (4)).
                if let Err(e) = store.with(|s| s.outbox_detach_parents(&applied.recreated)) {
                    tracing::warn!("cannot let the outbox wait for folders made again: {e}");
                }
            }
            // `items` just took this cycle's answer: a held or pending
            // `delete`/`move-out` row whose item is not in it any more has
            // nothing left to send (the fix for a held delete outliving the
            // item's own removal in OneDrive).
            match store.with(|s| s.outbox_drop_removed()) {
                Ok(dropped) if !dropped.is_empty() => {
                    let events = dropped
                        .iter()
                        .map(|row| activity::event(EventKind::Removed, root.path.join(&row.rel).display().to_string(), "already removed in OneDrive"))
                        .collect();
                    report.activity.record_blocking(events);
                    dropped_removed(dropped);
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("cannot drop held or pending removals of items already gone from OneDrive: {e}"),
            }
            record(&report, &store, &root.path, &applied, said);
            if !applied.examine.is_empty() {
                let mut batch = Batch::new();
                for (rel, below) in &applied.examine {
                    match (below, rel.parent(), rel.file_name()) {
                        (true, _, _) => batch.tree(rel),
                        (false, Some(parent), Some(name)) => batch.name(parent, name),
                        _ => {}
                    }
                }
                examine(batch);
            }
            Ok(Reconciled { applied, full })
        })
        .await
        .map_err(|e| CycleError::Apply(format!("the reconcile task failed: {e}")))?
    }
}

#[cfg(test)]
mod tests;
