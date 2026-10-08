
use tokio_util::sync::CancellationToken;

use konedrive_graph::drive::{DeltaFrom, DeltaNext, DriveError};
use crate::folder::classify::classify;
use konedrive_tree::Change;
use super::reconcile::{Commit, Reconciled, RwCycle, Waiting};
use super::stage::{Fetched, News, Since};
use super::{cancellable, drive_error, refused, CycleError, Listing, OnDrop, Turn};
use crate::remote::mode::Mode;

impl Listing {
    /// What OneDrive says since the stored link: the changes of a delta, a
    /// whole listing into `staging`, or — while the store holds no tree yet —
    /// a first listing shown page by page, begun or resumed.
    pub(super) async fn fetch(&self, turn: &Turn, mode: Mode<Since<'_>>, cancel: &CancellationToken) -> Result<Fetched, CycleError> {
        let (stored, resume_at) = self.on_store(turn, |s| Ok((s.delta_link()?, s.listing_next()?))).await?;
        match (stored, resume_at) {
            (Some(link), _) => Ok(Fetched::News(self.fetch_changes(turn, link, cancel).await?)),
            (None, Some(next)) if next.is_empty() => self.list_placing(turn, mode, DeltaFrom::Start, cancel).await,
            (None, Some(next)) => self.list_placing(turn, mode, DeltaFrom::Link(next), cancel).await,
            // The folder under the mount holds nothing of ours that the store does not
            // name: a first listing is shown page by page, each page's names as soon as
            // the page is committed.
            (None, None) => {
                // Under way from here on, also after a stop before its first page is
                // committed.
                self.on_store(turn, |s| s.begin_placing()).await?;
                self.list_placing(turn, mode, DeltaFrom::Start, cancel).await
            }
        }
    }

    /// A full listing into `staging`, page by page, publishing its progress.
    pub(super) async fn list_all(&self, turn: &Turn, cancel: &CancellationToken) -> Result<News, CycleError> {
        self.ctx.state.update(|s| {
            s.cycle.listing = true;
            s.cycle.items_listed = 0;
        });
        // However the listing ends — also when its future is dropped.
        let _said = OnDrop(Some(|| self.ctx.state.update(|s| s.cycle.listing = false)));
        self.list_all_pages(turn, cancel).await
    }

    /// A whole listing begins: `staging` is emptied for it.
    pub(crate) async fn begin_listing(&self, turn: &Turn) -> Result<(), CycleError> {
        self.on_store(turn, |s| s.begin_staging(konedrive_tree::NewTree::Whole)).await
    }

    /// One page of a whole listing, into `staging`.
    pub(crate) async fn stage_page(&self, turn: &Turn, changes: Vec<Change>) -> Result<(), CycleError> {
        self.on_store(turn, move |s| s.stage(&changes)).await
    }

    async fn list_all_pages(&self, turn: &Turn, cancel: &CancellationToken) -> Result<News, CycleError> {
        self.begin_listing(turn).await?;
        let mut from = DeltaFrom::Start;
        let mut listed = 0u64;
        loop {
            let page = cancellable(cancel, self.ctx.drive.delta(&from)).await?.map_err(drive_error)?;
            let changes: Vec<Change> = page.items.iter().map(classify).collect();
            listed += changes.iter().filter(|c| !matches!(c, Change::Root(_) | Change::Delete(_))).count() as u64;
            self.stage_page(turn, changes).await?;
            self.ctx.state.update(|s| s.cycle.items_listed = listed);
            match page.next {
                DeltaNext::Page(next) => from = DeltaFrom::Link(next),
                DeltaNext::Done(link) => return Ok(News::Listed { link }),
            }
        }
    }

