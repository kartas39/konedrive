use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::folder::disk::Disk;
use crate::remote::materialize::Scope;
use konedrive_graph::drive::{DeltaFrom, DeltaNext, DriveError};
use crate::folder::classify::classify;
use konedrive_tree::{Change, Table};
use super::rw::RwCycle;
use super::{applying, cancellable, drive_error, refused, Commit, CycleError, Fetched, Listing, OnDrop, Reconciled, Turn};

impl Listing {
    /// A full listing into `staging`, page by page, publishing its progress.
    pub(super) async fn list_all(&self, turn: &Turn, cancel: &CancellationToken) -> Result<Fetched, CycleError> {
        self.ctx.state.update(|s| {
            s.listing = true;
            s.items_listed = 0;
        });
        // However the listing ends — also when its future is dropped.
        let _said = OnDrop(Some(|| self.ctx.state.update(|s| s.listing = false)));
        self.list_all_pages(turn, cancel).await
    }

    async fn list_all_pages(&self, turn: &Turn, cancel: &CancellationToken) -> Result<Fetched, CycleError> {
        self.on_store(turn, |s| s.begin_staging(konedrive_tree::NewTree::Whole)).await?;
        let mut from = DeltaFrom::Start;
        let mut listed = 0u64;
        loop {
            let page = cancellable(cancel, self.ctx.drive.delta(&from)).await?.map_err(drive_error)?;
            let changes: Vec<Change> = page.items.iter().map(classify).collect();
            listed += changes.iter().filter(|c| !matches!(c, Change::Root(_) | Change::Delete(_))).count() as u64;
            self.on_store(turn, move |s| s.stage(&changes)).await?;
            self.ctx.state.update(|s| s.items_listed = listed);
            match page.next {
                DeltaNext::Page(next) => from = DeltaFrom::Link(next),
                DeltaNext::Done(link) => return Ok(Fetched::Listed { link, upload_differences: false }),
            }
        }
    }

    /// Whether a first listing can be placed page by page: `items`
    /// is empty, and nothing in the folder carries an item id — no
    /// placeholder, no directory of ours, nothing waiting in the holding
    /// directory. Asked once, when such a listing begins; from then on
    /// `listing_next` says it is under way.
    ///
    /// A folder that shows the drive already — its tree store lost, or
    /// forgotten and registered again — is listed whole and reconciled once
    /// at the end instead ([`Self::list_all`]): part-way through a listing,
    /// an item of ours not listed yet cannot be told from one that is gone,
    /// and only the end of the listing tells them apart. It is found by its
    /// id then, downloaded content and all, rather than removed and made
    /// again. Read without the lifecycle lock: it changes nothing.
    pub(super) async fn holds_nothing_yet(&self, turn: &Turn) -> Result<bool, CycleError> {
        if !self.on_store(turn, |s| s.is_empty(Table::Items)).await? {
            return Ok(false);
        }
        let (root, held) = (self.ctx.root.clone(), Arc::clone(turn));
        tokio::task::spawn_blocking(move || {
            let _turn = held;
            let scanned = Disk::open(&root, false)?.scan("")?;
            Ok::<_, std::io::Error>(scanned.iter().all(|entry| entry.id.is_none()))
        })
        .await
        .map_err(|e| CycleError::Apply(format!("the task looking at the folder failed: {e}")))?
        .map_err(|e| applying(e.into()))
    }

