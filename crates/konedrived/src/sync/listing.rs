//! One folder's sync with OneDrive: the delta
//! feed read into the tree store's `staging`, the folder made to match it, and
//! only then `staging` swapped in with the new delta link. A crash before the
//! swap leaves the old link, and the next cycle asks for the same changes.
//!
//! The first cycle of every `Listing`, and every cycle after one that failed
//! (or whose future was dropped), reconciles the whole folder — as
//! does the cycle after one that left a file for later, or after a replacement
//! that ended with nothing to do: a Changed scope never looks at those files
//! again. A replacement that failed is retried as it is, after every cycle.
//!
//! Cycles of one `Listing` never overlap. A cycle asks Graph without the
//! lifecycle lock and takes it only to change the folder and swap the link in,
//! so a helper's reconnect is never kept waiting by a listing. A stop
//! (`Poller::stop`) never waits for Graph or for whoever holds that lock; it
//! waits only for a reconcile already changing the folder, which checks for the
//! stop between steps.
//!
//! A folder's first listing, into a folder that holds nothing of ours yet, is
//! placed page by page instead: each page is reconciled and
//! committed into `items` as it comes, with the link to the next page, so
//! that the folder fills as the drive is listed and a listing stopped
//! part-way resumes where it stopped. See [`Listing::list_placing`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::{Notify, OwnedMutexGuard, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::activity::{self, Kind, Report, Tracked};
use super::disk::{rescue_base, rescue_stamp, Disk};
use super::helper::HelperLink;
use super::materialize::{replace, Applied, ApplyError, Materializer, ReplaceOutcome, Replacement, Scope};
use super::root::SyncRoot;
use super::source::ContentSource;
use super::{InodeLocks, SyncStateHandle, SyncTrouble};
use crate::drive::{DeltaFrom, DeltaNext, DriveClient, DriveError};
use crate::tree::{classify, Change, ConflictRow, Store, Table, TreeError, TreeStore};

/// A delta with more changes than this is reconciled in full.
pub const FULL_THRESHOLD: usize = 5000;
/// Replacements of changed files downloading at once.
const REPLACEMENT_SLOTS: usize = 2;

pub type LinkCell = Arc<std::sync::Mutex<Option<HelperLink>>>;

/// The drive a folder was listed from, as `config.toml` keeps it beside the
/// root: the same-account check then
/// survives a tree store rebuilt empty, whose `meta` has forgotten it.
#[derive(Clone)]
pub struct DriveRecord {
    pub config_file: PathBuf,
    /// The root the record belongs to: written only while `config.toml`
    /// still names it.
    pub root_id: String,
    /// What `config.toml` recorded when the sync started; `None` when
    /// nothing was, and the first cycle writes it.
    pub recorded: Option<String>,
}

pub struct ListingContext {
    pub root: SyncRoot,
    pub intercepted: bool,
    pub store: Store,
    pub drive: DriveClient,
    /// `None` where nothing keeps a record (tests): the store's `meta` alone
    /// says which drive the folder shows.
    pub drive_record: Option<DriveRecord>,
    pub source: Arc<dyn ContentSource>,
    /// The helper link as `SyncService` holds it; read at every reconcile.
    pub link: LinkCell,
    pub locks: InodeLocks,
    pub state: SyncStateHandle,
    /// `SyncService`'s: a reconcile holds it for reading, so a Forget waits
    /// for one that is changing the folder.
    pub lifecycle: Arc<tokio::sync::RwLock<()>>,
    pub rescue_dir: PathBuf,
    pub full_threshold: usize,
    /// Nudged at the end of every successful cycle, so the thumbnail filler
    /// runs right after there is something new to make thumbnails
    /// of, rather than waiting out its own idle timer.
    pub after_cycle: Option<Arc<Notify>>,
    /// Where the cycle's activity, conflicts and downloads are reported, and
    /// the folder's space measured again: `SyncService`'s.
    pub report: Report,
}

#[derive(Debug, thiserror::Error)]
pub enum CycleError {
    #[error("signed out: sign in again to keep this folder in step with OneDrive")]
    SignedOut,
    #[error(
        "this folder was listed from another OneDrive account (drive {0}); forget it and register \
         a folder for the account signed in now"
    )]
    OtherAccount(String),
    #[error("cannot reach OneDrive ({0}); trying again")]
    Offline(String),
    #[error("the helper is not connected; the folder is brought up to date when it is back")]
    NoHelper,
    #[error("the tree store: {0}")]
    Store(String),
    #[error("the folder could not be brought up to date: {0}")]
    Apply(String),
    #[error("cancelled")]
    Cancelled,
}

impl CycleError {
    /// Trouble that stops the folder until someone acts — the
    /// helper's absence among it (HS2), though that one is published as the
    /// folder waiting for the helper, not as sync trouble
    /// ([`Listing::publish_outcome`]).
    pub fn blocking(&self) -> bool {
        matches!(self, CycleError::SignedOut | CycleError::OtherAccount(_) | CycleError::Store(_) | CycleError::NoHelper)
    }
}

impl From<TreeError> for CycleError {
    fn from(e: TreeError) -> Self {
        CycleError::Store(e.to_string())
    }
}

/// `work`, unless `cancel` fires first; it is dropped then. Only for what is
/// safe to drop half-way: a request to Graph (which can wait out a long
/// `Retry-After`; nothing is written until it answers), and the wait for a
/// lock (whose holder may be the one stopping this poller).
async fn cancellable<T>(cancel: &CancellationToken, work: impl std::future::Future<Output = T>) -> Result<T, CycleError> {
    cancel.run_until_cancelled(work).await.ok_or(CycleError::Cancelled)
}

fn drive_error(e: DriveError) -> CycleError {
    match e {
        DriveError::SignedOut => CycleError::SignedOut,
        other => CycleError::Offline(other.to_string()),
    }
}

/// What making the folder match the tree ran into.
fn applying(e: ApplyError) -> CycleError {
    match e {
        ApplyError::Cancelled => CycleError::Cancelled,
        other => CycleError::Apply(other.to_string()),
    }
}

/// What one cycle did.
#[derive(Debug, Default)]
pub struct CycleReport {
    /// A Full reconcile ran.
    pub full: bool,
    /// Entries in the delta; 0 for a full listing.
    pub changes: usize,
    pub applied: Applied,
}

/// A cycle's turn: while any clone of it lives — in the cycle, or in a
/// blocking task the cycle started — no other cycle of the same `Listing`
/// runs, even when this cycle's future has been dropped.
type Turn = Arc<OwnedMutexGuard<()>>;

pub struct Listing {
    ctx: ListingContext,
    needs_full: AtomicBool,
    /// The drive has been written into `config.toml` (A-M5), or is being:
    /// once per `Listing`.
    drive_recorded: AtomicBool,
    /// The drive to write there, by the next reconcile.
    pending_drive: std::sync::Mutex<Option<(DriveRecord, String)>>,
    /// Whose turn it is (see [`Turn`]).
    turns: Arc<tokio::sync::Mutex<()>>,
    /// The replacements under way, by item id.
    replacing: std::sync::Mutex<HashMap<String, InFlight>>,
    /// Replacements that failed ("the status says why"), tried
    /// again after every cycle until they succeed or are no longer needed.
    failed_replacements: std::sync::Mutex<HashMap<String, (Replacement, String)>>,
    replacement_slots: Semaphore,
    replacements: std::sync::Mutex<JoinSet<()>>,
    cancel_replacements: CancellationToken,
}

/// A replacement under way: the version it fetches, and a newer version of
/// the same file that arrived meanwhile, fetched when it ends.
struct InFlight {
    ctag: String,
    next: Option<Replacement>,
}

/// Runs its closure when dropped, unless disarmed first.
struct OnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> OnDrop<F> {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// What a reconcile did.
#[derive(Default)]
struct Reconciled {
    applied: Applied,
    /// A Full reconcile ran — asked for, or handed over to.
    full: bool,
}

impl Reconciled {
    /// What one page of a first listing did, added to what the pages before
    /// it did. `changes` stays empty: a first listing is one `listed` event,
    /// as a Full reconcile is.
    fn add(&mut self, page: Reconciled) {
        // Every field named: one added to `Applied` does not compile here
        // until it is handled.
        let Applied { created, moved, deleted, updated, deferred, rescued, replacements, changes: _ } = page.applied;
        let all = &mut self.applied;
        all.created += created;
        all.moved += moved;
        all.deleted += deleted;
        all.updated += updated;
        all.deferred += deferred;
        all.rescued.extend(rescued);
        all.replacements.extend(replacements);
        self.full |= page.full;
    }
}

/// What the feed said since the stored link.
enum Fetched {
    /// A full listing is in `staging`: the first into a folder that shows
    /// the drive already, one after `410`, or one after a first listing's
    /// resume link was refused.
    Listed { link: String },
    Changes { changes: Vec<Change>, link: String },
    /// A first listing, placed page by page and committed with its link
    ///.
    Placed(Reconciled),
}

/// What a reconcile commits once the folder matches `staging`.
enum Commit {
    /// `staging` swapped in with the link to ask from next time.
    /// `listing`: the last page of a first listing placed page by page,
    /// which the one `listed` event stands for, however it was reconciled.
    Swap { link: String, listing: bool },
    /// One page of a first listing: its entries into `items`, with
    /// `next`, the link to the page after it.
    Page { changes: Vec<Change>, next: String },
}

/// What the activity log says of a reconcile, beside its conflicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Said {
    /// One `listed` event for the whole folder: a Full reconcile, or the end
    /// of a first listing.
    Listed,
    /// What a Changed reconcile did, item by item.
    EachChange,
    /// Nothing: a page of a first listing, which the `listed` event at its
    /// end stands for.
    Nothing,
}

/// Whether Graph refused a link it handed out — expired (`410`), gone, or a
/// token it no longer takes — rather than could not be reached or refused
/// the account.
fn refused(e: &DriveError) -> bool {
    matches!(e, DriveError::ResyncRequired | DriveError::NotFound | DriveError::Failed(_))
}

impl Listing {
    pub fn new(ctx: ListingContext) -> Arc<Self> {
        Arc::new(Self {
            ctx,
            needs_full: AtomicBool::new(true),
            drive_recorded: AtomicBool::new(false),
            pending_drive: std::sync::Mutex::new(None),
            turns: Arc::new(tokio::sync::Mutex::new(())),
            replacing: std::sync::Mutex::new(HashMap::new()),
            failed_replacements: std::sync::Mutex::new(HashMap::new()),
            replacement_slots: Semaphore::new(REPLACEMENT_SLOTS),
            replacements: std::sync::Mutex::new(JoinSet::new()),
            cancel_replacements: CancellationToken::new(),
        })
    }

    /// One cycle, and what came of it published.
    ///
    /// Cycles of one `Listing` run one at a time (a `refresh` and the
    /// poller's own, say). One that fails, or whose future is dropped
    /// part-way, leaves the next one a Full reconcile.
    pub async fn cycle(self: &Arc<Self>, cancel: &CancellationToken) -> Result<CycleReport, CycleError> {
        let result = self.take_turn(cancel).await;
        self.publish_outcome(&result);
        if result.is_ok() {
            if let Some(kick) = &self.ctx.after_cycle {
                kick.notify_one();
            }
        }
        // `LocalBytes` after each cycle: even one that failed
        // part-way may have changed the folder.
        if !matches!(result, Err(CycleError::Cancelled)) {
            self.ctx.report.space.kick();
        }
        result
    }

    async fn take_turn(self: &Arc<Self>, cancel: &CancellationToken) -> Result<CycleReport, CycleError> {
        let turn: Turn = Arc::new(cancellable(cancel, Arc::clone(&self.turns).lock_owned()).await?);
        // Taken, not read: a replacement that ends while this cycle runs asks
        // for a Full reconcile, and that request must outlive this cycle.
        let full_requested = self.needs_full.swap(false, Ordering::SeqCst);
        // Unless this cycle succeeds, the next one is Full — also
        // when its future is dropped part-way.
        let mut unless_done = OnDrop(Some(|| self.needs_full.store(true, Ordering::SeqCst)));
        let result = self.sync_once(&turn, full_requested, cancel).await;
        if result.is_ok() {
            unless_done.disarm();
        }
        result
    }

    async fn sync_once(self: &Arc<Self>, turn: &Turn, full_requested: bool, cancel: &CancellationToken) -> Result<CycleReport, CycleError> {
        // HS2: a folder that shows OneDrive is kept in step only with
        // interception and a connected helper — nothing is placed or updated
        // otherwise, and Graph is not asked for what could not be placed.
        // Asked again under the lifecycle lock, where the answer counts.
        if !self.ctx.intercepted || self.ctx.link.lock().unwrap().is_none() {
            return Err(CycleError::NoHelper);
        }
        self.check_account(turn, cancel).await?;
        let (stored, resume_at) = self.on_store(turn, |s| Ok((s.delta_link()?, s.listing_next()?))).await?;
        let fetched = match (stored, resume_at) {
            (Some(link), _) => self.fetch_changes(turn, link, cancel).await?,
            (None, Some(next)) if next.is_empty() => self.list_placing(turn, DeltaFrom::Start, cancel).await?,
            (None, Some(next)) => self.list_placing(turn, DeltaFrom::Link(next), cancel).await?,
            (None, None) if self.holds_nothing_yet(turn).await? => {
                // Under way from here on, so that whatever of ours the
                // folder holds from now on is this listing's, also after a
                // stop before its first page is committed.
                self.on_store(turn, |s| s.begin_placing()).await?;
                self.list_placing(turn, DeltaFrom::Start, cancel).await?
            }
            (None, None) => self.list_all(turn, cancel).await?,
        };
        let (reconciled, changes) = match fetched {
            Fetched::Placed(placed) => (placed, 0),
            Fetched::Listed { link } => (self.reconcile(turn, Scope::Full, Commit::Swap { link, listing: false }, cancel).await?, 0),
            Fetched::Changes { changes, link } => {
                let count = changes.len();
                if count > 0 || full_requested {
                    self.on_store(turn, move |s| {
                        s.begin_staging(true)?;
                        s.stage(&changes)
                    })
                    .await?;
                }
                let scope = if full_requested || count > self.ctx.full_threshold {
                    Some(Scope::Full)
                } else if count == 0 {
                    None
                } else {
                    // What `staging` now differs from `items` by: the delta
                    // just staged, read back so the closure above could own it.
                    Some(Scope::Changed(self.on_store(turn, |s| s.changed_ids()).await?))
                };
                let reconciled = match scope {
                    None => {
                        self.on_store(turn, move |s| s.set_meta("delta_link", Some(&link))).await?;
                        Reconciled::default()
                    }
                    Some(scope) => self.reconcile(turn, scope, Commit::Swap { link, listing: false }, cancel).await?,
                };
                (reconciled, count)
            }
        };
        if reconciled.applied.deferred > 0 {
            // Files being filled or freed up right now: a Changed scope would
            // never look at them again.
            self.needs_full.store(true, Ordering::SeqCst);
        }
        let Reconciled { applied, full } = reconciled;
        self.publish_counts(turn).await?;
        // `LastChecked`: this cycle succeeded. Kept in the store,
        // so a restart still knows when the folder was last in step.
        let now = activity::unix_now();
        self.on_store(turn, move |s| s.set_meta("last_checked", Some(&now.to_string()))).await?;
        self.ctx.state.update(|s| s.last_checked = now);
        // A conflict whose rescued file is gone drops off by itself (spec
        // §16.1), whether or not anyone asks for the list. Not through
        // `on_store`: the activity log takes the store's lock itself.
        let (report, held) = (self.ctx.report.clone(), Arc::clone(turn));
        if let Err(e) = tokio::task::spawn_blocking(move || {
            let _turn = held;
            report.activity.prune();
        })
        .await
        {
            tracing::warn!("the task looking over the conflicts failed: {e}");
        }
        self.spawn_replacements(applied.replacements.clone());
        Ok(CycleReport { full, changes, applied })
    }