    /// A first listing shown page by page, from the start or from
    /// where a stopped one got to (`from`, the link `items` was last
    /// committed with). Each page is staged, then committed into `items`
    /// together with the link to the page after it — under the folder's
    /// lock, which is let go while Graph is asked for the next page, so that
    /// a Forget waits for one page at most — and the mount shows its names
    /// from then on. An entry whose folder has not come yet waits in `items`
    /// (and so in `staging`) and is shown when its folder is. The
    /// last page swaps `staging` in with the delta link, which ends the
    /// listing, and says the one `listed` event.
    ///
    /// Between pages `staging` is `items`.
    ///
    /// A resume link Graph refuses — the one a stopped listing left, asked
    /// for first in this cycle — lists the drive again from the start,
    /// whole, and reconciles it once ([`News::Listed`]), as after
    /// an expired feed. A next-page link handed out in this cycle that
    /// Graph turns down fails the cycle like any trouble with Graph; the
    /// next cycle resumes from it.
    pub(super) async fn list_placing(&self, turn: &Turn, mode: Mode<Since<'_>>, mut from: DeltaFrom, cancel: &CancellationToken) -> Result<Fetched, CycleError> {
        self.ctx.state.update(|s| s.cycle.listing = true);
        // However the listing ends — also when its future is dropped.
        let _said = OnDrop(Some(|| self.ctx.state.update(|s| s.cycle.listing = false)));
        // Read-write mode: the outbox's commit count when `staging` was last made from `items`.
        let (seq, counts) = self
            .on_store(turn, |s| {
                s.begin_staging(konedrive_tree::NewTree::Delta)?;
                Ok((s.outbox_seq()?, s.counts()?))
            })
            .await?;
        let mut staged_at = Some(seq);
        // The counts are walked once here and once at the end; in between,
        // each page adds what it listed and placed.
        let (mut listed, mut shown) = (counts.listed, counts.placed);
        self.ctx.state.update(|s| {
            s.cycle.items_listed = counts.listed;
            s.cycle.items_placed = counts.placed;
            s.cycle.skipped_count = counts.skipped;
        });
        let mut placed = Reconciled::default();
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
                    self.ctx.state.update(|s| s.cycle.items_listed = 0);
                    return Ok(Fetched::News(self.list_all_pages(turn, cancel).await?));
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
            let tree = match mode {
                Mode::ReadWrite(Since { writes, .. }) => {
                    let tree = self.tree_lock(writes, cancel).await?;
                    let seq = self.on_store(turn, |s| s.outbox_seq()).await?;
                    if staged_at.is_some_and(|at| at != seq) {
                        self.on_store(turn, |s| s.begin_staging(konedrive_tree::NewTree::Delta)).await?;
                    }
                    staged_at = Some(seq);
                    Some((writes, tree))
                }
                Mode::ReadOnly => None,
            };
            self.on_store(turn, move |s| s.stage(&staged)).await?;
            let (commit, next) = match page.next {
                DeltaNext::Page(next) => (Commit::Page { changes, next: next.clone() }, Some(next)),
                DeltaNext::Done(link) => (Commit::Swap { link, listing: true }, None),
            };
            let mode = match tree {
                None => Mode::ReadOnly,
                Some((writes, tree)) => {
                    let fetch_seq = self.on_store(turn, |s| s.outbox_seq()).await?;
                    // The last page ends a whole listing of the drive.
                    let waiting = Waiting { fetch_seq, consumed: Vec::new() };
                    Mode::ReadWrite(RwCycle { writes, tree, waiting })
                }
            };
            let done = self.reconcile(turn, mode, commit, cancel).await?;
            shown += done.applied.counts.created;
            placed.add(done);
            self.ctx.state.update(|s| {
                s.cycle.items_listed = listed;
                s.cycle.items_placed = shown;
            });
            match next {
                Some(next) => from = DeltaFrom::Link(next),
                None => return Ok(Fetched::Placed(placed)),
            }
        }
    }

    /// The changes since `link`; an expired feed (`410`) becomes a full
    /// listing.
    pub(crate) async fn fetch_changes(&self, turn: &Turn, link: String, cancel: &CancellationToken) -> Result<News, CycleError> {
        let mut from = DeltaFrom::Link(link);
        let mut changes = Vec::new();
        loop {
            match cancellable(cancel, self.ctx.drive.delta(&from)).await? {
                Ok(page) => {
                    changes.extend(page.items.iter().map(classify));
                    match page.next {
                        DeltaNext::Page(next) => from = DeltaFrom::Link(next),
                        DeltaNext::Done(link) => return Ok(News::Changes { changes, link }),
                    }
                }
                Err(DriveError::ResyncRequired) => {
                    tracing::info!("the change feed has expired; listing the drive again");
                    return self.list_all(turn, cancel).await;
                }
                // The two resyncs are one: what a change of the user's holds is kept
                // whichever it is.
                Err(DriveError::ResyncUpload) => {
                    tracing::info!("the change feed has expired; listing the drive again");
                    return self.list_all(turn, cancel).await;
                }
                Err(e) => return Err(drive_error(e)),
            }
        }
    }
}