    /// A first listing placed page by page, from the start or from
    /// where a stopped one got to (`from`, the link `items` was last
    /// committed with). Each page is staged, then placed and committed into `items`
    /// together with the link to the page after it — under the lifecycle
    /// lock, which is let go while Graph is asked for the next page, so that
    /// a Forget or a helper's reconnect waits for one page at most. An entry
    /// whose folder has not come yet waits in `items` (and so in `staging`)
    /// and is placed when its folder is; one whose folder never
    /// comes is left as a Full reconcile leaves it — listed, not placed. The
    /// last page swaps `staging` in with the delta link, which ends the
    /// listing, and says the one `listed` event.
    ///
    /// Between pages `staging` is `items`. The first page this cycle places
    /// is reconciled Full, as the first cycle of a `Listing` and the one
    /// after a failure are: it locks the root, and clears what a
    /// stopped page left — a page placed but not committed, a directory not
    /// yet named. That is safe part-way through: this listing started with
    /// nothing of ours in the folder ([`Self::holds_nothing_yet`]), so
    /// whatever a Full reconcile finds that `staging` does not have, this
    /// listing placed, and a page still to come places again. Later pages are
    /// reconciled Changed, and hand over to Full as every Changed reconcile
    /// does.
    ///
    /// A resume link Graph refuses — the one a stopped listing left, asked
    /// for first in this cycle — lists the drive again from the start,
    /// whole, and reconciles it once in full ([`Fetched::Listed`]), as after
    /// an expired feed: what is placed is found by its id, and
    /// what is gone goes. A next-page link handed out in this cycle that
    /// Graph turns down fails the cycle like any trouble with Graph; the
    /// next cycle resumes from it.
    pub(super) async fn list_placing(&self, turn: &Turn, mut from: DeltaFrom, cancel: &CancellationToken) -> Result<Fetched, CycleError> {
        self.ctx.state.update(|s| s.listing = true);
        // However the listing ends — also when its future is dropped.
        let _said = OnDrop(Some(|| self.ctx.state.update(|s| s.listing = false)));
        // Read-write mode: the outbox's commit count when `staging` was last made from `items`.
        let (seq, counts) = self
            .on_store(turn, |s| {
                s.begin_staging(konedrive_tree::NewTree::Delta)?;
                Ok((s.outbox_seq()?, s.counts()?))
            })
            .await?;
        let mut staged_at = Some(seq);
        // The counts are walked once here and once at the end; in between,
        // each page adds what it listed and placed (issue #39).
        let (mut listed, mut shown) = (counts.listed, counts.placed);
        self.ctx.state.update(|s| {
            s.items_listed = counts.listed;
            s.items_placed = counts.placed;
            s.skipped_count = counts.skipped;
        });
        let mut placed = Reconciled::default();
        let mut full = true;
        let mut handed_over = false;
        // The stored resume link, until its page has come: the only link
        // Graph may refuse and send the listing back to the start. One it
        // handed out in this cycle and then turns down fails the cycle, and
        // the next cycle resumes from it — and restarts only if it is refused
        // then, as a resume link.
        let mut resuming = matches!(from, DeltaFrom::Link(_));
        loop {
            let page = match cancellable(cancel, self.ctx.drive.delta(&from)).await? {
                Ok(page) => page,
                Err(e) if resuming && refused(&e) => {
                    tracing::info!("OneDrive would not go on with the listing ({e}); listing the drive again from the start");
                    self.on_store(turn, |s| s.forget_listing_next()).await?;
                    self.ctx.state.update(|s| s.items_listed = 0);
                    return self.list_all_pages(turn, cancel).await;
                }
                Err(e) => return Err(drive_error(e)),
            };
            resuming = false;
            let changes: Vec<Change> = page.items.iter().map(classify).collect();
            listed += changes.iter().filter(|c| matches!(c, Change::Upsert(_))).count() as u64;
            let staged = changes.clone();
            // Read-write mode: the tree lock from this page's staging to its commit,
            // and `staging` made again from `items` under it when an outbox commit wrote
            // `items` since the last page — between pages the two are the same otherwise,
            // and the swap would revert that commit. Only then: a copy per page
            // would grow with the square of a large listing. An examination
            // writes nothing before the listing is complete (`NoBase`).
            let tree = match &self.ctx.writes {
                Some(_) => {
                    let tree = self.tree_lock(cancel).await?;
                    let seq = self.on_store(turn, |s| s.outbox_seq()).await?;
                    if staged_at.is_some_and(|at| at != seq) {
                        self.on_store(turn, |s| s.begin_staging(konedrive_tree::NewTree::Delta)).await?;
                    }
                    staged_at = Some(seq);
                    Some(tree)
                }
                None => None,
            };
            self.on_store(turn, move |s| s.stage(&staged)).await?;
            let scope = if full { Scope::Full } else { Scope::Changed(changes.iter().map(|c| c.id().to_owned()).collect()) };
            let (commit, next) = match page.next {
                DeltaNext::Page(next) => (Commit::Page { changes, next: next.clone() }, Some(next)),
                DeltaNext::Done(link) => (Commit::Swap { link, listing: true }, None),
            };
            let changed = !full;
            let done = match tree {
                None => self.reconcile(turn, scope, commit, cancel).await?,
                Some(tree) => {
                    let fetch_seq = self.on_store(turn, |s| s.outbox_seq()).await?;
                    // The last page ends a whole listing of the drive.
                    let whole_listing = next.is_none();
                    let brought = Vec::new();
                    let rw = RwCycle { tree, fetch_seq, consumed: Vec::new(), upload_differences: false, whole_listing, brought };
                    self.reconcile_rw(turn, scope, commit, rw, cancel).await?
                }
            };
            // A later page that had to hand over to Full found the folder
            // not matching `items` part-way — a name two pages give to two
            // items, say — and a later Changed page cannot see all it moved
            // aside.
            handed_over |= changed && done.full;
            // Still Full until a page is placed at all: none is before the
            // drive's root has come.
            full &= !done.full;
            shown += done.applied.counts.created;
            placed.add(done);
            self.ctx.state.update(|s| {
                s.items_listed = listed;
                s.items_placed = shown;
            });
            match next {
                Some(next) => from = DeltaFrom::Link(next),
                None => {
                    if handed_over {
                        // The listing's end is reconciled once more, whole,
                        // at the next cycle.
                        self.needs_full.store(true, Ordering::SeqCst);
                    }
                    return Ok(Fetched::Placed(placed));
                }
            }
        }
    }

    /// The changes since `link`; an expired feed (`410`) becomes a full
    /// listing.
    pub(super) async fn fetch_changes(&self, turn: &Turn, link: String, cancel: &CancellationToken) -> Result<Fetched, CycleError> {
        let mut from = DeltaFrom::Link(link);
        let mut changes = Vec::new();
        loop {
            match cancellable(cancel, self.ctx.drive.delta(&from)).await? {
                Ok(page) => {
                    changes.extend(page.items.iter().map(classify));
                    match page.next {
                        DeltaNext::Page(next) => from = DeltaFrom::Link(next),
                        DeltaNext::Done(link) => return Ok(Fetched::Changes { changes, link }),
                    }
                }
                Err(DriveError::ResyncRequired) => {
                    tracing::info!("the change feed has expired; listing the drive again");
                    return self.list_all(turn, cancel).await;
                }
                // Read-only, the two resyncs are one.
                Err(DriveError::ResyncUpload) => {
                    tracing::info!("the change feed has expired; listing the drive again, keeping what it no longer has");
                    return match self.list_all(turn, cancel).await? {
                        Fetched::Listed { link, .. } => Ok(Fetched::Listed { link, upload_differences: true }),
                        other => Ok(other),
                    };
                }
                Err(e) => return Err(drive_error(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests;
