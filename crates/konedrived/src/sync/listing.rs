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

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::{watch, Notify, OwnedMutexGuard};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::activity::{self, Kind, Report, Tracked};
use super::disk::{rescue_base, rescue_stamp, Disk};
use super::helper::HelperLink;
use super::materialize::{replace, replace_leased, Applied, ApplyError, Claimed, Leased, Materializer, ReplaceOutcome, Replacement, Scope};
use super::pin::Pins;
use super::root::SyncRoot;
use super::source::ContentSource;
use super::{InodeLocks, SyncStateHandle, SyncTrouble};
use crate::drive::{DeltaFrom, DeltaNext, DriveClient, DriveError};
use crate::tree::{classify, Change, ConflictKind, ConflictRow, Store, Table, TreeError, TreeStore};

/// A read-write folder's cycle (`docs/design/writes.md` §9).
mod rw;
use rw::RwCycle;
pub use rw::Writes;

/// A delta with more changes than this is reconciled in full.
pub const FULL_THRESHOLD: usize = 5000;

pub type LinkCell = Arc<std::sync::Mutex<Option<HelperLink>>>;

/// The account's drive, as `config.toml` keeps it (A-M5, design §8.1): the
/// same-account check then survives a tree store rebuilt empty, whose `meta`
/// has forgotten it.
#[derive(Clone)]
pub struct DriveRecord {
    pub store: Arc<crate::config::ConfigStore>,
    /// The account's id.
    pub account: String,
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
    /// `SyncService`'s pins: a cycle queues what it placed under a pin, and
    /// a Full reconcile is followed by a sweep.
    pub pins: Arc<Pins>,
    /// Whether the folder is kept under the read-only lock: the account is
    /// read-only (`docs/design/writes.md` §2.2). A switch of mode stops the sync and
    /// starts a new one, so this never changes under a running sync.
    pub locked: bool,
    /// A read-write folder's: its cycle's part in uploading. `None`
    /// for a read-only folder, whose cycle is the read phase's.
    pub writes: Option<Writes>,
    /// The daemon's other parts a cycle asks or tells; `None` in tests.
    pub neighbours: Option<Neighbours>,
    /// What background work runs now (`sync::running`): the poll and the replacements it
    /// runs stop while the account's work does.
    pub running: Arc<crate::sync::running::Running>,
}

/// What a cycle asks of, or tells, the rest of the daemon.
#[derive(Clone)]
pub struct Neighbours {
    /// Whether another account claims an item id: an object carrying it is
    /// never removed here (`docs/design/writes.md` §8.3).
    pub claimed: Claimed,
    /// The drive the account's token reaches, when a cycle finds it is not
    /// the folder's: the account records it and works its
    /// mode out again, so that a read-write account turns read-only.
    pub drive_seen: DriveSeen,
}

/// Told the drive an account's token reaches.
pub type DriveSeen = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum CycleError {
    #[error("signed out: sign in again to keep this folder in step with OneDrive")]
    SignedOut,
    #[error(
        "this folder was listed from another OneDrive account (drive {0}); forget it and register \
         a folder for the account signed in now"
    )]
    OtherAccount(String),
    /// The account is signed in to a drive another account of this daemon has
    /// (design §8.2); that account's label. Two folders of one drive would
    /// download everything twice.
    #[error(
        "this account is signed in to the Microsoft account already connected as '{0}'; sign it \
         out, or remove one of the two"
    )]
    DriveTaken(String),
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
        matches!(
            self,
            CycleError::SignedOut | CycleError::OtherAccount(_) | CycleError::DriveTaken(_) | CycleError::Store(_) | CycleError::NoHelper
        )
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
    /// The replacement workers ([`REPLACE_WORKERS`] at most, issue #39).
    replacements: std::sync::Mutex<JoinSet<()>>,
    /// The replacements waiting for a worker, and how many workers run.
    queued_replacements: std::sync::Mutex<(VecDeque<Replacement>, usize)>,
    cancel_replacements: CancellationToken,
    /// Read-write mode: the outbox commit count the last cycle's fetch
    /// started at; items the outbox committed after it are looked at again
    /// by the next cycle.
    revisit_from: std::sync::atomic::AtomicI64,
}

