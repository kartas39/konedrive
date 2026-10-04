//! What OneDrive said, staged: the step between a cycle's fetch and its
//! reconcile, in either mode.
//!
//! [`Listing::stage`] puts a delta's changes into `staging` (a whole listing
//! was staged page by page as it was fetched) and says which reconcile they
//! ask for: its scope, its commit, and what a read-write one carries to its
//! swap. A read-write folder's staging has three more rules
//! (`docs/design/writes.md` §9):
//!
//! - **The tree lock** is taken before `staging` is written and goes on to
//!   the reconcile, which holds it until the swap: `commit_staging` replaces
//!   `items` with `staging`, so an outbox commit made in between would be
//!   reverted.
//! - **The stale-delta guard.** An entry for an item the outbox committed
//!   after the fetch began — or deleted, by its tombstone — may be older than
//!   the commit, or newer: it is read again from Graph, under the lock, and
//!   that answer is staged instead, newer than both.
//! - **What waits is staged again**, before the delta: an item the reconcile
//!   left as it is on disk keeps its base row, and its change is deferred
//!   until the disk takes it or an outbox commit supersedes it. Items the
//!   outbox committed since the last cycle, and items with no local object
//!   on record, are looked at again too, so the disk follows the base (F82
//!   (7), (8)).

use std::sync::atomic::Ordering;

use tokio_util::sync::CancellationToken;

use super::reconcile::{Commit, Reconciled, RwCycle, Waiting};
use super::{cancellable, drive_error, CycleError, Listing, Turn, Writes};
use crate::folder::classify::classify;
use crate::remote::materialize::Scope;
use crate::remote::mode::Mode;
use konedrive_graph::drive::DriveError;
use konedrive_tree::reconcile::RwStaged;
use konedrive_tree::Change;

/// What a read-write folder's cycle goes by from its start.
#[derive(Clone, Copy)]
pub(crate) struct Since<'a> {
    /// The folder's part in uploading.
    pub writes: &'a Writes,
    /// The outbox's commit count as the cycle's fetch started: the
    /// stale-delta guard and what is deferred are dated by it.
    pub fetch_seq: i64,
}

/// What the feed said since the stored link.
// Made once per cycle and taken apart at once: not worth a box.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Fetched {
    /// A first listing, placed page by page and committed with its link:
    /// nothing is left to stage or to reconcile.
    Placed(Reconciled),
    /// What is still to be staged and reconciled.
    News(News),
}

/// What OneDrive said that the folder has still to be made to match.
pub(crate) enum News {
    /// A full listing is in `staging`: the first into a folder that shows
    /// the drive already, one after `410`, or one after a first listing's
    /// resume link was refused.
    Listed {
        link: String,
        /// `410` with `resyncChangesUploadDifferences`: a read-write
        /// folder uploads what the listing left out.
        upload_differences: bool,
    },
    Changes { changes: Vec<Change>, link: String },
}

/// The reconcile that what was staged asks for.
pub(crate) struct Staged<'a> {
    pub mode: Mode<RwCycle<'a>>,
    pub scope: Scope,
    pub commit: Commit,
}