    /// A tree store call on a blocking thread, holding this cycle's turn
    /// until it is done — even when the cycle's future is dropped meanwhile.
    async fn on_store<T: Send + 'static>(
        &self,
        turn: &Turn,
        f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static,
    ) -> Result<T, CycleError> {
        let (store, turn) = (self.ctx.store.clone(), Arc::clone(turn));
        tokio::task::spawn_blocking(move || {
            let _turn = turn;
            store.with(f)
        })
        .await
        .map_err(|e| CycleError::Store(format!("the store task failed: {e}")))?
        .map_err(CycleError::from)
    }

    /// At every cycle: a sign-out and a sign-in as someone else
    /// can come between any two of them. The drive is the one the store's
    /// `meta` records, or — for a store rebuilt empty — the one `config.toml`
    /// keeps beside the root (A-M5); once known, it is recorded in both.
    async fn check_account(&self, turn: &Turn, cancel: &CancellationToken) -> Result<(), CycleError> {
        let id = cancellable(cancel, self.ctx.drive.drive_id()).await?.map_err(drive_error)?;
        let stored = self.on_store(turn, |s| s.meta("drive_id")).await?;
        let kept = self.ctx.drive_record.as_ref().and_then(|r| r.recorded.clone());
        if let Some(recorded) = stored.clone().or(kept.clone()).filter(|recorded| *recorded != id) {
            return Err(CycleError::OtherAccount(recorded));
        }
        if stored.is_none() {
            let recorded = id.clone();
            self.on_store(turn, move |s| s.set_meta("drive_id", Some(&recorded))).await?;
        }
        if let Some(record) = self.ctx.drive_record.as_ref().filter(|_| kept.is_none()) {
            if !self.drive_recorded.swap(true, Ordering::SeqCst) {
                // Written by the next reconcile, which holds the lifecycle
                // lock anyway: the Graph phase never waits for it (B3).
                *self.pending_drive.lock().unwrap() = Some((record.clone(), id));
            }
        }
        Ok(())
    }

    /// A full listing into `staging`, page by page, publishing its progress.
    async fn list_all(&self, turn: &Turn, cancel: &CancellationToken) -> Result<Fetched, CycleError> {
        self.ctx.state.update(|s| {
            s.listing = true;
            s.items_listed = 0;
        });
        // However the listing ends — also when its future is dropped.
        let _said = OnDrop(Some(|| self.ctx.state.update(|s| s.listing = false)));
        self.list_all_pages(turn, cancel).await
    }

    async fn list_all_pages(&self, turn: &Turn, cancel: &CancellationToken) -> Result<Fetched, CycleError> {
        self.on_store(turn, |s| s.begin_staging(false)).await?;
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
                DeltaNext::Done(link) => return Ok(Fetched::Listed { link }),
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
    async fn holds_nothing_yet(&self, turn: &Turn) -> Result<bool, CycleError> {
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
    async fn list_placing(&self, turn: &Turn, mut from: DeltaFrom, cancel: &CancellationToken) -> Result<Fetched, CycleError> {
        self.ctx.state.update(|s| s.listing = true);
        // However the listing ends — also when its future is dropped.
        let _said = OnDrop(Some(|| self.ctx.state.update(|s| s.listing = false)));
        self.on_store(turn, |s| s.begin_staging(true)).await?;
        self.publish_counts(turn).await?;
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
                    self.on_store(turn, |s| s.set_meta(crate::tree::LISTING_NEXT, None)).await?;
                    self.ctx.state.update(|s| s.items_listed = 0);
                    return self.list_all_pages(turn, cancel).await;
                }
                Err(e) => return Err(drive_error(e)),
            };
            resuming = false;
            let changes: Vec<Change> = page.items.iter().map(classify).collect();
            let staged = changes.clone();
            self.on_store(turn, move |s| s.stage(&staged)).await?;
            let scope = if full { Scope::Full } else { Scope::Changed(changes.iter().map(|c| c.id().to_owned()).collect()) };
            let (commit, next) = match page.next {
                DeltaNext::Page(next) => (Commit::Page { changes, next: next.clone() }, Some(next)),
                DeltaNext::Done(link) => (Commit::Swap { link, listing: true }, None),
            };
            let changed = !full;
            let done = self.reconcile(turn, scope, commit, cancel).await?;
            // A later page that had to hand over to Full found the folder
            // not matching `items` part-way — a name two pages give to two
            // items, say — and a later Changed page cannot see all it moved
            // aside.
            handed_over |= changed && done.full;
            // Still Full until a page is placed at all: none is before the
            // drive's root has come.
            full &= !done.full;
            placed.add(done);
            self.publish_counts(turn).await?;
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
    async fn fetch_changes(&self, turn: &Turn, link: String, cancel: &CancellationToken) -> Result<Fetched, CycleError> {
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
                Err(e) => return Err(drive_error(e)),
            }
        }
    }

    /// Makes the folder match `staging` and commits it: swaps
    /// `staging` in with its link, or puts a first listing's page into
    /// `items` with the link to the next one. Under the lifecycle
    /// lock, on a blocking thread (part 1's). A Changed scope that
    /// finds the folder not matching the stored tree hands over to a Full
    /// reconcile in the same run. Everything before this wrote only
    /// `staging` and `meta`, and needed no lock.
    ///
    /// The lock and the turn go into the blocking task: however this future
    /// ends, they are held until the folder is no longer being changed. The
    /// materializer sees `cancel` itself between steps; the commit follows
    /// the change to the folder in the same task, so a page placed is a page
    /// committed unless the daemon dies in between.
    async fn reconcile(&self, turn: &Turn, scope: Scope, commit: Commit, cancel: &CancellationToken) -> Result<Reconciled, CycleError> {
        let lifecycle = cancellable(cancel, Arc::clone(&self.ctx.lifecycle).read_owned()).await?;
        // The link as it is now. A helper's reconnect sets it before `resume`
        // takes the lock to re-register the root, so this may be a new link
        // whose helper has no marks yet: at worst a `MarkDir` fails, this
        // cycle fails (the next is Full), and `resume` re-marks the tree.
        let link = self.ctx.link.lock().unwrap().clone().filter(|_| self.ctx.intercepted);
        if link.is_none() {
            return Err(CycleError::NoHelper);
        }
        let held = (Arc::clone(turn), lifecycle);
        let (root, preferred, store) = (self.ctx.root.clone(), self.ctx.rescue_dir.clone(), self.ctx.store.clone());
        let (locks, cancel) = (self.ctx.locks.clone(), cancel.clone());
        let report = self.ctx.report.clone();
        let runtime = tokio::runtime::Handle::current();
        let drive = self.pending_drive.lock().unwrap().take();
        tokio::task::spawn_blocking(move || {
            let _held = held;
            if let Some((record, id)) = drive {
                record_drive(&record, &id);
            }
            let Some(root_item_id) = store.with(|s| s.root_item_id()).map_err(|e| applying(e.into()))? else {
                return match commit {
                    // Nothing on a page can be placed before the drive's
                    // root has come: it waits in `items` like any entry
                    // whose folder has not come yet.
                    Commit::Page { changes, next } => {
                        store.with(|s| s.commit_page(&changes, &next))?;
                        Ok(Reconciled::default())
                    }
                    Commit::Swap { .. } => Err(CycleError::Apply("the drive's listing has no root".into())),
                };
            };
            let materializer = Materializer {
                disk: Disk::open(&root, true).map_err(|e| applying(e.into()))?,
                store: store.clone(),
                link,
                runtime,
                locks,
                root_item_id,
                // One directory for the whole cycle, on the folder's own
                // filesystem: a rescue is one rename, never a copy.
                rescue_into: rescue_base(&root.path, &preferred).join(rescue_stamp(SystemTime::now())),
                cancel,
            };
            let changed = matches!(scope, Scope::Changed(_));
            // What a Changed pass rescued before it handed over is rescued
            // all the same: the Full pass finds nothing left to rescue there,
            // so these are the conflicts.
            let mut first_pass = Vec::new();
            let (mut applied, full) = match materializer.apply_keeping(scope, &mut first_pass) {
                Err(ApplyError::NeedFull(why) | ApplyError::Io(why)) if changed => {
                    tracing::info!("{why}; reconciling the whole folder");
                    (materializer.apply(Scope::Full).map_err(applying)?, true)
                }
                other => (other.map_err(applying)?, !changed),
            };
            if !first_pass.is_empty() {
                first_pass.append(&mut applied.rescued);
                applied.rescued = first_pass;
            }
            // Where each rescued file went is a conflict: a row
            // in `Conflicts()`, `ConflictCount` and a `conflict` event, which
            // `record` below writes. It is not a problem, so `LastError` no
            // longer says it. A page's are written with the page, not at the
            // end of the listing: a listing that never ends must still say
            // where the files went.
            let said = match commit {
                Commit::Swap { link, listing } => {
                    store.with(|s| s.commit_staging(&link))?;
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
            record(&report, &store, &root.path, &applied, said);
            Ok(Reconciled { applied, full })
        })
        .await
        .map_err(|e| CycleError::Apply(format!("the reconcile task failed: {e}")))?
    }

    async fn publish_counts(&self, turn: &Turn) -> Result<(), CycleError> {
        let counts = self.on_store(turn, |s| s.counts(Table::Items)).await?;
        self.ctx.state.update(|s| {
            s.items_listed = counts.listed;
            s.items_placed = counts.placed;
            s.skipped_count = counts.skipped;
        });
        Ok(())
    }

    /// Starts a replacement for each file not being replaced already (spec
    /// §7.3), and retries those that failed. A newer version of a file whose
    /// replacement is under way is fetched when that one ends; a failed one
    /// is retried only when no fresher replacement of the file stands for it.
    fn spawn_replacements(self: &Arc<Self>, fresh: Vec<Replacement>) {
        let retries: Vec<Replacement> = self
            .failed_replacements
            .lock()
            .unwrap()
            .values()
            .filter(|(failed, _)| !fresh.iter().any(|r| r.id == failed.id))
            .map(|(failed, _)| failed.clone())
            .collect();
        let mut start = Vec::new();
        {
            let mut replacing = self.replacing.lock().unwrap();
            for replacement in fresh {
                match replacing.get_mut(&replacement.id) {
                    None => {
                        replacing.insert(replacement.id.clone(), InFlight { ctag: replacement.ctag.clone(), next: None });
                        start.push(replacement);
                    }
                    // This very version is on its way; nothing newer after it.
                    Some(running) if running.ctag == replacement.ctag => running.next = None,
                    Some(running) => running.next = Some(replacement),
                }
            }
            for replacement in retries {
                if !replacing.contains_key(&replacement.id) {
                    replacing.insert(replacement.id.clone(), InFlight { ctag: replacement.ctag.clone(), next: None });
                    start.push(replacement);
                }
            }
        }
        for replacement in start {
            self.start_replacement(replacement);
        }
    }

    fn start_replacement(self: &Arc<Self>, replacement: Replacement) {
        let this = Arc::clone(self);
        let mut tasks = self.replacements.lock().unwrap();
        // Finished ones are kept only for `join_replacements`.
        while tasks.try_join_next().is_some() {}
        tasks.spawn(async move {
            // Cut short by `Poller::stop`: no outcome, and nothing after it.
            let outcome = this.cancel_replacements.run_until_cancelled(this.replace_one(&replacement)).await;
            let stopped = outcome.is_none();
            if let Some((outcome, event)) = outcome {
                // A failure retried after every cycle is said once, not a
                // minute (I1).
                let news = this.record_replacement(&replacement, outcome);
                if let Some(event) = event {
                    if news {
                        this.ctx.report.activity.record(vec![event]).await;
                    }
                    this.ctx.report.space.kick();
                }
            }
            let next = {
                let mut replacing = this.replacing.lock().unwrap();
                let next = replacing.remove(&replacement.id).and_then(|running| running.next).filter(|_| !stopped);
                if let Some(next) = &next {
                    replacing.insert(next.id.clone(), InFlight { ctag: next.ctag.clone(), next: None });
                }
                next
            };
            if let Some(next) = next {
                this.start_replacement(next);
            }
        });
    }

    /// Replaces one file, shown in `Transfers` while it downloads (spec
    /// §16.2), and what the activity log would say of it: `updated` or
    /// `update-failed`. Whether it says it is [`record_replacement`]'s to
    /// decide.
    async fn replace_one(&self, replacement: &Replacement) -> (ReplaceOutcome, Option<activity::Event>) {
        let shown = self.ctx.root.path.join(&replacement.rel).display().to_string();
        let tracked = Tracked::new(Arc::clone(&self.ctx.source), self.ctx.report.transfers.clone(), shown.clone());
        let outcome = self.replace_through(&tracked, replacement).await;
        let size = tracked.fetched().unwrap_or(replacement.size);
        drop(tracked);
        let event = match &outcome {
            ReplaceOutcome::Replaced => Some(activity::event(Kind::Updated, shown, activity::human_size(size))),
            ReplaceOutcome::Failed(why) => Some(activity::event(Kind::UpdateFailed, shown, why.clone())),
            ReplaceOutcome::NoSpace(_) => Some(activity::event(Kind::UpdateFailed, shown, activity::NO_DISK_SPACE)),
            ReplaceOutcome::Current => None,
        };
        (outcome, event)
    }

    async fn replace_through(&self, source: &Tracked, replacement: &Replacement) -> ReplaceOutcome {
        let _slot = self.replacement_slots.acquire().await;
        // Opening reads the root's attribute to prove it is still this root:
        // on a blocking thread, like every open (part 1's).
        let root = self.ctx.root.clone();
        let disk = match tokio::task::spawn_blocking(move || Disk::open(&root, true)).await {
            Ok(Ok(disk)) => disk,
            Ok(Err(e)) => return ReplaceOutcome::Failed(e.to_string()),
            Err(e) => return ReplaceOutcome::Failed(format!("the replacement task failed: {e}")),
        };
        replace(&disk, &self.ctx.locks, source, replacement).await
    }

    /// What a replacement came to. One that ended with nothing to do asks for
    /// a Full reconcile: a Changed scope never looks at that file again, and
    /// only a Full one works out from the disk and the tree what it needs now
    /// — the replacement again, an update of its placeholder, a rescue, or
    /// nothing. One that failed is kept, said, and retried as it is after
    /// every cycle (`spawn_replacements`); a Full reconcile would add nothing
    /// but a scan of the whole folder.
    ///
    /// Whether it is news for the activity log (I1): a
    /// replacement that went through is; a failure is only when it is new
    /// for this file — a first failure, one for a newer version, or one for
    /// another reason than the last. The same failure on every retry is said
    /// once, and the status note keeps saying it.
    fn record_replacement(&self, replacement: &Replacement, outcome: ReplaceOutcome) -> bool {
        let mut failed = self.failed_replacements.lock().unwrap();
        let news = match &outcome {
            ReplaceOutcome::Replaced => true,
            ReplaceOutcome::Current => false,
            ReplaceOutcome::Failed(why) | ReplaceOutcome::NoSpace(why) => !matches!(
                failed.get(&replacement.id),
                Some((before, said)) if before.ctag == replacement.ctag && said == why
            ),
        };
        match outcome {
            ReplaceOutcome::Replaced => {
                failed.remove(&replacement.id);
            }
            ReplaceOutcome::Current => {
                failed.remove(&replacement.id);
                self.needs_full.store(true, Ordering::SeqCst);
            }
            ReplaceOutcome::Failed(why) | ReplaceOutcome::NoSpace(why) => {
                if news {
                    tracing::warn!("{}: {why}", replacement.rel.display());
                } else {
                    tracing::debug!("{}: still {why}", replacement.rel.display());
                }
                failed.insert(replacement.id.clone(), (replacement.clone(), why));
            }
        }
        let note = match failed.values().next() {
            None => String::new(),
            Some((_, why)) => format!("{} file(s) changed in OneDrive could not be updated here yet: {why}", failed.len()),
        };
        self.ctx.state.update(|s| s.replacement_note = note);
        news
    }

    /// Waits for the replacements under way, and for the newer versions they
    /// hand over to (tests; and `Poller::stop` after cancelling them).
    pub async fn join_replacements(&self) {
        loop {
            let mut set = std::mem::take(&mut *self.replacements.lock().unwrap());
            if set.is_empty() {
                return;
            }
            while set.join_next().await.is_some() {}
        }
    }

    fn publish_outcome(&self, result: &Result<CycleReport, CycleError>) {
        self.ctx.state.update(|s| match result {
            Ok(_) => {
                s.sync_trouble = None;
                s.waits_for_helper = false;
            }
            Err(CycleError::Cancelled) => {}
            // The folder waits for the helper (HS2, HS3): `LastError` says so
            // in the helper's own words (`HelperState`), and `RootState`
            // reads `error`. `SyncService` publishes the same the moment the
            // link drops; this only makes sure of it.
            Err(CycleError::NoHelper) => s.waits_for_helper = true,
            Err(e) => s.sync_trouble = Some(SyncTrouble { text: e.to_string(), blocking: e.blocking() }),
        });
    }
}

/// Writes the drive into `config.toml` beside the root (A-M5), if it still
/// names this root and records none. Called by a reconcile, which holds the
/// lifecycle lock for reading: no registration or Forget, which write the
/// same file under it held for writing, comes in between. A failure is
/// logged; the store's `meta` still has the drive.
fn record_drive(record: &DriveRecord, id: &str) {
    let written = crate::config::Config::load(&record.config_file).and_then(|mut config| {
        if config.sync_root_id == record.root_id && config.sync_root_drive_id.is_empty() {
            config.sync_root_drive_id = id.to_owned();
            config.save(&record.config_file)?;
        }
        Ok(())
    });
    if let Err(e) = written {
        tracing::warn!("cannot record the folder's drive in config.toml: {e}");
    }
}

/// What a reconcile that went through records, on its blocking
/// thread, right after the tree it made the folder match is committed: each
/// rescue as a conflict — the row first, so that whoever hears of it can
/// already find it — and the activity `said`: one `listed` event for a Full
/// reconcile or the end of a first listing, what a Changed one did item by
/// item, at most [`activity::PER_KIND`] of each kind plus one "and N more",
/// or nothing but the conflicts for a page of a first listing.
fn record(report: &Report, store: &Store, root: &std::path::Path, applied: &Applied, said: Said) {
    if said == Said::Nothing && applied.rescued.is_empty() {
        return;
    }
    let shown = |rel: &std::path::Path| root.join(rel).display().to_string();
    let folder = root.display().to_string();
    let mut events = match said {
        Said::Listed => {
            let listed = match store.with(|s| s.counts(Table::Items)) {
                Ok(counts) => counts.listed,
                Err(e) => {
                    tracing::warn!("cannot count what was listed: {e}");
                    0
                }
            };
            vec![activity::event(Kind::Listed, folder.clone(), activity::items(listed))]
        }
        Said::EachChange => {
            let each = applied
                .changes
                .iter()
                .map(|c| {
                    let from = c.from.as_deref().map(|from| format!("from {}", shown(from))).unwrap_or_default();
                    activity::event(c.kind, shown(&c.rel), from)
                })
                .collect();
            activity::capped(each, activity::PER_KIND, &folder)
        }
        Said::Nothing => Vec::new(),
    };
    let at = activity::unix_now();
    let conflicts: Vec<ConflictRow> = applied
        .rescued
        .iter()
        .map(|r| ConflictRow { at, original: shown(&r.original), rescued: r.rescued.display().to_string() })
        .collect();
    // Capped like every other kind; every conflict is
    // still a row.
    let each = conflicts.iter().map(|c| activity::event(Kind::Conflict, c.original.clone(), c.rescued.clone())).collect();
    events.extend(activity::capped(each, activity::PER_KIND, &folder));
    report.activity.add_conflicts(conflicts);
    report.activity.record_blocking(events);
}

/// How often the poller runs a cycle, and how soon after a failure.
#[derive(Debug, Clone)]
pub struct Schedule {
    pub interval: Duration,
    /// The waits after the first, second, … failure in a row; after the last,
    /// the ordinary interval.
    pub retry: Vec<Duration>,
}

impl Default for Schedule {
    fn default() -> Self {
        Self { interval: Duration::from_secs(60), retry: vec![Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(30)] }
    }
}

/// Runs a cycle at once, then every `interval`, at once on `refresh()`, and on
/// the retry schedule after a failure (Poller).
pub struct Poller {
    refresh: Arc<Notify>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    listing: Arc<Listing>,
}

impl Poller {
    pub fn start(listing: Arc<Listing>, schedule: Schedule) -> Self {
        let refresh = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(Arc::clone(&listing), schedule, Arc::clone(&refresh), cancel.clone()));
        Self { refresh, cancel, task, listing }
    }

    pub fn refresh(&self) {
        self.refresh.notify_one();
    }

    /// Stops the poller and every replacement under way, and waits for them.
    pub async fn stop(self) {
        self.cancel.cancel();
        self.listing.cancel_replacements.cancel();
        let _ = self.task.await;
        self.listing.join_replacements().await;
    }
}