/// Replacements at once (issue #39): the queue's workers. A guess; each
/// also waits for a slot of the account's transfer pool.
pub const REPLACE_WORKERS: usize = 8;

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
        let Applied {
            created,
            moved,
            deleted,
            updated,
            deferred,
            rescued,
            replacements,
            changes: _,
            pinned,
            unsettled,
            content_waits,
            copies,
            examine,
            recreated,
            taken,
        } = page.applied;
        let all = &mut self.applied;
        all.created += created;
        all.moved += moved;
        all.deleted += deleted;
        all.updated += updated;
        all.deferred += deferred;
        all.rescued.extend(rescued);
        all.replacements.extend(replacements);
        all.pinned.extend(pinned);
        all.unsettled.extend(unsettled);
        all.content_waits.extend(content_waits);
        all.copies.extend(copies);
        all.examine.extend(examine);
        all.recreated.extend(recreated);
        all.taken.extend(taken);
        self.full |= page.full;
    }
}

/// What the feed said since the stored link.
// Made once per cycle and taken apart at once: not worth a box.
#[allow(clippy::large_enum_variant)]
enum Fetched {
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
    matches!(e, DriveError::ResyncRequired | DriveError::ResyncUpload | DriveError::NotFound | DriveError::Failed(_))
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
            replacements: std::sync::Mutex::new(JoinSet::new()),
            queued_replacements: std::sync::Mutex::new((VecDeque::new(), 0)),
            cancel_replacements: CancellationToken::new(),
            revisit_from: std::sync::atomic::AtomicI64::new(0),
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
        // Read-write mode: the first cycle waits for the watcher's Full local scan (write
        // design §3.3), and the stale-delta guard starts from the outbox's commits so far.
        let fetch_seq = match &self.ctx.writes {
            Some(writes) => {
                writes.scanned(cancel).await?;
                Some(self.on_store(turn, |s| s.outbox_seq()).await?)
            }
            None => None,
        };
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
        // Whether this cycle may have changed the tree: its counts are
        // published then, and not in an idle cycle (issue #39).
        let listed = matches!(fetched, Fetched::Listed { .. } | Fetched::Placed(_));
        let (reconciled, changes) = match (fetched, fetch_seq) {
            (Fetched::Placed(placed), _) => (placed, 0),
            (fetched, Some(seq)) => {
                let done = self.reconcile_rw_fetched(turn, fetched, seq, full_requested, cancel).await?;
                self.revisit_from.store(seq, Ordering::SeqCst);
                done
            }
            (Fetched::Listed { link, .. }, None) => (self.reconcile(turn, Scope::Full, Commit::Swap { link, listing: false }, cancel).await?, 0),
            (Fetched::Changes { changes, link }, None) => {
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
        if listed || full || full_requested || changes > 0 {
            self.publish_counts(turn).await?;
        }
        // `LastChecked`: this cycle succeeded. Kept in the store,
        // so a restart still knows when the folder was last in step.
        let now = activity::unix_now();
        self.on_store(turn, move |s| s.set_meta("last_checked", Some(&now.to_string()))).await?;
        self.ctx.state.update(|s| s.last_checked = now);
        // A conflict whose rescued file is gone drops off by itself (spec
        // §16.1), whether or not anyone asks for the list: a batch of them
        // looked over each cycle (issue #39). Not through `on_store`: the
        // activity log takes the store's lock itself.
        let (report, held) = (self.ctx.report.clone(), Arc::clone(turn));
        if let Err(e) = tokio::task::spawn_blocking(move || {
            let _turn = held;
            report.activity.prune();
        })
        .await
        {
            tracing::warn!("the task looking over the conflicts failed: {e}");
        }
        // "Always keep on this device": the sweep finds every pinned file not
        // downloaded yet and counts the pins again. It follows a Full
        // reconcile; a cycle after a pinned download failed; and a cycle that
        // moved or removed anything while pins exist, since a pinned item —
        // or a folder with one inside — may have moved or gone with it.
        // After any other, what it placed under a pin is queued.
        let resweep = self.ctx.pins.take_resweep();
        let moved_pins = (applied.moved > 0 || applied.deleted > 0) && self.ctx.pins.count() > 0;
        if full || resweep || moved_pins {
            self.ctx.pins.sweep(self.ctx.root.path.clone()).await;
        } else if !applied.pinned.is_empty() {
            let placed = applied.pinned.iter().map(|rel| self.ctx.root.path.join(rel)).collect();
            self.ctx.pins.queue_under(placed).await;
        }
        // Paused (`docs/design/writes.md` §11): no replacement starts; the next cycle after
        // the pause is Full, and finds them again.
        let paused = self.ctx.running.stopped(&self.ctx.store);
        if paused && !applied.replacements.is_empty() {
            self.needs_full.store(true, Ordering::SeqCst);
        } else {
            self.spawn_replacements(applied.replacements.clone());
        }
        if let Some(writes) = &self.ctx.writes {
            // The base caught up: the outbox sends (`docs/design/writes.md` §9).
            (writes.cycled)();
        }
        Ok(CycleReport { full, changes, applied })
    }

    /// A tree store call on a blocking thread, holding this cycle's turn
    /// until it is done — even when the cycle's future is dropped meanwhile.
    async fn on_store<T: Send + 'static>(
        &self,
        turn: &Turn,
        f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static,
    ) -> Result<T, CycleError> {
        // The job holds the turn until it has run, whatever becomes of this future.
        let turn = Arc::clone(turn);
        self.ctx
            .store
            .call(move |s| {
                let _turn = turn;
                f(s)
            })
            .await
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
            // The account learns which drive its token reaches now.
            if let Some(neighbours) = &self.ctx.neighbours {
                (neighbours.drive_seen)(&id);
            }
            return Err(CycleError::OtherAccount(recorded));
        }
        // A drive is one account (§8.2): one another account has recorded is not
        // listed a second time into this folder.
        if let Some(record) = self.ctx.drive_record.as_ref().filter(|_| kept.is_none()) {
            let config = record.store.snapshot();
            if let Some(other) = config.accounts.iter().find(|a| a.id != record.account && a.drive_id == id) {
                return Err(CycleError::DriveTaken(other.label.clone()));
            }
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
        // Read-write mode: the outbox's commit count when `staging` was last made from `items`.
        let (seq, counts) = self
            .on_store(turn, |s| {
                s.begin_staging(true)?;
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
                    self.on_store(turn, |s| s.set_meta(crate::tree::LISTING_NEXT, None)).await?;
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
                        self.on_store(turn, |s| s.begin_staging(true)).await?;
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
            shown += done.applied.created;
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
        let (locks, cancel, locked) = (self.ctx.locks.clone(), cancel.clone(), self.ctx.locked);
        let report = self.ctx.report.clone();
        let claimed = self.ctx.neighbours.as_ref().map(|n| Arc::clone(&n.claimed));
        let runtime = tokio::runtime::Handle::current();
        let drive = self.pending_drive.lock().unwrap().take();
        tokio::task::spawn_blocking(move || {
            let _held = held;
            if let Some((record, id)) = drive {
                record_drive(&record, &id);
                // The folder remembers its drive too (design §8.3).
                if let Err(e) = super::root::mark_drive(&root, &id) {
                    tracing::warn!("cannot record the drive on {}: {e}", root.path.display());
                }
            }
            let Some(root_item_id) = store.call_blocking(move |s| s.root_item_id()).map_err(|e| applying(e.into()))? else {
                return match commit {
                    // Nothing on a page can be placed before the drive's
                    // root has come: it waits in `items` like any entry
                    // whose folder has not come yet.
                    Commit::Page { changes, next } => {
                        store.call_blocking(move |s| s.commit_page(&changes, &next))?;
                        Ok(Reconciled::default())
                    }
                    Commit::Swap { .. } => Err(CycleError::Apply("the drive's listing has no root".into())),
                };
            };
            let materializer = Materializer {
                disk: Disk::open(&root, locked).map_err(|e| applying(e.into()))?,
                store: store.clone(),
                link,
                runtime,
                locks,
                root_item_id,
                // One directory for the whole cycle, on the folder's own
                // filesystem: a rescue is one rename, never a copy.
                rescue_into: rescue_base(&root.path, &preferred).join(rescue_stamp(SystemTime::now())),
                cancel,
                rw: None,
                claimed,
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
            // in `Conflicts.List()`, `Conflicts.Count` and a `conflict` event, which
            // `record` below writes. It is not a problem, so `LastError` no
            // longer says it. A page's are written with the page, not at the
            // end of the listing: a listing that never ends must still say
            // where the files went.
            let said = match commit {
                Commit::Swap { link, listing } => {
                    store.call_blocking(move |s| s.commit_staging(&link))?;
                    if listing || full {
                        Said::Listed
                    } else {
                        Said::EachChange
                    }
                }
                Commit::Page { changes, next } => {
                    store.call_blocking(move |s| s.commit_page(&changes, &next))?;
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
        let counts = self.on_store(turn, |s| s.counts()).await?;
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
    /// They wait in one queue, worked by at most [`REPLACE_WORKERS`] tasks
    /// (issue #39): a delta changing thousands of files starts a few tasks,
    /// not one each.
    fn spawn_replacements(self: &Arc<Self>, fresh: Vec<Replacement>) {
        let fresh_ids: HashSet<&str> = fresh.iter().map(|r| r.id.as_str()).collect();
        let retries: Vec<Replacement> = self
            .failed_replacements
            .lock()
            .unwrap()
            .values()
            .filter(|(failed, _)| !fresh_ids.contains(failed.id.as_str()))
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
        self.queue_replacements(start);
    }

    /// Queues `replacements`, each already in `replacing`, and starts
    /// workers for them up to [`REPLACE_WORKERS`].
    fn queue_replacements(self: &Arc<Self>, replacements: Vec<Replacement>) {
        if replacements.is_empty() {
            return;
        }
        let starting = {
            let mut queued = self.queued_replacements.lock().unwrap();
            queued.0.extend(replacements);
            let starting = REPLACE_WORKERS.saturating_sub(queued.1).min(queued.0.len());
            queued.1 += starting;
            starting
        };
        if starting == 0 {
            return;
        }
        let mut tasks = self.replacements.lock().unwrap();
        // Finished ones are kept only for `join_replacements`.
        while tasks.try_join_next().is_some() {}
        for _ in 0..starting {
            let this = Arc::clone(self);
            tasks.spawn(async move { this.replace_queued().await });
        }
    }

    /// A replacement worker: takes the next file from the queue until it is
    /// empty. Cut short by `Poller::stop`: what is left in the queue goes
    /// with no outcome.
    async fn replace_queued(self: Arc<Self>) {
        loop {
            let replacement = {
                let mut queued = self.queued_replacements.lock().unwrap();
                match queued.0.pop_front().filter(|_| !self.cancel_replacements.is_cancelled()) {
                    Some(replacement) => replacement,
                    None => {
                        let left: Vec<Replacement> = queued.0.drain(..).collect();
                        queued.1 -= 1;
                        drop(queued);
                        let mut replacing = self.replacing.lock().unwrap();
                        for replacement in left {
                            replacing.remove(&replacement.id);
                        }
                        return;
                    }
                }
            };
            let outcome = self.cancel_replacements.run_until_cancelled(self.replace_one(&replacement)).await;
            let stopped = outcome.is_none();
            if let Some((outcome, event)) = outcome {
                // A failure retried after every cycle is said once, not a
                // minute (I1).
                let news = self.record_replacement(&replacement, outcome);
                if let Some(event) = event {
                    if news {
                        self.ctx.report.activity.record(vec![event]).await;
                    }
                    self.ctx.report.space.kick();
                }
            }
            let next = {
                let mut replacing = self.replacing.lock().unwrap();
                let next = replacing.remove(&replacement.id).and_then(|running| running.next).filter(|_| !stopped);
                if let Some(next) = &next {
                    replacing.insert(next.id.clone(), InFlight { ctag: next.ctag.clone(), next: None });
                }
                next
            };
            if let Some(next) = next {
                self.queued_replacements.lock().unwrap().0.push_back(next);
            }
        }
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
            ReplaceOutcome::Current | ReplaceOutcome::Busy => None,
        };
        (outcome, event)
    }

    async fn replace_through(&self, source: &Tracked, replacement: &Replacement) -> ReplaceOutcome {
        // A background download in the account's transfer pool; a large one also waits for
        // the large-file limit.
        let size = crate::pool::Size::of(replacement.size);
        let mut slot = self.ctx.drive.pool().acquire_sized(crate::pool::Class::Download, size).await;
        // Opening reads the root's attribute to prove it is still this root:
        // on a blocking thread, like every open (part 1's).
        let (root, locked) = (self.ctx.root.clone(), self.ctx.locked);
        let disk = match tokio::task::spawn_blocking(move || Disk::open(&root, locked)).await {
            Ok(Ok(disk)) => disk,
            Ok(Err(e)) => return ReplaceOutcome::Failed(e.to_string()),
            Err(e) => return ReplaceOutcome::Failed(format!("the replacement task failed: {e}")),
        };
        // Read-write mode: the swap under a write lease and the tree lock, and the new
        // version's deferred change into the base as it lands.
        let outcome = match &self.ctx.writes {
            None => replace(&disk, &self.ctx.locks, source, replacement).await,
            Some(writes) => {
                let leased = Leased { tree_lock: &writes.tree_lock, store: &self.ctx.store };
                replace_leased(&disk, &self.ctx.locks, source, replacement, Some(&leased)).await
            }
        };
        if matches!(outcome, ReplaceOutcome::Replaced) {
            // A new version is a new inode: the item's recorded one now.
            super::local::record_replaced_async(&disk, &self.ctx.store, &replacement.id, &replacement.rel).await;
            slot.succeeded();
        }
        outcome
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
            ReplaceOutcome::Current | ReplaceOutcome::Busy => false,
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
            // Open somewhere: its deferred change brings it back at the next cycle.
            ReplaceOutcome::Busy => {
                failed.remove(&replacement.id);
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

/// Writes the drive into `config.toml` as the account's (A-M5), if it records
/// none yet. Called by a reconcile, on its blocking thread. A failure is
/// logged; the store's `meta` still has the drive.
fn record_drive(record: &DriveRecord, id: &str) {
    if let Err(e) = record.store.record_drive(&record.account, id) {
        tracing::warn!("cannot record the account's drive in config.toml: {e}");
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
    if said == Said::Nothing && applied.rescued.is_empty() && applied.copies.is_empty() {
        return;
    }
    let shown = |rel: &std::path::Path| root.join(rel).display().to_string();
    let folder = root.display().to_string();
    let mut events = match said {
        Said::Listed => {
            let listed = match store.call_blocking(move |s| s.listed_count()) {
                Ok(listed) => listed,
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
        .map(|r| ConflictRow { at, original: shown(&r.original), rescued: r.rescued.display().to_string(), kind: ConflictKind::Rescued })
        // Read-write mode's copies (`docs/design/writes.md` §7): conflicts of kind `copy`, both
        // versions in the folder.
        .chain(applied.copies.iter().map(|c| ConflictRow { at, original: shown(&c.original), rescued: shown(&c.copy), kind: ConflictKind::Copy }))
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
    /// The interval while the notification socket is up (`live`): the poll is only the safety
    /// net for an event OneDrive never sent.
    pub live_interval: Duration,
    /// The live task's waits; `None` runs none, and the poll alone brings changes (the tests
    /// that do not ask for it).
    pub live: Option<super::live::Timing>,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
            retry: vec![Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(30)],
            live_interval: Duration::from_secs(300),
            live: Some(super::live::Timing::default()),
        }
    }
}

impl Schedule {
    /// The poll alone, every `interval`, with these retries: no live task.
    pub fn polled(interval: Duration, retry: Vec<Duration>) -> Self {
        Self { interval, retry, live: None, ..Self::default() }
    }
}

/// Runs a cycle at once, then every `interval` (`live_interval` while the notification
/// socket is up), at once on `refresh()`, and on the retry schedule after a failure
/// (Poller). The live task (`live`), when the schedule has one, runs alongside and stops
/// with it.
pub struct Poller {
    refresh: Arc<Notify>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    listing: Arc<Listing>,
    /// Whether the notification socket is up: the live task says, the poller reads.
    up: Arc<watch::Sender<bool>>,
    live: Option<super::live::Live>,
}

impl Poller {
    pub fn start(listing: Arc<Listing>, schedule: Schedule) -> Self {
        let refresh = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let up = Arc::new(watch::channel(false).0);
        let live = schedule.live.clone().map(|timing| {
            let ctx = super::live::LiveContext {
                drive: listing.ctx.drive.clone(),
                store: listing.ctx.store.clone(),
                running: Arc::clone(&listing.ctx.running),
                state: listing.ctx.state.clone(),
                refresh: Arc::clone(&refresh),
                up: Arc::clone(&up),
            };
            super::live::Live::start(ctx, timing, cancel.clone())
        });
        let task = tokio::spawn(run(Arc::clone(&listing), schedule, Arc::clone(&refresh), cancel.clone(), up.subscribe()));
        Self { refresh, cancel, task, listing, up, live }
    }

    pub fn refresh(&self) {
        self.refresh.notify_one();
    }

    /// The pause, the hold or the network may have changed: the live task looks again.
    pub fn wake_live(&self) {
        if let Some(live) = &self.live {
            live.wake();
        }
    }

    /// Whether the notification socket is up, as the poller reads it.
    pub fn live_up(&self) -> Arc<watch::Sender<bool>> {
        Arc::clone(&self.up)
    }

    /// A cycle now whose reconcile is Full: it places again what is missing
    /// here though OneDrive did not change it (`RestoreDeletes`, an item whose
    /// local object was forgotten).
    pub fn refresh_full(&self) {
        self.listing.needs_full.store(true, Ordering::SeqCst);
        self.refresh.notify_one();
    }

    /// Stops the poller, the live task and every replacement under way, and waits for them.
    pub async fn stop(self) {
        self.cancel.cancel();
        self.listing.cancel_replacements.cancel();
        let _ = self.task.await;
        if let Some(live) = self.live {
            live.join().await;
        }
        self.listing.ctx.state.set_live_changes(super::live::LiveChanges::Off);
        self.listing.join_replacements().await;
    }
}

/// Whether a read-only folder holds changes waiting to upload: its cycles wait meanwhile,
/// and its `LastError` says why ([`run`]). A store that cannot be read holds them back too.
/// What it said goes once none wait.
async fn held_back(listing: &Listing) -> bool {
    if !listing.ctx.locked {
        return false;
    }
    let store = listing.ctx.store.clone();
    let waiting = tokio::task::spawn_blocking(move || store.call_blocking(move |s| s.outbox_len())).await.ok().and_then(Result::ok);
    let note = match waiting {
        Some(0) => String::new(),
        Some(n) => format!(
            "{n} change(s) made here wait to be uploaded, so the folder is not kept in step with \
             OneDrive: they go once the account is read-write again, or are dropped by a forced \
             switch to read-only"
        ),
        None => "the changes waiting to be uploaded cannot be read, so the folder is not kept in step with OneDrive".into(),
    };
    if listing.ctx.state.get().outbox_note != note {
        listing.ctx.state.update(|s| s.outbox_note = note);
    }
    waiting != Some(0)
}

async fn run(
    listing: Arc<Listing>,
    schedule: Schedule,
    refresh: Arc<Notify>,
    cancel: CancellationToken,
    mut up: watch::Receiver<bool>,
) {
    let mut failures = 0usize;
    // False once the sender is gone: `changed` would then answer at once, for good.
    let mut up_open = true;
    loop {
        // Paused (`docs/design/writes.md` §11): OneDrive is not asked, so nothing is
        // replaced either, until the pause ends or `Resume()` nudges.
        // The same for anything else that stops the account's background work.
        if let Some(stop) = listing.ctx.running.stop(&listing.ctx.store) {
            let left = match stop {
                crate::sync::running::Stop::Paused(until) if until > 0 => {
                    Duration::from_secs((until - crate::sync::activity::unix_now()).max(1) as u64)
                }
                _ => schedule.interval,
            };
            tokio::select! {
                () = tokio::time::sleep(left.min(schedule.interval)) => {}
                () = refresh.notified() => {}
                () = cancel.cancelled() => return,
            }
            continue;
        }
        // A read-only folder that holds changes waiting to upload — a switch to read-only
        // nobody forced: a sign-out, the gate, `config.toml` — runs no
        // cycle: the read phase's reconcile would put back the moves and deletes they
        // describe. It waits for read-write again, which sends them, or for the forced switch
        // that drops them; `LastError` says so meanwhile.
        if held_back(&listing).await {
            tokio::select! {
                () = tokio::time::sleep(schedule.interval) => {}
                () = refresh.notified() => {}
                () = cancel.cancelled() => return,
            }
            continue;
        }
        let result = listing.cycle(&cancel).await;
        let wait = match &result {
            Ok(_) => {
                failures = 0;
                None
            }
            Err(CycleError::Cancelled) => return,
            Err(e) => {
                tracing::warn!("the sync with OneDrive failed: {e}");
                let wait = schedule.retry.get(failures).copied().unwrap_or(schedule.interval);
                failures += 1;
                Some(wait)
            }
        };
        // Counted from the end of the cycle: a socket that goes down makes the next cycle
        // due at most `interval` after it, and one that comes up puts it off.
        let since = tokio::time::Instant::now();
        loop {
            let wait = wait.unwrap_or(if *up.borrow_and_update() { schedule.live_interval } else { schedule.interval });
            tokio::select! {
                () = tokio::time::sleep_until(since + wait) => break,
                () = refresh.notified() => break,
                () = cancel.cancelled() => return,
                changed = up.changed(), if up_open => up_open = changed.is_ok(),
            }
        }
    }
}

#[cfg(test)]
mod tests;