impl Listing {
    /// The mode of the cycle that starts now. A read-write folder's first
    /// cycle waits for the watcher's Full local scan (write design §3.3),
    /// and its stale-delta guard starts from the outbox's commits so far.
    pub(crate) async fn begin(&self, turn: &Turn, cancel: &CancellationToken) -> Result<Mode<Since<'_>>, CycleError> {
        let Mode::ReadWrite(writes) = &self.ctx.mode else { return Ok(Mode::ReadOnly) };
        writes.scanned(cancel).await?;
        let fetch_seq = self.on_store(turn, |s| s.outbox_seq()).await?;
        Ok(Mode::ReadWrite(Since { writes, fetch_seq }))
    }

    /// Stages what OneDrive said: the reconcile it asks for, with the
    /// number of entries a delta had. No reconcile when nothing changed and
    /// nothing is to be looked at again: the new link is stored, and that
    /// is the cycle. `full_requested`: the reconcile is Full whatever the
    /// delta holds.
    pub(crate) async fn stage<'a>(&self, turn: &Turn, mode: Mode<Since<'a>>, news: News, full_requested: bool, cancel: &CancellationToken) -> Result<(Option<Staged<'a>>, usize), CycleError> {
        let swap = |link| Commit::Swap { link, listing: false };
        match (news, mode) {
            (News::Listed { link, .. }, Mode::ReadOnly) => Ok((Some(Staged { mode: Mode::ReadOnly, scope: Scope::Full, commit: swap(link) }), 0)),
            (News::Changes { changes, link }, Mode::ReadOnly) => {
                let count = changes.len();
                if count > 0 || full_requested {
                    self.on_store(turn, move |s| {
                        s.begin_staging(konedrive_tree::NewTree::Delta)?;
                        s.stage(&changes)
                    })
                    .await?;
                }
                let scope = if full_requested || count > self.ctx.full_threshold {
                    Scope::Full
                } else if count == 0 {
                    self.on_store(turn, move |s| s.set_delta_link(&link)).await?;
                    return Ok((None, count));
                } else {
                    // What `staging` now differs from `items` by: the delta
                    // just staged, read back so the closure above could own it.
                    Scope::Changed(self.on_store(turn, |s| s.changed_ids()).await?)
                };
                Ok((Some(Staged { mode: Mode::ReadOnly, scope, commit: swap(link) }), count))
            }
            (News::Listed { link, upload_differences }, Mode::ReadWrite(Since { writes, fetch_seq })) => {
                let tree = self.tree_lock(writes, cancel).await?;
                // `staging` holds the whole new listing: what the outbox
                // committed since the listing began is read again.
                let since = self.on_store(turn, move |s| s.committed_since(fetch_seq)).await?;
                let mut fresh = Vec::new();
                for id in since.into_keys() {
                    fresh.push(self.fresh(&id, cancel).await?);
                }
                // The listing is newer than anything deferred.
                let consumed = self
                    .on_store(turn, move |s| {
                        s.stage_over(&fresh)?;
                        s.deferred_ids()
                    })
                    .await?;
                let rw = RwCycle { writes, tree, upload_differences, waiting: Waiting { fetch_seq, consumed } };
                Ok((Some(Staged { mode: Mode::ReadWrite(rw), scope: Scope::Full, commit: swap(link) }), 0))
            }
            (News::Changes { changes, link }, Mode::ReadWrite(Since { writes, fetch_seq })) => {
                let count = changes.len();
                let tree = self.tree_lock(writes, cancel).await?;
                let changes = self.guard_delta(turn, changes, fetch_seq, cancel).await?;
                let since = self.revisit_from.load(Ordering::SeqCst);
                let staged = self.on_store(turn, move |s| s.stage_rw(&changes, since, full_requested)).await?;
                let Some(RwStaged { ids, consumed }) = staged else {
                    self.on_store(turn, move |s| s.set_delta_link(&link)).await?;
                    return Ok((None, count));
                };
                let scope = if full_requested || count > self.ctx.full_threshold { Scope::Full } else { Scope::Changed(ids) };
                let rw = RwCycle { writes, tree, upload_differences: false, waiting: Waiting { fetch_seq, consumed } };
                Ok((Some(Staged { mode: Mode::ReadWrite(rw), scope, commit: swap(link) }), count))
            }
        }
    }

    /// What was fetched, staged and reconciled; with the number of entries
    /// the delta had.
    pub(super) async fn reconcile_fetched(&self, turn: &Turn, mode: Mode<Since<'_>>, fetched: Fetched, full_requested: bool, cancel: &CancellationToken) -> Result<(Reconciled, usize), CycleError> {
        let news = match fetched {
            Fetched::Placed(placed) => return Ok((placed, 0)),
            Fetched::News(news) => news,
        };
        let (staged, changes) = self.stage(turn, mode, news, full_requested, cancel).await?;
        let reconciled = match staged {
            None => Reconciled::default(),
            Some(Staged { mode, scope, commit }) => self.reconcile(turn, mode, scope, commit, cancel).await?,
        };
        if let Mode::ReadWrite(since) = mode {
            // Items the outbox committed after this cycle's fetch began are
            // looked at again by the next cycle.
            self.revisit_from.store(since.fetch_seq, Ordering::SeqCst);
        }
        Ok((reconciled, changes))
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