async fn run(listing: Arc<Listing>, schedule: Schedule, refresh: Arc<Notify>, cancel: CancellationToken) {
    let mut failures = 0usize;
    loop {
        let result = listing.cycle(&cancel).await;
        let wait = match &result {
            Ok(_) => {
                failures = 0;
                schedule.interval
            }
            Err(CycleError::Cancelled) => return,
            Err(e) => {
                tracing::warn!("the sync with OneDrive failed: {e}");
                let wait = schedule.retry.get(failures).copied().unwrap_or(schedule.interval);
                failures += 1;
                wait
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = refresh.notified() => {}
            () = cancel.cancelled() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;

    use konedrive_fs::placeholder::{self, State, XATTR_ROOT};
    use konedrive_proto::{Channel, ToDaemon, ToHelper, PROTOCOL_VERSION};
    use nix::sys::socket::{accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr};
    use serde_json::{json, Value};
    use url::Url;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};
    use xattr::FileExt;

    use super::*;
    use crate::drive::RetryPolicy;
    use crate::sync::graph_source::GraphSource;
    use crate::sync::materialize::Rescued;
    use crate::sync::SyncSnapshot;
    use crate::token::StaticToken;
    use crate::tree::TreeStore;

    struct Setup {
        server: MockServer,
        _dir: tempfile::TempDir,
        root: SyncRoot,
        store: Store,
        state: SyncStateHandle,
        /// Where every listing made from this setup reports,
        /// its activity kept in `store`.
        report: Report,
        /// The preferred rescue directory (`ListingContext::rescue_dir`).
        rescue_dir: PathBuf,
        _rescue: Option<tempfile::TempDir>,
        /// A link to a helper that acknowledges everything: a folder that
        /// shows OneDrive is kept in step only with one (HS2).
        link: HelperLink,
        _helper: tempfile::TempDir,
    }

    impl Drop for Setup {
        fn drop(&mut self) {
            if let Ok(disk) = Disk::open(&self.root, false) {
                let _ = disk.unlock_tree();
            }
        }
    }

    async fn setup() -> Setup {
        let rescue = tempfile::tempdir().unwrap();
        let mut s = setup_rescuing_into(rescue.path().to_path_buf()).await;
        s._rescue = Some(rescue);
        s
    }

    /// The folder is `OneDrive` inside a temporary directory, so that a
    /// rescue directory made beside it is cleaned up with it.
    async fn setup_rescuing_into(rescue_dir: PathBuf) -> Setup {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/me/drive"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D1"})))
            .mount(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().canonicalize().unwrap().join("OneDrive");
        std::fs::create_dir(&folder).unwrap();
        let root_id = "8f6c0a3e-3b0e-4d7a-9c1e-5b2d7e4f1a90".to_owned();
        File::open(&folder).unwrap().set_xattr(XATTR_ROOT, root_id.as_bytes()).unwrap();
        let store = Store::new(TreeStore::in_memory().unwrap());
        // The folder is what `SyncService` has registered: what its events are about.
        let state = SyncStateHandle::new(SyncSnapshot { root_path: folder.display().to_string(), ..SyncSnapshot::default() });
        let report = Report::new(state.clone());
        report.activity.attach(store.clone(), &folder);
        let helper = tempfile::tempdir().unwrap();
        let socket_path = helper.path().join("helper.sock");
        recording_helper(&socket_path);
        let link = HelperLink::connect(&socket_path).await.unwrap().0;
        Setup {
            server,
            _dir: dir,
            root: SyncRoot { path: folder, root_id },
            store,
            state,
            report,
            rescue_dir,
            _rescue: None,
            link,
            _helper: helper,
        }
    }

    impl Setup {
        fn drive(&self) -> DriveClient {
            DriveClient::new(Url::parse(&format!("{}/", self.server.uri())).unwrap(), Arc::new(StaticToken::new("T")))
                .unwrap()
                .with_retry(RetryPolicy { attempts: 2, default_wait: Duration::from_millis(5), max_wait: Duration::from_millis(10) })
        }

        fn context(&self) -> ListingContext {
            ListingContext {
                root: self.root.clone(),
                intercepted: true,
                store: self.store.clone(),
                drive: self.drive(),
                drive_record: None,
                source: Arc::new(GraphSource::new(self.drive())),
                link: Arc::new(std::sync::Mutex::new(Some(self.link.clone()))),
                locks: InodeLocks::new(),
                state: self.state.clone(),
                lifecycle: Arc::new(tokio::sync::RwLock::new(())),
                rescue_dir: self.rescue_dir.clone(),
                full_threshold: FULL_THRESHOLD,
                after_cycle: None,
                report: self.report.clone(),
            }
        }

        /// Every event recorded so far, oldest first, as (kind, path, detail).
        fn activity(&self) -> Vec<(String, String, String)> {
            let mut events = self.report.activity.recent(1000).unwrap();
            events.reverse();
            events.into_iter().map(|e| (e.kind, e.path, e.detail)).collect()
        }

        /// A full path in the folder, as events name it.
        fn full(&self, rel: &str) -> String {
            self.root.path.join(rel).display().to_string()
        }

        fn listing(&self) -> Arc<Listing> {
            Listing::new(self.context())
        }

        fn listing_with(&self, full_threshold: usize) -> Arc<Listing> {
            Listing::new(ListingContext { full_threshold, ..self.context() })
        }

        fn link(&self, token: &str) -> String {
            format!("{}/me/drive/root/delta?token={token}", self.server.uri())
        }

        /// The delta feed from `from` (None: the start) answers `items` and
        /// ends with the link `next`, once.
        async fn feed(&self, from: Option<&str>, items: Value, next: &str) {
            self.feed_after(from, items, next, Duration::ZERO).await;
        }

        /// [`Self::feed`], answering only after `delay`.
        async fn feed_after(&self, from: Option<&str>, items: Value, next: &str, delay: Duration) {
            let mock = Mock::given(method("GET")).and(path("/me/drive/root/delta"));
            let mock = match from {
                Some(token) => mock.and(query_param("token", token)),
                None => mock,
            };
            mock.respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"value": items, "@odata.deltaLink": self.link(next)}))
                    .set_delay(delay),
            )
            .up_to_n_times(1)
            .with_priority(if from.is_some() { 1 } else { 5 })
            .mount(&self.server)
            .await;
        }

        /// Page `from` of the delta feed (None: the first) holds `items`,
        /// and the page after it is at the link `next`, once.
        async fn page(&self, from: Option<&str>, items: Value, next: &str) {
            let body = json!({"value": items, "@odata.nextLink": self.link(next)});
            self.answer(from, ResponseTemplate::new(200).set_body_json(body)).await;
        }

        /// The delta request from `from` (None: the start) is answered by
        /// `respond`, once.
        async fn answer(&self, from: Option<&str>, respond: impl wiremock::Respond + 'static) {
            let mock = Mock::given(method("GET")).and(path("/me/drive/root/delta"));
            let mock = match from {
                Some(token) => mock.and(query_param("token", token)),
                None => mock,
            };
            mock.respond_with(respond)
                .up_to_n_times(1)
                .with_priority(if from.is_some() { 1 } else { 5 })
                .mount(&self.server)
                .await;
        }

        /// The delta request from `from`, held: the channel says when it is
        /// asked, and the answer — no page at all — comes only after twice
        /// [`PATIENCE`], longer than any test step waits (see [`within`]),
        /// so a test sees the request still open for as long as it looks.
        async fn held(&self, from: Option<&str>) -> tokio::sync::mpsc::UnboundedReceiver<()> {
            let (asked, heard) = tokio::sync::mpsc::unbounded_channel();
            self.answer(from, move |_: &Request| {
                let _ = asked.send(());
                ResponseTemplate::new(200).set_delay(2 * PATIENCE)
            })
            .await;
            heard
        }

        /// The `token` of every delta request so far, in order; `None` for
        /// one from the start.
        async fn delta_tokens(&self) -> Vec<Option<String>> {
            self.server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.url.path() == "/me/drive/root/delta")
                .map(|r| r.url.query_pairs().find(|(k, _)| k == "token").map(|(_, v)| v.into_owned()))
                .collect()
        }

        /// Graph's metadata for F at version `ctag`, holding `content`.
        fn version(&self, ctag: &str, content: &[u8]) -> ResponseTemplate {
            let mut hash = crate::quickxor::QuickXor::new();
            hash.update(content);
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "F", "name": "f.txt", "size": content.len(), "cTag": ctag,
                "file": {"hashes": {"quickXorHash": hash.finish_base64()}},
                "@microsoft.graph.downloadUrl": format!("{}/dl/F/{ctag}", self.server.uri())
            }))
        }

        /// Graph's metadata for F at version c2, holding `content`.
        fn new_version(&self, content: &[u8]) -> ResponseTemplate {
            self.version("c2", content)
        }

        /// The bytes of F's version `ctag`.
        async fn serve_download(&self, ctag: &str, content: &[u8]) {
            Mock::given(method("GET")).and(path(format!("/dl/F/{ctag}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
                .mount(&self.server).await;
        }

        /// Serves `content` as F's version c2, its metadata answered by `metadata`.
        async fn serve_new_version(&self, content: &[u8], metadata: impl wiremock::Respond + 'static) {
            Mock::given(method("GET")).and(path("/me/drive/items/F"))
                .respond_with(metadata)
                .with_priority(2)
                .mount(&self.server).await;
            self.serve_download("c2", content).await;
        }
    }

    fn root_item() -> Value {
        json!({"id": "R", "root": {}, "folder": {}})
    }

    fn folder(id: &str, parent: &str, name: &str) -> Value {
        json!({"id": id, "name": name, "folder": {}, "parentReference": {"id": parent}})
    }

    fn file(id: &str, parent: &str, name: &str, ctag: &str) -> Value {
        json!({"id": id, "name": name, "size": 10, "cTag": ctag, "file": {}, "parentReference": {"id": parent},
               "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}})
    }

    fn vault() -> Value {
        json!({"id": "V", "name": "Personal Vault", "folder": {}, "specialFolder": {"name": "vault"}, "parentReference": {"id": "R"}})
    }

    async fn delta_requests(server: &MockServer) -> usize {
        server.received_requests().await.unwrap().iter().filter(|r| r.url.path() == "/me/drive/root/delta").count()
    }

    async fn listed(s: &Setup) -> Arc<Listing> {
        listed_with(s, s.context()).await
    }

    /// [`listed`], through a listing made from `context`.
    async fn listed_with(s: &Setup, context: ListingContext) -> Arc<Listing> {
        s.feed(None, json!([root_item(), folder("D", "R", "docs"), file("F", "D", "f.txt", "c1"), vault()]), "L1").await;
        let listing = Listing::new(context);
        listing.cycle(&CancellationToken::new()).await.unwrap();
        listing
    }

    /// The longest a page-by-page test waits for anything: a regression that
    /// would hang it fails it in seconds instead.
    const PATIENCE: Duration = Duration::from_secs(10);

    /// `work`, or a failure once [`PATIENCE`] is out.
    async fn within<T>(work: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(PATIENCE, work).await.expect("waited longer than a test should")
    }

    /// A cycle of `listing` in the background, stopped by `cancel`.
    fn spawn_cycle(listing: &Arc<Listing>, cancel: &CancellationToken) -> tokio::task::JoinHandle<Result<CycleReport, CycleError>> {
        let (listing, cancel) = (Arc::clone(listing), cancel.clone());
        tokio::spawn(async move { listing.cycle(&cancel).await })
    }

    /// Everything beneath `root`, hidden names too, as sorted relative paths.
    fn tree_of(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut pending = vec![PathBuf::new()];
        while let Some(rel) = pending.pop() {
            for entry in std::fs::read_dir(root.join(&rel)).unwrap() {
                let entry = entry.unwrap();
                let child = rel.join(entry.file_name());
                if entry.file_type().unwrap().is_dir() {
                    pending.push(child.clone());
                }
                out.push(child.display().to_string());
            }
        }
        out.sort();
        out
    }

    fn ino(at: &Path) -> u64 {
        std::fs::symlink_metadata(at).unwrap().ino()
    }

    fn mode(at: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(at).unwrap().permissions().mode() & 0o7777
    }

    /// Downloads the file at `at` by hand, as a finished fill leaves it:
    /// `content` at version c1.
    fn hydrate_by_hand(at: &Path, content: &[u8]) {
        use std::os::unix::fs::FileExt as _;
        let file = placeholder::reopen_writable(&File::open(at).unwrap()).unwrap();
        file.write_all_at(content, 0).unwrap();
        placeholder::write_ctag(&file, "c1").unwrap();
        placeholder::write_state(&file, State::Hydrated).unwrap();
        placeholder::write_stamp(&file).unwrap();
    }

    /// Renames `docs` to `papers` in the (locked) folder, as a user with
    /// their own chmod might while a replacement downloads.
    fn move_docs_away(root: &Path) {
        let dir = File::open(root).unwrap();
        placeholder::with_owner_write(&dir, || std::fs::rename(root.join("docs"), root.join("papers"))).unwrap();
    }

    /// A helper that acknowledges everything at once, except the marking of
    /// the directory whose path ends in `stall_on`: it says so on the first
    /// channel, and acknowledges only when told to on the second.
    fn stalling_helper(socket_path: &Path, stall_on: &'static str) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let listener = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None).unwrap();
        bind(listener.as_raw_fd(), &UnixAddr::new(socket_path).unwrap()).unwrap();
        listen(&listener, Backlog::new(4).unwrap()).unwrap();
        let (reached_tx, reached_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            let accepted = accept(listener.as_raw_fd()).unwrap();
            // SAFETY: a descriptor `accept` just returned, owned by nothing else.
            let mut channel = Channel::new(unsafe { UnixStream::from_raw_fd(accepted) }).unwrap();
            channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
            let _ = channel.recv::<ToHelper>().unwrap();
            channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
            while let Ok((message, dir)) = channel.recv::<ToHelper>() {
                if let (ToHelper::MarkDir, Some(dir)) = (&message, dir) {
                    let at = std::fs::read_link(format!("/proc/self/fd/{}", dir.as_raw_fd())).unwrap();
                    if at.to_string_lossy().ends_with(stall_on) {
                        let _ = reached_tx.send(());
                        let _ = release_rx.recv();
                    }
                }
                if channel.send(&ToDaemon::Ack { errno: 0 }, None).is_err() {
                    break;
                }
            }
        });
        (reached_rx, release_tx)
    }

    /// What a [`recording_helper`] was asked to mark, in order: each
    /// directory's item id (None for the holding directory), and how many
    /// entries it held right then.
    type Marks = Arc<std::sync::Mutex<Vec<(Option<String>, usize)>>>;

    /// A helper that acknowledges everything, and keeps what it marked.
    fn recording_helper(socket_path: &Path) -> Marks {
        let listener = socket(AddressFamily::Unix, SockType::SeqPacket, SockFlag::SOCK_CLOEXEC, None).unwrap();
        bind(listener.as_raw_fd(), &UnixAddr::new(socket_path).unwrap()).unwrap();
        listen(&listener, Backlog::new(4).unwrap()).unwrap();
        let marks: Marks = Arc::default();
        let kept = Arc::clone(&marks);
        std::thread::spawn(move || {
            let accepted = accept(listener.as_raw_fd()).unwrap();
            // SAFETY: a descriptor `accept` just returned, owned by nothing else.
            let mut channel = Channel::new(unsafe { UnixStream::from_raw_fd(accepted) }).unwrap();
            channel.send(&ToDaemon::Welcome { version: PROTOCOL_VERSION }, None).unwrap();
            let _ = channel.recv::<ToHelper>().unwrap();
            channel.send(&ToDaemon::Ack { errno: 0 }, None).unwrap();
            while let Ok((message, dir)) = channel.recv::<ToHelper>() {
                if let (ToHelper::MarkDir, Some(dir)) = (&message, dir) {
                    let dir = File::from(dir);
                    let id = dir.get_xattr(placeholder::XATTR_ITEM_ID).unwrap().map(|v| String::from_utf8(v).unwrap());
                    let inside = std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd())).unwrap().count();
                    kept.lock().unwrap().push((id, inside));
                }
                if channel.send(&ToDaemon::Ack { errno: 0 }, None).is_err() {
                    break;
                }
            }
        });
        marks
    }

    #[tokio::test]
    async fn an_initial_listing_fills_the_folder_and_stores_the_link() {
        let s = setup().await;
        let mut states = s.state.subscribe();
        let seen_listing = tokio::spawn(async move {
            loop {
                if states.borrow_and_update().listing {
                    return true;
                }
                if states.changed().await.is_err() {
                    return false;
                }
            }
        });
        // Slow enough that `listing = true` is still published when the
        // watcher looks.
        Mock::given(method("GET")).and(path("/me/drive/root/delta"))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"value": [root_item(), folder("D", "R", "docs"), file("F", "D", "f.txt", "c1"), vault()],
                                      "@odata.deltaLink": s.link("L1")}))
                .set_delay(Duration::from_millis(300)))
            .up_to_n_times(1)
            .mount(&s.server).await;
        s.listing().cycle(&CancellationToken::new()).await.unwrap();
        assert!(s.root.path.join("docs/f.txt").is_file());
        assert_eq!(s.store.run(|t| t.delta_link()).await.unwrap(), Some(s.link("L1")));
        let snapshot = s.state.get();
        assert!(!snapshot.listing);
        assert_eq!((snapshot.items_listed, snapshot.items_placed, snapshot.skipped_count), (3, 2, 1));
        assert_eq!(snapshot.sync_trouble, None);
        assert!(tokio::time::timeout(Duration::from_secs(1), seen_listing).await.unwrap().unwrap(), "`listing` was published while it ran");
    }

    #[tokio::test]
    async fn a_later_cycle_applies_only_the_changes() {
        let s = setup().await;
        let listing = listed(&s).await;
        s.feed(Some("L1"), json!([file("F", "D", "renamed.txt", "c1")]), "L2").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        assert!(!report.full);
        assert!(s.root.path.join("docs/renamed.txt").is_file());
        assert_eq!(s.store.run(|t| t.delta_link()).await.unwrap(), Some(s.link("L2")));
    }

    #[tokio::test]
    async fn an_empty_delta_touches_nothing_but_the_link() {
        let s = setup().await;
        let listing = listed(&s).await;
        let ctime = |p: PathBuf| {
            let m = std::fs::metadata(p).unwrap();
            (m.ctime(), m.ctime_nsec())
        };
        let before = ctime(s.root.path.join("docs"));
        s.feed(Some("L1"), json!([]), "L2").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        assert_eq!((report.full, report.changes), (false, 0));
        assert_eq!(ctime(s.root.path.join("docs")), before);
        assert_eq!(s.store.run(|t| t.delta_link()).await.unwrap(), Some(s.link("L2")));
    }

    /// A feed that has expired is listed again, and what the new
    /// listing no longer has is deleted here.
    #[tokio::test]
    async fn an_expired_feed_lists_again_and_deletes_what_is_gone() {
        let s = setup().await;
        let listing = listed(&s).await;
        Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L1"))
            .respond_with(ResponseTemplate::new(410))
            .with_priority(1)
            .mount(&s.server).await;
        s.feed(None, json!([root_item(), folder("D", "R", "docs")]), "L9").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        assert!(report.full);
        assert!(!s.root.path.join("docs/f.txt").exists());
        assert_eq!(s.store.run(|t| t.delta_link()).await.unwrap(), Some(s.link("L9")));
    }

    #[tokio::test]
    async fn a_very_large_delta_is_reconciled_in_full() {
        let s = setup().await;
        s.feed(None, json!([root_item(), folder("D", "R", "docs")]), "L1").await;
        let listing = s.listing_with(2);
        listing.cycle(&CancellationToken::new()).await.unwrap();
        s.feed(Some("L1"), json!([file("A", "D", "a", "c"), file("B", "D", "b", "c"), file("C", "D", "c", "c")]), "L2").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        assert!(report.full);
        assert!(s.root.path.join("docs/c").is_file());
    }

    #[tokio::test]
    async fn a_new_listing_starts_with_a_full_reconcile() {
        let s = setup().await;
        listed(&s).await;
        Disk::open(&s.root, false).unwrap().unlock_tree().unwrap();
        std::fs::remove_file(s.root.path.join("docs/f.txt")).unwrap();
        s.feed(Some("L1"), json!([]), "L2").await;
        let restarted = s.listing();
        let report = restarted.cycle(&CancellationToken::new()).await.unwrap();
        assert!(report.full);
        assert!(s.root.path.join("docs/f.txt").is_file(), "the folder was repaired from the tree");
    }

    #[tokio::test]
    async fn another_account_blocks_the_folder_and_touches_nothing() {
        let s = setup().await;
        s.store.run(|t| t.set_meta("drive_id", Some("D0"))).await.unwrap();
        s.feed(None, json!([root_item(), folder("D", "R", "docs")]), "L1").await;
        let err = s.listing().cycle(&CancellationToken::new()).await.unwrap_err();
        assert!(matches!(err, CycleError::OtherAccount(_)), "{err:?}");
        assert!(err.blocking());
        assert!(!s.root.path.join("docs").exists());
        assert_eq!(s.state.get().sync_trouble, Some(SyncTrouble { text: err.to_string(), blocking: true }));
    }

    #[tokio::test]
    async fn no_network_is_said_and_is_not_blocking() {
        let s = setup().await;
        let listing = listed(&s).await;
        Mock::given(method("GET")).and(path("/me/drive/root/delta"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .with_priority(1)
            .mount(&s.server).await;
        let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();
        assert!(matches!(err, CycleError::Offline(_)), "{err:?}");
        assert!(!err.blocking());
        assert_eq!(s.state.get().sync_trouble, Some(SyncTrouble { text: err.to_string(), blocking: false }));
        s.feed(Some("L1"), json!([]), "L2").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        assert!(report.full, "a cycle after a failed one reconciles in full (Ruling R7)");
        assert_eq!(s.state.get().sync_trouble, None);
    }

    #[tokio::test]
    async fn a_failed_reconcile_makes_the_next_cycle_full() {
        let s = setup().await;
        let listing = listed(&s).await;
        let root = File::open(&s.root.path).unwrap();
        placeholder::with_owner_write(&root, || root.remove_xattr(XATTR_ROOT)).unwrap();
        s.feed(Some("L1"), json!([file("F", "D", "renamed.txt", "c1")]), "L2").await;
        let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();
        assert!(matches!(err, CycleError::Apply(_)), "{err:?}");
        placeholder::with_owner_write(&root, || root.set_xattr(XATTR_ROOT, s.root.root_id.as_bytes())).unwrap();
        s.feed(Some("L1"), json!([file("F", "D", "renamed.txt", "c1")]), "L2").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        assert!(report.full, "the stored link was not advanced, and the folder is reconciled in full");
        assert!(s.root.path.join("docs/renamed.txt").is_file());
    }

    #[tokio::test]
    async fn the_poller_runs_again_on_refresh_and_stops() {
        let s = setup().await;
        s.feed(None, json!([root_item()]), "L1").await;
        Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"value": [], "@odata.deltaLink": s.link("L1")})))
            .mount(&s.server).await;
        let poller = Poller::start(s.listing(), Schedule { interval: Duration::from_secs(3600), retry: vec![] });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(delta_requests(&s.server).await, 1, "the first cycle runs at once");
        poller.refresh();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(delta_requests(&s.server).await, 2, "Refresh() runs another now, not in an hour");
        tokio::time::timeout(Duration::from_secs(5), poller.stop()).await.expect("stop returns");
    }

    #[tokio::test]
    async fn a_failed_cycle_is_retried_on_the_retry_schedule() {
        let s = setup().await;
        Mock::given(method("GET")).and(path("/me/drive/root/delta"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2).with_priority(1)
            .mount(&s.server).await;
        s.feed(None, json!([root_item(), folder("D", "R", "docs")]), "L1").await;
        let poller = Poller::start(s.listing(), Schedule { interval: Duration::from_secs(3600), retry: vec![Duration::from_millis(100)] });
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(s.root.path.join("docs").is_dir(), "retried after 100 ms rather than an hour");
        assert_eq!(s.state.get().sync_trouble, None, "the trouble clears once a cycle succeeds");
        poller.stop().await;
    }

    /// Forget stops the sync before it takes the lifecycle lock for writing,
    /// but whoever holds that lock must never make a stop wait for it. The
    /// cycle asks Graph without the lock, and changes nothing without it.
    #[tokio::test]
    async fn stopping_does_not_wait_for_the_lifecycle_lock() {
        let s = setup().await;
        s.feed(None, json!([root_item(), folder("D", "R", "docs")]), "L1").await;
        let lifecycle = Arc::new(tokio::sync::RwLock::new(()));
        let held = Arc::clone(&lifecycle).write_owned().await;
        let listing = Listing::new(ListingContext { lifecycle: Arc::clone(&lifecycle), ..s.context() });
        let poller = Poller::start(listing, Schedule { interval: Duration::from_secs(3600), retry: vec![] });
        let docs = s.root.path.join("docs");
        let mut staged = false;
        for _ in 0..100 {
            staged = s.store.run(|t| t.get(Table::Staging, "D")).await.unwrap().is_some();
            if staged || docs.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!docs.exists(), "the folder is not changed without the lock");
        assert!(staged, "Graph is asked, and its answer staged, without the lock");
        assert_eq!(delta_requests(&s.server).await, 1);
        tokio::time::timeout(Duration::from_secs(2), poller.stop()).await.expect("stop does not wait for the lock's holder");
        drop(held);
    }

    /// A helper's reconnect takes the lifecycle lock for writing before it
    /// serves fills again; a listing that takes minutes must not keep it
    /// waiting (Z1: opens meanwhile would not be intercepted).
    #[tokio::test]
    async fn a_cycle_asking_graph_does_not_hold_the_lifecycle_lock() {
        let s = setup().await;
        s.feed_after(None, json!([root_item(), folder("D", "R", "docs")]), "L1", Duration::from_secs(30)).await;
        let lifecycle = Arc::new(tokio::sync::RwLock::new(()));
        let listing = Listing::new(ListingContext { lifecycle: Arc::clone(&lifecycle), ..s.context() });
        let cancel = CancellationToken::new();
        let running = tokio::spawn({
            let (listing, cancel) = (Arc::clone(&listing), cancel.clone());
            async move { listing.cycle(&cancel).await }
        });
        for _ in 0..100 {
            if delta_requests(&s.server).await == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(delta_requests(&s.server).await, 1, "the listing has asked");
        let writing = tokio::time::timeout(Duration::from_secs(1), lifecycle.write()).await;
        assert!(writing.is_ok(), "a writer is not kept waiting by a cycle asking Graph");
        drop(writing);
        cancel.cancel();
        assert!(matches!(running.await.unwrap(), Err(CycleError::Cancelled)));
    }

    /// Nor for Graph: a request that hangs is dropped, not waited out.
    #[tokio::test]
    async fn stopping_does_not_wait_for_a_slow_answer_from_graph() {
        let s = setup().await;
        s.feed_after(None, json!([root_item()]), "L1", Duration::from_secs(30)).await;
        let poller = Poller::start(s.listing(), Schedule { interval: Duration::from_secs(3600), retry: vec![] });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(delta_requests(&s.server).await, 1, "the listing has asked");
        assert!(s.state.get().listing);
        tokio::time::timeout(Duration::from_secs(2), poller.stop()).await.expect("stop does not wait for the answer");
        assert!(!s.state.get().listing, "a stopped listing is not said to run");
    }

    /// A rescue is one rename, never a copy (ruling): with the
    /// preferred rescue directory on another filesystem than the folder, the
    /// files go beside the folder instead — and the conflict says so, not
    /// where they would have gone. The preferred one here is under `/proc`,
    /// which is never the folder's filesystem, and nothing is ever written
    /// there.
    #[tokio::test]
    async fn the_conflict_names_the_directory_the_files_really_went_to() {
        let preferred = PathBuf::from("/proc/konedrive-nonexistent/rescued");
        let s = setup_rescuing_into(preferred.clone()).await;
        let listing = listed(&s).await;
        // A file of the user's own where the cloud now puts one.
        let docs = File::open(s.root.path.join("docs")).unwrap();
        placeholder::with_owner_write(&docs, || std::fs::write(s.root.path.join("docs/new.txt"), b"mine")).unwrap();
        s.feed(Some("L1"), json!([file("N", "D", "new.txt", "c1")]), "L2").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();

        let beside = s.root.path.parent().unwrap().join(".konedrive-rescued-OneDrive");
        assert_eq!(report.applied.rescued.len(), 1);
        let kept = &report.applied.rescued[0].rescued;
        assert!(kept.starts_with(&beside), "{}", kept.display());
        assert_eq!(std::fs::read(kept).unwrap(), b"mine");
        let conflicts = s.report.activity.conflicts().unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].rescued, kept.display().to_string());
        assert!(!conflicts[0].rescued.starts_with(&preferred.display().to_string()));
    }

    #[tokio::test]
    async fn a_file_changed_in_the_cloud_is_replaced_after_the_cycle() {
        let s = setup().await;
        let listing = listed(&s).await;
        let f_txt = s.root.path.join("docs/f.txt");
        hydrate_by_hand(&f_txt, b"old conten");
        let new = b"new content".to_vec();
        s.serve_new_version(&new, s.new_version(&new)).await;
        s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
        listing.cycle(&CancellationToken::new()).await.unwrap();
        listing.join_replacements().await;
        assert_eq!(std::fs::read(&f_txt).unwrap(), new);
    }

    /// When the new version cannot be had, the old one stays, the
    /// status says why, and it is tried again.
    #[tokio::test]
    async fn a_replacement_that_fails_is_said_and_tried_again() {
        let s = setup().await;
        let listing = listed(&s).await;
        let f_txt = s.root.path.join("docs/f.txt");
        hydrate_by_hand(&f_txt, b"old conten");
        Mock::given(method("GET")).and(path("/me/drive/items/F"))
            .respond_with(ResponseTemplate::new(404))
            .up_to_n_times(1).with_priority(1)
            .mount(&s.server).await;
        s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
        listing.cycle(&CancellationToken::new()).await.unwrap();
        listing.join_replacements().await;
        assert_eq!(std::fs::read(&f_txt).unwrap(), b"old conten", "the old version stays");
        assert!(s.state.get().replacement_note.contains("could not be updated"), "{:?}", s.state.get().replacement_note);

        let new = b"new content".to_vec();
        s.serve_new_version(&new, s.new_version(&new)).await;
        s.feed(Some("L2"), json!([]), "L3").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        assert!(!report.full, "a failed replacement is retried as it is, with no Full reconcile");
        listing.join_replacements().await;
        assert_eq!(std::fs::read(&f_txt).unwrap(), new);
        assert_eq!(s.state.get().replacement_note, "");
    }

    /// A replacement the disk has no room for is an
    /// `update-failed` event whose detail is exactly "not enough disk space"
    /// — the words the window's notifier turns into "disk full".
    #[tokio::test]
    async fn a_replacement_with_no_room_on_the_disk_says_exactly_that() {
        let s = setup().await;
        let listing = listed(&s).await;
        let f_txt = s.root.path.join("docs/f.txt");
        hydrate_by_hand(&f_txt, b"old conten");
        // A new version no disk here holds beside the old one.
        let huge = json!({"id": "F", "name": "f.txt", "size": 1u64 << 60, "cTag": "c2", "file": {},
                          "parentReference": {"id": "D"}, "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}});
        s.feed(Some("L1"), json!([huge]), "L2").await;
        listing.cycle(&CancellationToken::new()).await.unwrap();
        listing.join_replacements().await;
        assert_eq!(
            s.activity().pop().unwrap(),
            ("update-failed".to_owned(), s.full("docs/f.txt"), activity::NO_DISK_SPACE.to_owned())
        );
        assert!(s.state.get().replacement_note.contains("not enough space"), "{:?}", s.state.get().replacement_note);
        assert_eq!(std::fs::read(&f_txt).unwrap(), b"old conten", "the old version stays");
    }

    /// A replacement that goes through is an `updated` event,
    /// one that fails an `update-failed` event saying why — not `failed`,
    /// which is a download's.
    #[tokio::test]
    async fn a_replacement_is_recorded_as_updated_or_failed() {
        let s = setup().await;
        let listing = listed(&s).await;
        let f_txt = s.root.path.join("docs/f.txt");
        hydrate_by_hand(&f_txt, b"old conten");
        Mock::given(method("GET")).and(path("/me/drive/items/F"))
            .respond_with(ResponseTemplate::new(404))
            .up_to_n_times(1).with_priority(1)
            .mount(&s.server).await;
        s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
        listing.cycle(&CancellationToken::new()).await.unwrap();
        listing.join_replacements().await;
        let (kind, at, why) = s.activity().pop().unwrap();
        assert_eq!((kind.as_str(), at.as_str()), ("update-failed", s.full("docs/f.txt").as_str()));
        assert!(why.contains("could not be downloaded"), "{why}");

        let new = b"new content".to_vec();
        s.serve_new_version(&new, s.new_version(&new)).await;
        s.feed(Some("L2"), json!([]), "L3").await;
        listing.cycle(&CancellationToken::new()).await.unwrap();
        listing.join_replacements().await;
        assert_eq!(s.activity().pop().unwrap(), ("updated".into(), s.full("docs/f.txt"), "11 B".into()));
        assert!(s.report.transfers.list().is_empty(), "no download is left showing");
    }

    /// A replacement retried after every cycle and failing
    /// the same way each time is one `update-failed` event, not one a
    /// minute — on a full disk, where it fails at once, that flushed the
    /// log and notified every minute.
    #[tokio::test]
    async fn a_replacement_that_keeps_failing_the_same_way_is_recorded_once() {
        let s = setup().await;
        let listing = listed(&s).await;
        hydrate_by_hand(&s.root.path.join("docs/f.txt"), b"old conten");
        Mock::given(method("GET")).and(path("/me/drive/items/F"))
            .respond_with(ResponseTemplate::new(404))
            .with_priority(1)
            .mount(&s.server).await;
        s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
        listing.cycle(&CancellationToken::new()).await.unwrap();
        listing.join_replacements().await;
        s.feed(Some("L2"), json!([]), "L3").await;
        listing.cycle(&CancellationToken::new()).await.unwrap();
        listing.join_replacements().await;

        let asked = s.server.received_requests().await.unwrap().iter().filter(|r| r.url.path() == "/me/drive/items/F").count();
        assert_eq!(asked, 2, "it was tried again");
        let recorded = s.activity().into_iter().filter(|(kind, _, _)| kind == "update-failed").count();
        assert_eq!(recorded, 1, "the same failure again is not news: {:?}", s.activity());
        assert!(s.state.get().replacement_note.contains("could not be updated"), "the status still says it");
    }

    /// I1's other half: a failure is news again when its reason changes,
    /// when it is for a newer version, and when the file was replaced since.
    #[tokio::test]
    async fn a_failure_with_a_new_reason_or_version_is_recorded_again() {
        let s = setup().await;
        let listing = s.listing();
        let r = |ctag: &str| Replacement { id: "F".into(), rel: "docs/f.txt".into(), ctag: ctag.into(), size: 10 };
        assert!(listing.record_replacement(&r("c2"), ReplaceOutcome::Failed("a".into())));
        assert!(!listing.record_replacement(&r("c2"), ReplaceOutcome::Failed("a".into())), "the same again");
        assert!(listing.record_replacement(&r("c2"), ReplaceOutcome::NoSpace("b".into())), "another reason");
        assert!(listing.record_replacement(&r("c3"), ReplaceOutcome::NoSpace("b".into())), "a newer version");
        assert!(listing.record_replacement(&r("c3"), ReplaceOutcome::Replaced));
        assert!(listing.record_replacement(&r("c3"), ReplaceOutcome::NoSpace("b".into())), "failing after it went through");
    }

    /// `conflict` events are capped like every other
    /// kind — 50 and "and N more" — while every conflict is still listed.
    #[tokio::test]
    async fn conflict_events_are_capped_like_the_other_kinds() {
        let s = setup().await;
        let kept = tempfile::tempdir().unwrap();
        let rescued = (0..53)
            .map(|n| {
                let at = kept.path().join(format!("f{n:02}.txt"));
                std::fs::write(&at, b"mine").unwrap();
                Rescued { original: format!("docs/f{n:02}.txt").into(), rescued: at }
            })
            .collect();
        record(&s.report, &s.store, &s.root.path, &Applied { rescued, ..Applied::default() }, Said::EachChange);

        let folder = s.root.path.display().to_string();
        let events = s.activity();
        assert_eq!(events.iter().filter(|(kind, at, _)| kind == "conflict" && *at != folder).count(), 50);
        assert!(events.contains(&("conflict".to_owned(), folder, "and 3 more".to_owned())), "{events:?}");
        assert_eq!(s.report.activity.conflicts().unwrap().len(), 53, "every conflict is still listed");
    }

    /// A Changed pass that moves a local file out of
    /// the way and then hands over to a Full reconcile. The Full pass finds
    /// nothing left to rescue, so what the first pass rescued must be the
    /// conflict — a row and an event — or it is lost.
    #[tokio::test]
    async fn a_rescue_made_before_a_full_hand_over_is_still_a_conflict() {
        let s = setup().await;
        let listing = listed(&s).await;
        let root = File::open(&s.root.path).unwrap();
        placeholder::with_owner_write(&root, || std::fs::write(s.root.path.join("top.txt"), b"mine")).unwrap();
        // A new file for `docs`, which is not where the tree has it: the
        // Changed pass rescues `top.txt` first (shallower), then hands over.
        move_docs_away(&s.root.path);
        s.feed(Some("L1"), json!([file("T", "R", "top.txt", "c1"), file("M", "D", "m.txt", "c1")]), "L2").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        assert!(report.full, "the Changed pass handed over to a Full one");

        let original = s.full("top.txt");
        let rows = s.report.activity.conflicts().unwrap();
        assert_eq!(rows.iter().map(|c| c.original.as_str()).collect::<Vec<_>>(), vec![original.as_str()]);
        assert_eq!(std::fs::read(&rows[0].rescued).unwrap(), b"mine");
        assert!(s.activity().contains(&("conflict".to_owned(), original, rows[0].rescued.clone())), "{:?}", s.activity());
        assert_eq!(s.state.get().conflict_count, 1);
    }

    /// A first listing, and any Full reconcile, is ONE summary
    /// event — "N items" — for the whole folder, not one event per item.
    #[tokio::test]
    async fn a_full_reconcile_is_one_listed_event() {
        let s = setup().await;
        let _first = listed(&s).await;
        let folder = s.root.path.display().to_string();
        assert_eq!(s.activity(), vec![("listed".to_owned(), folder.clone(), "3 items".to_owned())]);
        // A new `Listing` reconciles its first cycle in full, whatever the
        // delta holds: two new files are still one event.
        s.feed(Some("L1"), json!([file("N", "D", "n.txt", "c1"), file("M", "D", "m.txt", "c1")]), "L2").await;
        let report = s.listing().cycle(&CancellationToken::new()).await.unwrap();
        assert!(report.full);
        assert_eq!(
            s.activity(),
            vec![("listed".to_owned(), folder.clone(), "3 items".to_owned()), ("listed".to_owned(), folder, "5 items".to_owned())]
        );
    }

    /// An incremental cycle logs what it did item by item, but
    /// at most 50 events of a kind, then one "and N more" for the rest.
    #[tokio::test]
    async fn an_incremental_cycle_logs_at_most_fifty_of_a_kind_and_how_many_more() {
        let s = setup().await;
        let listing = listed(&s).await;
        let folder = s.root.path.display().to_string();
        let mut items: Vec<Value> = (0..53).map(|n| file(&format!("N{n}"), "D", &format!("n{n:02}.txt"), "c1")).collect();
        items.push(json!({"id": "F", "deleted": {"state": "deleted"}}));
        s.feed(Some("L1"), json!(items), "L2").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        assert!(!report.full, "the Changed scope is what is capped");
        let events = s.activity().split_off(1);
        let added_each = events.iter().filter(|(kind, at, _)| kind == "added" && *at != folder).count();
        assert_eq!(added_each, 50);
        assert!(events.contains(&("added".to_owned(), folder.clone(), "and 3 more".to_owned())), "{events:?}");
        assert!(events.contains(&("removed".to_owned(), s.full("docs/f.txt"), String::new())), "{events:?}");
        assert_eq!(events.len(), 52, "{events:?}");
    }

    /// A rescue is a conflict — a row, a `conflict` event saying
    /// where the file was and where it is now — and the row drops off by
    /// itself once the rescued file is gone.
    #[tokio::test]
    async fn a_rescue_is_a_conflict_until_its_file_is_gone() {
        let s = setup().await;
        let listing = listed(&s).await;
        let docs = File::open(s.root.path.join("docs")).unwrap();
        placeholder::with_owner_write(&docs, || std::fs::write(s.root.path.join("docs/new.txt"), b"mine")).unwrap();
        s.feed(Some("L1"), json!([file("N", "D", "new.txt", "c1")]), "L2").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        let rescued = report.applied.rescued[0].rescued.display().to_string();
        let original = s.full("docs/new.txt");

        let conflicts = s.report.activity.conflicts().unwrap();
        let rows: Vec<_> = conflicts.iter().map(|c| (c.original.clone(), c.rescued.clone())).collect();
        assert_eq!(rows, vec![(original.clone(), rescued.clone())]);
        assert_eq!(s.state.get().conflict_count, 1);
        assert_eq!(
            crate::sync::published_error(&s.state.get()),
            "",
            "a conflict is not a problem: LastError says nothing of it, Conflicts() says it all"
        );
        assert!(s.activity().contains(&("conflict".to_owned(), original, rescued.clone())), "{:?}", s.activity());

        std::fs::remove_file(&rescued).unwrap();
        s.feed(Some("L2"), json!([]), "L3").await;
        listing.cycle(&CancellationToken::new()).await.unwrap();
        assert_eq!(s.state.get().conflict_count, 0, "a conflict whose file is gone drops off by the next cycle");
        assert!(s.report.activity.conflicts().unwrap().is_empty());
    }

    /// `LastChecked` is when a cycle last succeeded — kept in the
    /// store for the next start — and a cycle that fails leaves it alone.
    #[tokio::test]
    async fn last_checked_moves_only_when_a_cycle_succeeds() {
        let s = setup().await;
        let before = activity::unix_now();
        let listing = listed(&s).await;
        let checked = s.state.get().last_checked;
        assert!(checked >= before, "{checked} < {before}");
        assert_eq!(s.store.with(|x| x.meta("last_checked")).unwrap(), Some(checked.to_string()));

        // Marked, so that a failed cycle writing the time it ran — the same
        // second, most likely — could not pass for leaving it alone.
        s.state.update(|x| x.last_checked = 7);
        Mock::given(method("GET")).and(path("/me/drive"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D2"})))
            .up_to_n_times(1).with_priority(1)
            .mount(&s.server).await;
        assert!(matches!(listing.cycle(&CancellationToken::new()).await, Err(CycleError::OtherAccount(_))));
        assert_eq!(s.state.get().last_checked, 7, "a failed cycle checked nothing");

        s.feed(Some("L1"), json!([]), "L2").await;
        listing.cycle(&CancellationToken::new()).await.unwrap();
        assert!(s.state.get().last_checked >= before);
    }

    /// A file being filled when its change arrives is left for later
    /// (`Applied::deferred`); a Changed scope never looks at it again, so the
    /// next cycle is a Full one.
    #[tokio::test]
    async fn a_cycle_that_leaves_a_file_for_later_makes_the_next_one_full() {
        let s = setup().await;
        let listing = listed(&s).await;
        let f_txt = s.root.path.join("docs/f.txt");
        placeholder::write_state(&File::open(&f_txt).unwrap(), State::Hydrating).unwrap();
        s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        assert_eq!(report.applied.deferred, 1);
        // The fill ends without the file: it is online-only again.
        placeholder::write_state(&File::open(&f_txt).unwrap(), State::OnlineOnly).unwrap();
        s.feed(Some("L2"), json!([]), "L3").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        assert!(report.full, "the file left for later is looked at again");
        assert_eq!(placeholder::read_ctag(&File::open(&f_txt).unwrap()).unwrap().as_deref(), Some("c2"));
    }

    /// A replacement that ends with nothing to do (here: the folder above the
    /// file moved while it downloaded) makes the next cycle Full, and that
    /// cycle finds the file again and issues its replacement anew.
    #[tokio::test]
    async fn a_replacement_that_finds_its_file_moved_makes_the_next_cycle_full_and_issues_it_again() {
        let s = setup().await;
        let listing = listed(&s).await;
        hydrate_by_hand(&s.root.path.join("docs/f.txt"), b"old conten");
        let new = b"new content".to_vec();
        let (root, moved, answer) = (s.root.path.clone(), AtomicBool::new(false), s.new_version(&new));
        s.serve_new_version(&new, move |_: &Request| {
            if !moved.swap(true, Ordering::SeqCst) {
                move_docs_away(&root);
            }
            answer.clone()
        })
        .await;
        s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        assert_eq!(report.applied.replacements.len(), 1);
        listing.join_replacements().await;
        assert_eq!(std::fs::read(s.root.path.join("papers/f.txt")).unwrap(), b"old conten", "nothing was swapped in");

        s.feed(Some("L2"), json!([]), "L3").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        assert!(report.full, "only a Full reconcile finds the replacement again");
        assert_eq!(report.applied.replacements.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["F"]);
        listing.join_replacements().await;
        assert_eq!(std::fs::read(s.root.path.join("docs/f.txt")).unwrap(), new);
    }

    /// A replacement cut short because the poller stops has no outcome:
    /// it asks for no Full reconcile and changes no note.
    #[tokio::test]
    async fn a_replacement_stopped_with_the_poller_asks_for_nothing() {
        let s = setup().await;
        let listing = listed(&s).await;
        let f_txt = s.root.path.join("docs/f.txt");
        hydrate_by_hand(&f_txt, b"old conten");
        let new = b"new content".to_vec();
        s.serve_new_version(&new, s.new_version(&new).set_delay(Duration::from_secs(30))).await;
        s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
        let poller = Poller::start(Arc::clone(&listing), Schedule { interval: Duration::from_secs(3600), retry: vec![] });
        let mut asked = false;
        for _ in 0..100 {
            asked = s.server.received_requests().await.unwrap().iter().any(|r| r.url.path() == "/me/drive/items/F");
            if asked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(asked, "the replacement is under way");
        tokio::time::timeout(Duration::from_secs(2), poller.stop()).await.expect("the stop cuts the download short");
        assert_eq!(std::fs::read(&f_txt).unwrap(), b"old conten");
        assert_eq!(s.state.get().replacement_note, "");
        s.feed(Some("L2"), json!([]), "L3").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        assert!(!report.full, "a replacement the stop cut short is no reason for a Full reconcile");
    }

    /// A replacement that ends while a cycle reconciles asks for a Full
    /// reconcile after it — the cycle that was running must not swallow the
    /// request when it succeeds. Made deterministic by holding both
    /// replacement slots until the running cycle is stuck marking a folder.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_replacement_that_ends_while_a_cycle_runs_still_makes_the_next_one_full() {
        let s = setup().await;
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let (reached, release) = stalling_helper(&socket_path, "/.konedrive-new-N");
        let link = HelperLink::connect(&socket_path).await.unwrap().0;
        let listing = Listing::new(ListingContext { intercepted: true, link: Arc::new(std::sync::Mutex::new(Some(link))), ..s.context() });
        s.feed(None, json!([root_item(), folder("D", "R", "docs"), file("F", "D", "f.txt", "c1")]), "L1").await;
        listing.cycle(&CancellationToken::new()).await.unwrap();
        hydrate_by_hand(&s.root.path.join("docs/f.txt"), b"old conten");
        let new = b"new content".to_vec();
        let (root, moved, answer) = (s.root.path.clone(), AtomicBool::new(false), s.new_version(&new));
        s.serve_new_version(&new, move |_: &Request| {
            if !moved.swap(true, Ordering::SeqCst) {
                move_docs_away(&root);
            }
            answer.clone()
        })
        .await;

        // The replacement is issued, and waits for a slot.
        let slots = listing.replacement_slots.acquire_many(REPLACEMENT_SLOTS as u32).await.unwrap();
        s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
        listing.cycle(&CancellationToken::new()).await.unwrap();
        // The next cycle starts, and is stuck marking the folder it makes.
        s.feed(Some("L2"), json!([folder("N", "R", "new")]), "L3").await;
        let running = tokio::spawn({
            let listing = Arc::clone(&listing);
            async move { listing.cycle(&CancellationToken::new()).await }
        });
        tokio::task::spawn_blocking(move || reached.recv().unwrap()).await.unwrap();
        // Meanwhile the replacement runs, and ends with nothing to do.
        drop(slots);
        listing.join_replacements().await;
        assert!(!running.is_finished());
        release.send(()).unwrap();
        running.await.unwrap().unwrap();

        s.feed(Some("L3"), json!([]), "L4").await;
        let report = listing.cycle(&CancellationToken::new()).await.unwrap();
        assert!(report.full, "the replacement's request outlived the cycle that was running when it came");
        listing.join_replacements().await;
        assert_eq!(std::fs::read(s.root.path.join("docs/f.txt")).unwrap(), new);
    }

    /// A newer version that arrives while an older one of the same file is
    /// still being fetched is fetched after it, not dropped: nothing else
    /// would ever look at that file again.
    #[tokio::test]
    async fn a_newer_version_that_arrives_while_a_replacement_runs_is_fetched_after_it() {
        let s = setup().await;
        let listing = listed(&s).await;
        let f_txt = s.root.path.join("docs/f.txt");
        hydrate_by_hand(&f_txt, b"old conten");
        let (two, three) = (b"version two".to_vec(), b"version three".to_vec());
        // Graph serves version two once, and version three from then on.
        Mock::given(method("GET")).and(path("/me/drive/items/F"))
            .respond_with(s.version("c2", &two))
            .up_to_n_times(1).with_priority(1)
            .mount(&s.server).await;
        Mock::given(method("GET")).and(path("/me/drive/items/F"))
            .respond_with(s.version("c3", &three))
            .with_priority(2)
            .mount(&s.server).await;
        s.serve_download("c2", &two).await;
        s.serve_download("c3", &three).await;

        let slots = listing.replacement_slots.acquire_many(REPLACEMENT_SLOTS as u32).await.unwrap();
        s.feed(Some("L1"), json!([file("F", "D", "f.txt", "c2")]), "L2").await;
        listing.cycle(&CancellationToken::new()).await.unwrap();
        s.feed(Some("L2"), json!([file("F", "D", "f.txt", "c3")]), "L3").await;
        listing.cycle(&CancellationToken::new()).await.unwrap();
        drop(slots);
        listing.join_replacements().await;
        assert_eq!(std::fs::read(&f_txt).unwrap(), three);
        assert_eq!(placeholder::read_ctag(&File::open(&f_txt).unwrap()).unwrap().as_deref(), Some("c3"));
    }

    /// at every cycle: signing out and in as another account
    /// between two cycles stops the folder before anything is placed.
    #[tokio::test]
    async fn a_sign_in_as_another_account_between_cycles_blocks_the_folder() {
        let s = setup().await;
        let listing = listed(&s).await;
        Mock::given(method("GET")).and(path("/me/drive"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "D2"})))
            .with_priority(1)
            .mount(&s.server).await;
        s.feed(Some("L1"), json!([folder("N", "R", "new")]), "L2").await;
        let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();
        assert!(matches!(err, CycleError::OtherAccount(_)), "{err:?}");
        assert!(err.blocking());
        assert!(!s.root.path.join("new").exists());
    }

    /// the drive a folder was listed from is kept
    /// in `config.toml` beside its root too, so a tree store rebuilt empty —
    /// its `meta` has forgotten the drive — still refuses another account.
    /// The first cycle writes it there.
    #[tokio::test]
    async fn the_drive_kept_beside_the_root_outlives_a_rebuilt_store() {
        let s = setup().await;
        let config_dir = tempfile::tempdir().unwrap();
        let config_file = config_dir.path().join("config.toml");
        let config = crate::config::Config { sync_root_id: s.root.root_id.clone(), ..Default::default() };
        config.save(&config_file).unwrap();
        let record = DriveRecord { config_file: config_file.clone(), root_id: s.root.root_id.clone(), recorded: None };
        listed_with(&s, ListingContext { drive_record: Some(record), ..s.context() }).await;
        assert_eq!(crate::config::Config::load(&config_file).unwrap().sync_root_drive_id, "D1", "written by the first cycle");

        let rebuilt = Store::new(TreeStore::in_memory().unwrap());
        let record = DriveRecord { config_file, root_id: s.root.root_id.clone(), recorded: Some("D0".into()) };
        let listing = Listing::new(ListingContext { store: rebuilt, drive_record: Some(record), ..s.context() });
        let err = listing.cycle(&CancellationToken::new()).await.unwrap_err();
        assert!(matches!(&err, CycleError::OtherAccount(drive) if drive == "D0"), "{err:?}");
    }

    /// A cycle whose future is dropped part-way is a failed one: the Full
    /// reconcile it had taken is asked for again, and a listing it was
    /// running is no longer said to run.
    #[tokio::test]
    async fn a_dropped_cycle_leaves_a_full_reconcile_and_no_listing_behind() {
        let s = setup().await;
        listed(&s).await;
        let restarted = s.listing();
        // The feed has expired, and the listing that follows is slow.
        Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "L1"))
            .respond_with(ResponseTemplate::new(410))
            .up_to_n_times(1).with_priority(1)
            .mount(&s.server).await;
        s.feed_after(None, json!([root_item()]), "L9", Duration::from_secs(30)).await;
        let dropped = tokio::time::timeout(Duration::from_millis(500), restarted.cycle(&CancellationToken::new())).await;
        assert!(dropped.is_err(), "still listing when dropped");
        assert!(!s.state.get().listing, "a dropped listing is not said to run");
        s.feed(Some("L1"), json!([]), "L2").await;
        let report = restarted.cycle(&CancellationToken::new()).await.unwrap();
        assert!(report.full, "the Full reconcile the dropped cycle had taken is asked for again");
    }

    /// Cycles of one listing never overlap (a `refresh` and the poller's
    /// own, say): the second starts from the link the first left.
    #[tokio::test]
    async fn two_cycles_at_once_run_one_after_the_other() {
        let s = setup().await;
        let listing = listed(&s).await;
        s.feed_after(Some("L1"), json!([folder("N", "R", "new")]), "L2", Duration::from_millis(300)).await;
        s.feed(Some("L2"), json!([]), "L3").await;
        let token = CancellationToken::new();
        let (first, second) = tokio::join!(listing.cycle(&token), listing.cycle(&token));
        first.unwrap();
        second.unwrap();
        assert!(s.root.path.join("new").is_dir());
        assert_eq!(s.store.run(|t| t.delta_link()).await.unwrap(), Some(s.link("L3")));
    }

    /// A cycle dropped while its reconcile runs keeps the lifecycle lock, and
    /// its turn, until the reconcile has stopped: a Forget must not take the
    /// lock off a folder something is still changing, and no other cycle may
    /// rebuild `staging` under it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_cycle_keeps_its_locks_until_its_reconcile_stops() {
        let s = setup().await;
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let (reached, release) = stalling_helper(&socket_path, "/.konedrive-new-N");
        let link = HelperLink::connect(&socket_path).await.unwrap().0;
        let lifecycle = Arc::new(tokio::sync::RwLock::new(()));
        let listing = Listing::new(ListingContext {
            intercepted: true,
            link: Arc::new(std::sync::Mutex::new(Some(link))),
            lifecycle: Arc::clone(&lifecycle),
            ..s.context()
        });
        s.feed(None, json!([root_item(), folder("D", "R", "docs")]), "L1").await;
        listing.cycle(&CancellationToken::new()).await.unwrap();
        s.feed(Some("L1"), json!([folder("N", "R", "new")]), "L2").await;
        let reached = tokio::task::spawn_blocking(move || reached.recv().unwrap());
        let token = CancellationToken::new();
        tokio::select! {
            _ = listing.cycle(&token) => panic!("the reconcile cannot end before the helper answers"),
            _ = reached => {}
        }
        s.feed(Some("L2"), json!([]), "L3").await;
        let second = tokio::spawn({
            let listing = Arc::clone(&listing);
            async move { listing.cycle(&CancellationToken::new()).await }
        });
        let early = tokio::time::timeout(Duration::from_millis(300), lifecycle.write()).await;
        assert!(early.is_err(), "the lock is held while the folder is still being changed");
        assert!(!second.is_finished(), "no other cycle runs while the dropped one's reconcile does");
        release.send(()).unwrap();
        let report = second.await.unwrap().unwrap();
        assert!(report.full, "the dropped cycle counts as a failed one");
        let later = tokio::time::timeout(Duration::from_secs(5), lifecycle.write()).await;
        assert!(later.is_ok(), "and the lock is let go once the reconcile has stopped");
        drop(later);
        assert!(s.root.path.join("new").is_dir());
    }

    /// A first listing places each page as it comes. While page 2
    /// is still being asked for, page 1 is in the folder, under the lock,
    /// and in the counts, the store knows where to go on from, and the
    /// lifecycle lock is free for a Forget or a helper's reconnect.
    #[tokio::test]
    async fn a_first_listing_shows_each_page_while_the_next_is_asked_for() {
        let s = setup().await;
        s.page(None, json!([root_item(), folder("D", "R", "docs"), file("F", "D", "f.txt", "c1"), folder("E", "R", "extra")]), "P2").await;
        let mut asked = s.held(Some("P2")).await;
        let lifecycle = Arc::new(tokio::sync::RwLock::new(()));
        let listing = Listing::new(ListingContext { lifecycle: Arc::clone(&lifecycle), ..s.context() });
        let cancel = CancellationToken::new();
        let running = spawn_cycle(&listing, &cancel);
        within(asked.recv()).await.unwrap();

        assert!(lifecycle.try_write().is_ok(), "the lifecycle lock is let go between pages");
        assert_eq!(tree_of(&s.root.path), ["docs", "docs/f.txt", "extra"]);
        assert_eq!(mode(&s.root.path.join("docs")), placeholder::LOCKED_DIR_MODE, "the folder is under the read-only lock between pages");
        assert_eq!(mode(&s.root.path), placeholder::LOCKED_DIR_MODE);
        let snapshot = s.state.get();
        assert!(snapshot.listing, "the listing is still said to run");
        assert_eq!((snapshot.items_listed, snapshot.items_placed), (3, 3));
        assert_eq!(s.store.with(|t| t.listing_next()).unwrap(), Some(s.link("P2")));
        assert_eq!(s.store.with(|t| t.delta_link()).unwrap(), None);
        assert!(s.activity().is_empty(), "the one `listed` event comes at the end: {:?}", s.activity());

        cancel.cancel();
        assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
    }

    /// Across pages: an item whose folder has not come yet waits
    /// for it, and is placed with it. One whose folder never comes is listed
    /// and not placed, as a Full reconcile leaves it.
    #[tokio::test]
    async fn an_item_whose_folder_comes_on_a_later_page_waits_for_it() {
        let s = setup().await;
        s.page(None, json!([root_item(), file("C", "P", "c.txt", "c1"), folder("Q", "R", "q"), file("O", "NOWHERE", "o.txt", "c1")]), "P2").await;
        let seen = Arc::new(std::sync::Mutex::new(None));
        let (root, look) = (s.root.path.clone(), Arc::clone(&seen));
        let answer = ResponseTemplate::new(200)
            .set_body_json(json!({"value": [folder("P", "R", "papers")], "@odata.deltaLink": s.link("L1")}));
        s.answer(Some("P2"), move |_: &Request| {
            *look.lock().unwrap() = Some(tree_of(&root));
            answer.clone()
        })
        .await;
        within(s.listing().cycle(&CancellationToken::new())).await.unwrap();

        assert_eq!(seen.lock().unwrap().take().expect("page 2 was asked for"), ["q"], "c.txt waited for its folder");
        assert_eq!(tree_of(&s.root.path), ["papers", "papers/c.txt", "q"]);
        let snapshot = s.state.get();
        assert_eq!((snapshot.items_listed, snapshot.items_placed), (4, 3), "o.txt is listed, and nowhere");
    }

    /// The riskiest case of across pages: an entry whose folder has
    /// not come yet is held only in `items` once its page is committed. A
    /// stop before its folder comes must not lose it: the listing resumed in
    /// a new `Listing`, as after a restart, places it with its folder.
    #[tokio::test]
    async fn an_entry_waiting_for_its_folder_survives_a_stop() {
        let s = setup().await;
        s.page(None, json!([root_item(), file("C", "P", "c.txt", "c1")]), "P2").await;
        let mut asked = s.held(Some("P2")).await;
        let cancel = CancellationToken::new();
        let running = spawn_cycle(&s.listing(), &cancel);
        within(asked.recv()).await.unwrap();
        cancel.cancel();
        assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
        assert_eq!(tree_of(&s.root.path), Vec::<String>::new(), "c.txt waits for its folder");

        s.feed(Some("P2"), json!([folder("P", "R", "papers")]), "L1").await;
        within(s.listing().cycle(&CancellationToken::new())).await.unwrap();
        assert_eq!(tree_of(&s.root.path), ["papers", "papers/c.txt"]);
        assert_eq!(s.delta_tokens().await, [None, Some("P2".to_owned()), Some("P2".to_owned())]);
    }

    /// Ruling 1 of: a listing stopped between pages resumes where it
    /// stopped — in a new `Listing`, as after a restart — and asks for no
    /// page it placed again. It still ends in one `listed` event.
    #[tokio::test]
    async fn a_listing_stopped_part_way_resumes_where_it_stopped() {
        let s = setup().await;
        s.page(None, json!([root_item(), folder("D", "R", "docs")]), "P2").await;
        s.page(Some("P2"), json!([file("F", "D", "f.txt", "c1")]), "P3").await;
        let mut asked = s.held(Some("P3")).await;
        let cancel = CancellationToken::new();
        let running = spawn_cycle(&s.listing(), &cancel);
        within(asked.recv()).await.unwrap();
        cancel.cancel();
        assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
        assert!(!s.state.get().listing);

        s.feed(Some("P3"), json!([folder("E", "R", "extra")]), "L1").await;
        let report = within(s.listing().cycle(&CancellationToken::new())).await.unwrap();
        assert!(report.full);
        let from = |t: &str| Some(t.to_owned());
        assert_eq!(s.delta_tokens().await, [None, from("P2"), from("P3"), from("P3")], "pages 1 and 2 are not asked for again");
        assert_eq!(tree_of(&s.root.path), ["docs", "docs/f.txt", "extra"]);
        assert_eq!(s.store.with(|t| Ok((t.delta_link()?, t.listing_next()?))).unwrap(), (Some(s.link("L1")), None));
        let folder = s.root.path.display().to_string();
        assert_eq!(s.activity(), vec![("listed".to_owned(), folder, "3 items".to_owned())]);
        let snapshot = s.state.get();
        assert_eq!((snapshot.listing, snapshot.items_listed, snapshot.items_placed), (false, 3, 3));
    }

    /// A stopped listing whose resume link Graph refuses — expired (`410`),
    /// or a token it no longer takes (`400`) — lists the drive again from
    /// the start and reconciles the folder once in full, as after any
    /// expired feed: what is placed is found by its id, not made again, and
    /// none of it is rescued.
    #[tokio::test]
    async fn a_refused_resume_link_lists_again_from_the_start_without_duplicates() {
        for refusal in [410, 400] {
            let s = setup().await;
            s.page(None, json!([root_item(), folder("D", "R", "docs"), file("F", "D", "f.txt", "c1")]), "P2").await;
            let mut asked = s.held(Some("P2")).await;
            let cancel = CancellationToken::new();
            let running = spawn_cycle(&s.listing(), &cancel);
            within(asked.recv()).await.unwrap();
            cancel.cancel();
            assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
            let placed = ino(&s.root.path.join("docs/f.txt"));

            s.answer(Some("P2"), ResponseTemplate::new(refusal)).await;
            s.feed(None, json!([root_item(), folder("D", "R", "docs"), file("F", "D", "f.txt", "c1"), folder("E", "R", "extra")]), "L1").await;
            let report = within(s.listing().cycle(&CancellationToken::new())).await.unwrap();

            assert!(report.full, "{refusal}");
            assert!(report.applied.rescued.is_empty(), "{refusal}: {:?}", report.applied.rescued);
            assert!(s.report.activity.conflicts().unwrap().is_empty(), "{refusal}");
            assert_eq!(tree_of(&s.root.path), ["docs", "docs/f.txt", "extra"], "{refusal}");
            assert_eq!(ino(&s.root.path.join("docs/f.txt")), placed, "{refusal}: the placeholder was found, not made again");
            let from = |t: &str| Some(t.to_owned());
            assert_eq!(s.delta_tokens().await, [None, from("P2"), from("P2"), None], "{refusal}");
            assert_eq!(s.store.with(|t| Ok((t.delta_link()?, t.listing_next()?))).unwrap(), (Some(s.link("L1")), None), "{refusal}");
            let folder = s.root.path.display().to_string();
            assert_eq!(s.activity(), vec![("listed".to_owned(), folder, "3 items".to_owned())], "{refusal}");
        }
    }

    /// Only the resume link a stopped listing left is one Graph may refuse
    /// and send the listing back to the start. A next-page link handed out
    /// earlier in the same cycle that Graph turns down fails the cycle as
    /// any trouble with Graph does; the listing stays page by page, and the
    /// next cycle resumes at that page.
    #[tokio::test]
    async fn a_next_page_turned_down_fails_the_cycle_and_the_listing_resumes_there() {
        let s = setup().await;
        s.page(None, json!([root_item(), folder("D", "R", "docs")]), "P2").await;
        s.answer(Some("P2"), ResponseTemplate::new(400)).await;
        let listing = s.listing();
        let err = within(listing.cycle(&CancellationToken::new())).await.unwrap_err();
        assert!(matches!(err, CycleError::Offline(_)), "{err:?}");
        assert_eq!(s.store.with(|t| t.listing_next()).unwrap(), Some(s.link("P2")), "still page by page, at page 2");
        assert_eq!(tree_of(&s.root.path), ["docs"]);

        s.feed(Some("P2"), json!([folder("E", "R", "extra")]), "L1").await;
        within(listing.cycle(&CancellationToken::new())).await.unwrap();
        let from = |t: &str| Some(t.to_owned());
        assert_eq!(s.delta_tokens().await, [None, from("P2"), from("P2")], "page 1 is not asked for again");
        assert_eq!(tree_of(&s.root.path), ["docs", "extra"]);
        assert_eq!(s.store.with(|t| Ok((t.delta_link()?, t.listing_next()?))).unwrap(), (Some(s.link("L1")), None));
    }

    /// Only the first listing is placed page by page (Ruling 2 of):
    /// a later cycle's delta, however many pages it has, still goes into
    /// `staging` and changes the folder only once all of it is in.
    #[tokio::test]
    async fn a_later_cycle_still_changes_the_folder_only_once_its_delta_is_all_in() {
        let s = setup().await;
        let listing = listed(&s).await;
        s.page(Some("L1"), json!([folder("N", "R", "new")]), "P2").await;
        let mut asked = s.held(Some("P2")).await;
        let cancel = CancellationToken::new();
        let running = spawn_cycle(&listing, &cancel);
        within(asked.recv()).await.unwrap();

        assert!(!s.root.path.join("new").exists(), "nothing of the delta is placed before all of it is in");
        assert_eq!(s.store.with(|t| Ok((t.delta_link()?, t.listing_next()?))).unwrap(), (Some(s.link("L1")), None));
        cancel.cancel();
        assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
    }

    /// Invariant M1, page by page: every folder is marked through the helper
    /// while it is still empty, the folder above it before it — page 1's
    /// before page 2 is even asked for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_folder_placed_page_by_page_is_marked_before_anything_is_put_in_it() {
        let s = setup().await;
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let marks = recording_helper(&socket_path);
        let link = HelperLink::connect(&socket_path).await.unwrap().0;
        let listing = Listing::new(ListingContext { intercepted: true, link: Arc::new(std::sync::Mutex::new(Some(link))), ..s.context() });
        s.page(None, json!([root_item(), folder("A", "R", "a"), file("AF", "A", "a.txt", "c1"), folder("C", "B", "c"), file("CF", "C", "c.txt", "c1")]), "P2").await;
        let seen = Arc::new(std::sync::Mutex::new(None));
        let (look, marked) = (Arc::clone(&seen), Arc::clone(&marks));
        let answer = ResponseTemplate::new(200).set_body_json(json!({"value": [folder("B", "R", "b")], "@odata.deltaLink": s.link("L1")}));
        s.answer(Some("P2"), move |_: &Request| {
            *look.lock().unwrap() = Some(marked.lock().unwrap().clone());
            answer.clone()
        })
        .await;
        within(listing.cycle(&CancellationToken::new())).await.unwrap();

        let id = |s: &str| Some(s.to_owned());
        assert_eq!(seen.lock().unwrap().take().expect("page 2 was asked for"), [(id("A"), 0)], "page 1's folder was marked, empty, before page 2");
        assert_eq!(*marks.lock().unwrap(), [(id("A"), 0), (id("B"), 0), (id("C"), 0)], "each folder marked while empty, b before the c inside it");
        assert_eq!(tree_of(&s.root.path), ["a", "a/a.txt", "b", "b/c", "b/c/c.txt"]);
    }

    /// A page stopped while it was being placed is not committed: the
    /// listing resumes at that page, and what it had placed already is found
    /// by its id — nothing made twice, nothing rescued, the lock back on it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_page_stopped_while_it_was_being_placed_is_placed_again_without_duplicates() {
        let s = setup().await;
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let (reached, release) = stalling_helper(&socket_path, "/.konedrive-new-G");
        let link = HelperLink::connect(&socket_path).await.unwrap().0;
        let context = || ListingContext { intercepted: true, link: Arc::new(std::sync::Mutex::new(Some(link.clone()))), ..s.context() };
        s.page(None, json!([root_item(), folder("D", "R", "docs")]), "P2").await;
        let page_two = json!({"value": [folder("G", "R", "g"), file("Y", "G", "y.txt", "c1")], "@odata.deltaLink": s.link("L1")});
        Mock::given(method("GET")).and(path("/me/drive/root/delta")).and(query_param("token", "P2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page_two))
            .with_priority(1)
            .mount(&s.server).await;
        let cancel = CancellationToken::new();
        let running = spawn_cycle(&Listing::new(context()), &cancel);
        tokio::task::spawn_blocking(move || reached.recv_timeout(PATIENCE).unwrap()).await.unwrap();
        cancel.cancel();
        release.send(()).unwrap();
        assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
        assert_eq!(s.store.with(|t| t.listing_next()).unwrap(), Some(s.link("P2")), "page 2 was not committed");
        assert_eq!(tree_of(&s.root.path), ["docs", "g"], "g was placed before the stop was seen");
        let g = ino(&s.root.path.join("g"));

        let report = within(Listing::new(context()).cycle(&CancellationToken::new())).await.unwrap();
        let from = |t: &str| Some(t.to_owned());
        assert_eq!(s.delta_tokens().await, [None, from("P2"), from("P2")]);
        assert_eq!(tree_of(&s.root.path), ["docs", "g", "g/y.txt"]);
        assert_eq!(ino(&s.root.path.join("g")), g, "g was found by its id, not made again");
        assert_eq!(mode(&s.root.path.join("g")), placeholder::LOCKED_DIR_MODE);
        assert!(report.applied.rescued.is_empty(), "{:?}", report.applied.rescued);
    }

    /// A first page stopped while it was being placed has committed nothing,
    /// yet the folder holds what it placed: the listing is still one placed
    /// page by page, and starts again from the start as one — page 1's
    /// items found by their ids, page 1 placed before page 2 is asked for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_first_page_stopped_while_it_was_being_placed_starts_again_page_by_page() {
        let s = setup().await;
        let sockets = tempfile::tempdir().unwrap();
        let socket_path = sockets.path().join("helper.sock");
        let (reached, release) = stalling_helper(&socket_path, "/.konedrive-new-G");
        let link = HelperLink::connect(&socket_path).await.unwrap().0;
        let context = || ListingContext { intercepted: true, link: Arc::new(std::sync::Mutex::new(Some(link.clone()))), ..s.context() };
        let page_one = json!([root_item(), folder("A", "R", "a"), file("AF", "A", "a.txt", "c1"), folder("G", "R", "g")]);
        s.page(None, page_one.clone(), "P2").await;
        let cancel = CancellationToken::new();
        let running = spawn_cycle(&Listing::new(context()), &cancel);
        tokio::task::spawn_blocking(move || reached.recv_timeout(PATIENCE).unwrap()).await.unwrap();
        cancel.cancel();
        release.send(()).unwrap();
        assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
        assert_eq!(tree_of(&s.root.path), ["a", "g"], "the stop was seen before a.txt");
        let g = ino(&s.root.path.join("g"));

        s.page(None, page_one, "P2").await;
        let mut asked = s.held(Some("P2")).await;
        let cancel = CancellationToken::new();
        let running = spawn_cycle(&Listing::new(context()), &cancel);
        within(asked.recv()).await.unwrap();
        assert_eq!(tree_of(&s.root.path), ["a", "a/a.txt", "g"], "page 1 was placed before page 2 was asked for");
        assert_eq!(ino(&s.root.path.join("g")), g, "g was found by its id, not made again");
        cancel.cancel();
        assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
        assert_eq!(s.delta_tokens().await, [None, None, Some("P2".to_owned())]);
    }

    /// A folder that already shows the drive — its tree store lost, or the
    /// folder forgotten and registered again — is not placed page by page:
    /// part-way through, an item not listed yet cannot be told from one that
    /// is gone. It is reconciled once, when the whole listing is in, and
    /// what it has is found by its id.
    #[tokio::test]
    async fn a_folder_that_already_shows_the_drive_is_reconciled_once_the_listing_is_in() {
        let s = setup().await;
        listed(&s).await;
        let placed = ino(&s.root.path.join("docs/f.txt"));
        let fresh = Store::new(TreeStore::in_memory().unwrap());
        let listing = Listing::new(ListingContext { store: fresh.clone(), ..s.context() });
        s.page(None, json!([root_item(), folder("E", "R", "extra")]), "P2").await;
        let seen = Arc::new(std::sync::Mutex::new(None));
        let (root, look) = (s.root.path.clone(), Arc::clone(&seen));
        let answer = ResponseTemplate::new(200).set_body_json(
            json!({"value": [folder("D", "R", "docs"), file("F", "D", "f.txt", "c1"), vault()], "@odata.deltaLink": s.link("L2")}),
        );
        s.answer(Some("P2"), move |_: &Request| {
            *look.lock().unwrap() = Some(tree_of(&root));
            answer.clone()
        })
        .await;
        within(listing.cycle(&CancellationToken::new())).await.unwrap();

        assert_eq!(seen.lock().unwrap().take().expect("page 2 was asked for"), ["docs", "docs/f.txt"], "nothing changed part-way");
        assert_eq!(tree_of(&s.root.path), ["docs", "docs/f.txt", "extra"]);
        assert_eq!(ino(&s.root.path.join("docs/f.txt")), placed);
        assert_eq!(fresh.with(|t| t.delta_link()).unwrap(), Some(s.link("L2")));
    }

    /// A rescue made while a page is placed is a conflict at once (spec
    /// §16.2), not at the end of the listing: a listing that never ends must
    /// still say where the file went.
    #[tokio::test]
    async fn a_rescue_made_by_a_page_is_a_conflict_before_the_listing_ends() {
        let s = setup().await;
        std::fs::write(s.root.path.join("docs"), b"mine").unwrap();
        s.page(None, json!([root_item(), folder("D", "R", "docs")]), "P2").await;
        let mut asked = s.held(Some("P2")).await;
        let cancel = CancellationToken::new();
        let running = spawn_cycle(&s.listing(), &cancel);
        within(asked.recv()).await.unwrap();

        let conflicts = s.report.activity.conflicts().unwrap();
        assert_eq!(conflicts.iter().map(|c| c.original.clone()).collect::<Vec<_>>(), [s.full("docs")]);
        assert_eq!(std::fs::read(&conflicts[0].rescued).unwrap(), b"mine");
        assert!(s.activity().contains(&("conflict".to_owned(), s.full("docs"), conflicts[0].rescued.clone())), "{:?}", s.activity());
        cancel.cancel();
        assert!(matches!(within(running).await.unwrap(), Err(CycleError::Cancelled)));
    }

    /// a download cut off by a restart keeps its
    /// checkpoint through startup recovery AND through the Full reconcile
    /// that the restarted sync's first cycle is. The fill's writes moved the
    /// time to now, and that reconcile took the file for a new version and
    /// punched the partial download away — 1.5 GB, seconds after login. Now
    /// only the time is put back.
    #[tokio::test]
    async fn a_partial_download_survives_recovery_and_the_full_reconcile_after_it() {
        use std::os::unix::fs::FileExt as _;
        let s = setup().await;
        listed(&s).await;
        let path = s.root.path.join("docs/f.txt");
        {
            let file = placeholder::reopen_writable(&File::open(&path).unwrap()).unwrap();
            placeholder::write_state(&file, State::Hydrating).unwrap();
            file.write_all_at(b"abcd", 0).unwrap();
            placeholder::write_progress(&file, &placeholder::Progress { ctag: "c1".into(), bytes: 4 }).unwrap();
            file.write_all_at(b"ef", 4).unwrap();
        }
        let nowhere = crate::sync::helper::Clearance::NoLink(s.rescue_dir.join("no-helper.sock"));
        let recovered = crate::sync::root::recover(&nowhere, &s.root, &InodeLocks::new()).await.unwrap();
        assert_eq!(recovered.reset, 1, "{recovered:?}");

        s.feed(Some("L1"), json!([]), "L2").await;
        let restarted = s.listing();
        let report = restarted.cycle(&CancellationToken::new()).await.unwrap();

        assert!(report.full, "a restarted sync reconciles in full first");
        let file = File::open(&path).unwrap();
        assert_eq!(placeholder::read_state(&file).unwrap(), Some(State::OnlineOnly));
        assert_eq!(placeholder::read_progress(&file).unwrap(), Some(placeholder::Progress { ctag: "c1".into(), bytes: 4 }));
        assert_eq!(&std::fs::read(&path).unwrap()[..6], b"abcd\0\0", "the checkpointed bytes stay, the rest is punched");
        assert_eq!(file.metadata().unwrap().mtime(), 1_714_557_600, "the cloud's time is back");
    }
}
