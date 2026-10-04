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

use crate::helper::LinkCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{Notify, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;

use crate::status::activity;
use crate::status::report::Report;
use super::materialize::{Applied, ApplyError, Claimed, Scope};
use crate::hydration::pin::Pins;
use crate::folder::root::SyncRoot;
use crate::hydration::source::ContentSource;
use crate::folder::locks::InodeLocks;
use crate::status::snapshot::{SyncStateHandle, SyncTrouble};
use konedrive_graph::drive::{DeltaFrom, DriveClient, DriveError};
use konedrive_tree::{Change, Store, TreeError, TreeStore};

mod lease;
pub use lease::Lease;
/// The reconcile of one cycle, in either mode: the folder, the commit, what follows.
pub(crate) mod reconcile;
/// A read-write folder's cycle (`docs/design/writes.md` §9).
mod rw;
pub use rw::Writes;
use reconcile::{Commit, Mode, Reconciled};

/// What OneDrive lists: a whole listing, one placed page by page, a delta's changes.
mod fetch;
/// The task that runs a cycle on a schedule, on a refresh and when the live socket says so.
mod poller;
/// Downloaded files that changed in OneDrive, replaced after the cycle.
mod replacements;
pub use poller::{Poller, Schedule};
use replacements::Replacements;
pub use replacements::REPLACE_WORKERS;

/// A delta with more changes than this is reconciled in full.
pub const FULL_THRESHOLD: usize = 5000;

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
    /// The folder's lease (`sync/`'s): a reconcile holds it, so a Forget waits
    /// for one that is changing the folder.
    pub lease: Lease,
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
    /// What background work runs now (`conditions::running`): the poll and the replacements it
    /// runs stop while the account's work does.
    pub running: Arc<crate::conditions::running::Running>,
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
    /// A failure of the tree store, in its own words ("the tree store: …"), that ends
    /// the cycle: at a store call of the cycle's own ([`From<TreeError>`](CycleError::from))
    /// or inside the materializer ([`applying`]); both are this, and stop the folder
    /// (quality finding `RE6`). Not every store failure ends a cycle: those the leaving
    /// walk and the read-write reconcile only log and pass over do not come here
    /// (limitations log F212).
    #[error("{0}")]
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

/// What making the folder match the tree ran into. A failure of the tree store is the
/// trouble it is anywhere else in a cycle: it stops the folder ([`CycleError::blocking`]),
/// and is not the kind that is only said and tried again.
fn applying(e: ApplyError) -> CycleError {
    match e {
        ApplyError::Cancelled => CycleError::Cancelled,
        ApplyError::Tree(e) => CycleError::from(e),
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
pub(crate) type Turn = Arc<OwnedMutexGuard<()>>;

/// Whether the next cycle's reconcile is Full. Asked for through
/// [`Listing::request_full`], and taken by the cycle that then runs it.
struct FullRequest(AtomicBool);

impl FullRequest {
    fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether one was asked for; none is from now on.
    fn take(&self) -> bool {
        self.0.swap(false, Ordering::SeqCst)
    }
}

pub struct Listing {
    ctx: ListingContext,
    /// Asked for from the start: a `Listing`'s first cycle is Full.
    full: FullRequest,
    /// The drive has been written into `config.toml` (A-M5), or is being:
    /// once per `Listing`.
    drive_recorded: AtomicBool,
    /// The drive to write there, by the next reconcile.
    pending_drive: std::sync::Mutex<Option<(DriveRecord, String)>>,
    /// Whose turn it is (see [`Turn`]).
    turns: Arc<tokio::sync::Mutex<()>>,
    /// Downloaded files that changed in OneDrive: replaced after the cycle.
    replacements: Replacements,
    /// Read-write mode: the outbox commit count the last cycle's fetch
    /// started at; items the outbox committed after it are looked at again
    /// by the next cycle.
    revisit_from: std::sync::atomic::AtomicI64,
    /// Tests only: a read-write cycle is queued for the tree lock (`rw`'s `waits_for_tree`).
    #[cfg(test)]
    waits_for_tree: AtomicBool,
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

/// Whether Graph refused a link it handed out — expired (`410`), gone, or a
/// token it no longer takes — rather than could not be reached or refused
/// the account.
fn refused(e: &DriveError) -> bool {
    matches!(e, DriveError::ResyncRequired | DriveError::ResyncUpload | DriveError::NotFound | DriveError::Failed(_))
}

impl Listing {
    pub fn new(ctx: ListingContext) -> Arc<Self> {
        let replacements = Replacements::new(ctx.state.clone());
        Arc::new(Self {
            ctx,
            full: FullRequest(AtomicBool::new(true)),
            drive_recorded: AtomicBool::new(false),
            pending_drive: std::sync::Mutex::new(None),
            turns: Arc::new(tokio::sync::Mutex::new(())),
            replacements,
            revisit_from: std::sync::atomic::AtomicI64::new(0),
            #[cfg(test)]
            waits_for_tree: AtomicBool::new(false),
        })
    }

    /// One cycle, and what came of it published.
    ///
    /// Cycles of one `Listing` run one at a time (a `refresh` and the
    /// poller's own, say). One that fails, or whose future is dropped
    /// part-way, leaves the next one a Full reconcile.
    /// Asks for the next cycle's reconcile to be Full: it looks at the whole
    /// folder, not only at what the delta names. The one way to ask.
    fn request_full(&self) {
        self.full.request();
    }

    pub async fn cycle(self: &Arc<Self>, cancel: &CancellationToken) -> Result<CycleReport, CycleError> {
        let result = self.take_turn(cancel).await;
        let was_stopped = self.ctx.state.get().sync_trouble.is_some_and(|t| t.blocking);
        self.publish_outcome(&result);
        // The trouble that closed the write gate is gone only now, after the cycle's own
        // word to the outbox (`Writes::cycled`): the worker is told again. Whatever the
        // cycle came to: one that failed with trouble that is only said opens the gate too.
        let is_stopped = self.ctx.state.get().sync_trouble.is_some_and(|t| t.blocking);
        if let Some(writes) = self.ctx.writes.as_ref().filter(|_| was_stopped && !is_stopped) {
            (writes.reopened)();
        }
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
        let full_requested = self.full.take();
        // Unless this cycle succeeds, the next one is Full — also
        // when its future is dropped part-way.
        let mut unless_done = OnDrop(Some(|| self.request_full()));
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
            (Fetched::Listed { link, .. }, None) => (self.reconcile(turn, Mode::ReadOnly, Scope::Full, Commit::Swap { link, listing: false }, cancel).await?, 0),
            (Fetched::Changes { changes, link }, None) => {
                let count = changes.len();
                if count > 0 || full_requested {
                    self.on_store(turn, move |s| {
                        s.begin_staging(konedrive_tree::NewTree::Delta)?;
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
                        self.on_store(turn, move |s| s.set_delta_link(&link)).await?;
                        Reconciled::default()
                    }
                    Some(scope) => self.reconcile(turn, Mode::ReadOnly, scope, Commit::Swap { link, listing: false }, cancel).await?,
                };
                (reconciled, count)
            }
        };
        if reconciled.applied.counts.deferred > 0 {
            // Files being filled or freed up right now: a Changed scope would
            // never look at them again.
            self.request_full();
        }
        let Reconciled { applied, full } = reconciled;
        if listed || full || full_requested || changes > 0 {
            self.publish_counts(turn).await?;
        }
        // `LastChecked`: this cycle succeeded. Kept in the store,
        // so a restart still knows when the folder was last in step.
        let now = activity::unix_now();
        self.on_store(turn, move |s| s.set_last_checked(now)).await?;
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
        let moved_pins = (applied.counts.moved > 0 || applied.counts.deleted > 0) && self.ctx.pins.count() > 0;
        if full || resweep || moved_pins {
            self.ctx.pins.sweep(self.ctx.root.path.clone()).await;
        } else if !applied.pinned.is_empty() {
            let placed = applied.pinned.iter().map(|rel| self.ctx.root.path.join(rel)).collect();
            self.ctx.pins.queue_under(placed).await;
        }
        // Paused (`docs/design/writes.md` §11): no replacement starts; the next cycle after
        // the pause is Full, and finds them again.
        let paused = self.ctx.running.stopped(&self.ctx.store);
        if paused && !applied.pending.replacements.is_empty() {
            self.request_full();
        } else {
            self.spawn_replacements(applied.pending.replacements.clone());
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
        let stored = self.on_store(turn, |s| s.drive_id()).await?;
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
            self.on_store(turn, move |s| s.set_drive_id(&recorded)).await?;
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

    async fn publish_counts(&self, turn: &Turn) -> Result<(), CycleError> {
        let counts = self.on_store(turn, |s| s.counts()).await?;
        self.ctx.state.update(|s| {
            s.items_listed = counts.listed;
            s.items_placed = counts.placed;
            s.skipped_count = counts.skipped;
        });
        Ok(())
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

#[cfg(test)]
mod tests;
