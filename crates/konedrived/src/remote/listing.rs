//! One folder's sync with OneDrive: the delta
//! feed read into the tree store's `staging`, the folder made to match it, and
//! only then `staging` swapped in with the new delta link. A crash before the
//! swap leaves the old link, and the next cycle asks for the same changes.
//!
//! The first cycle of every `Listing`, and every cycle after one that failed
//! (or whose future was dropped), has a reconcile whatever its delta holds:
//! the folder's mount is told a cycle was committed, and downloads again what
//! its pins keep.
//!
//! Cycles of one `Listing` never overlap. A cycle asks Graph without the
//! folder's lock (`SyncService`'s `folder`, held through the lease) and takes it only to change the folder and swap the link in,
//! so a change of the folder is never kept waiting by a listing. A stop
//! (`Poller::stop`) never waits for Graph or for whoever holds that lock; it
//! waits only for a reconcile already changing the folder, which checks for the
//! stop between steps.
//!
//! A folder's first listing is shown page by page: each page is
//! committed into `onedrive_items` as it comes, with the link to the next page, so
//! that the folder fills as the drive is listed and a listing stopped
//! part-way resumes where it stopped. See [`Listing::list_placing`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{Notify, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;

use crate::status::report::Report;
use super::applied::Applied;
use crate::folder::root::SyncRoot;
use crate::status::snapshot::{SyncStateHandle, SyncTrouble, TroubleKind};
use konedrive_graph::drive::{DriveClient, DriveError};
use konedrive_tree::{Store, TreeError, TreeStore};

mod lease;
pub use lease::Lease;
/// The reconcile of one cycle, in either mode: the folder, the commit, what follows.
pub(crate) mod reconcile;
/// A read-write folder's cycle (`docs/design/writes.md` §9).
mod rw;
pub use rw::Writes;
use reconcile::Reconciled;
/// What OneDrive said, staged: the reconcile it asks for.
pub(crate) mod stage;
use stage::Fetched;
use crate::remote::mode::Mode;

/// What OneDrive lists: a whole listing, one placed page by page, a delta's changes.
mod fetch;
/// The task that runs a cycle on a schedule, on a refresh and when the live socket says so.
mod poller;
pub use poller::{PollHandle, Poller, Schedule};


/// The account's drive, as `config.toml` keeps it (`docs/design/accounts.md` §6.1): the
/// same-account check then survives a tree store rebuilt empty, whose `meta`
/// has forgotten it.
#[derive(Clone)]
pub struct DriveRecord {
    pub store: Arc<crate::config::ConfigStore>,
    /// The account's id.
    pub account: crate::config::AccountId,
    /// What `config.toml` recorded when the sync started; `None` when
    /// nothing was, and the first cycle writes it.
    pub recorded: Option<String>,
}

pub struct ListingContext {
    pub root: SyncRoot,
    pub store: Store,
    pub drive: DriveClient,
    /// `None` where nothing keeps a record (tests): the store's `meta` alone
    /// says which drive the folder shows.
    pub drive_record: Option<DriveRecord>,
    /// The folder's mount (`crate::fuse`): a cycle makes the folder under it follow the new
    /// tree, and tells it once the tree is committed.
    pub mount: Arc<crate::fuse::Mount>,
    pub state: SyncStateHandle,
    /// The folder's lease (`sync/`'s): a reconcile holds it, so a Forget waits
    /// for one that is changing the folder.
    pub lease: Lease,
    /// Nudged at the end of every successful cycle, so the thumbnail filler
    /// runs right after there is something new to make thumbnails
    /// of, rather than waiting out its own idle timer.
    pub after_cycle: Option<Arc<Notify>>,
    /// Where the cycle's activity, conflicts and downloads are reported, and
    /// the folder's space measured again: `SyncService`'s.
    pub report: Report,
    /// The folder's mode (`docs/design/writes.md` §2.2). Read-only: the
    /// account is, and the folder's mount takes no change. Read-write: with
    /// the cycle's part in uploading. A switch of mode stops the sync and starts a new one, so
    /// this never changes under a running sync.
    pub mode: Mode<Writes>,
    /// The folder's tree lock, which the mount's write-operation thread runs each job under
    /// (`crate::fuse`): a reconcile holds it while it changes the folder and commits, in
    /// either mode. A read-write folder's cycle takes it earlier, at its staging
    /// ([`Writes::tree_lock`], the same lock); a read-only folder's takes it here, for the
    /// reconcile alone.
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// The daemon's other parts a cycle asks or tells; `None` in tests.
    pub neighbours: Option<Neighbours>,
    /// What background work runs now (`conditions::running`): the poll stops while the
    /// account's work does.
    pub running: Arc<crate::conditions::running::Running>,
}

/// What a cycle asks of, or tells, the rest of the daemon.
#[derive(Clone)]
pub struct Neighbours {
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
    /// (`docs/design/accounts.md` §6.1); that account's label. Two folders of one drive would
    /// download everything twice.
    #[error(
        "this account is signed in to the Microsoft account already connected as '{0}'; sign it \
         out, or remove one of the two"
    )]
    DriveTaken(String),
    #[error("cannot reach OneDrive ({0}); trying again")]
    Offline(String),
    /// A failure of the tree store, in its own words ("the tree store: …"), that ends
    /// the cycle and stops the folder (quality finding `RE6`). Not every store failure ends a cycle: those the
    /// read-write reconcile only logs and passes over after its commit do not come here.
    #[error("{0}")]
    Store(String),
    #[error("the folder could not be brought up to date: {0}")]
    Apply(String),
    #[error("cancelled")]
    Cancelled,
}

impl CycleError {
    /// Trouble that stops the folder until someone acts.
    pub fn blocking(&self) -> bool {
        matches!(
            self,
            CycleError::SignedOut | CycleError::OtherAccount(_) | CycleError::DriveTaken(_) | CycleError::Store(_)
        )
    }
}

impl CycleError {
    /// The kind of trouble this is for the account's overall state: only
    /// [`Offline`](CycleError::Offline) is OneDrive out of reach, whatever its sentence says.
    pub fn kind(&self) -> TroubleKind {
        match self {
            CycleError::Offline(_) => TroubleKind::Unreachable,
            _ => TroubleKind::Other,
        }
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

/// What one cycle did.
#[derive(Debug, Default)]
pub struct CycleReport {
    /// Entries in the delta; 0 for a full listing.
    pub changes: usize,
    pub applied: Applied,
}

/// A cycle's turn: while any clone of it lives — in the cycle, or in a
/// blocking task the cycle started — no other cycle of the same `Listing`
/// runs, even when this cycle's future has been dropped.
pub(crate) type Turn = Arc<OwnedMutexGuard<()>>;

/// Whether the next cycle has a reconcile whatever its delta holds. Asked for through
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
    /// Asked for from the start, for a `Listing`'s first cycle.
    full: FullRequest,
    /// The drive has been written into `config.toml`, or is being:
    /// once per `Listing`.
    drive_recorded: AtomicBool,
    /// The drive to write there, by the next reconcile.
    pending_drive: std::sync::Mutex<Option<(DriveRecord, String)>>,
    /// Whose turn it is (see [`Turn`]).
    turns: Arc<tokio::sync::Mutex<()>>,
    /// Read-write mode: the outbox commit count the last cycle's fetch
    /// started at; items the outbox committed after it are looked at again
    /// by the next cycle.
    revisit_from: std::sync::atomic::AtomicI64,
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
            full: FullRequest(AtomicBool::new(true)),
            drive_recorded: AtomicBool::new(false),
            pending_drive: std::sync::Mutex::new(None),
            turns: Arc::new(tokio::sync::Mutex::new(())),
            revisit_from: std::sync::atomic::AtomicI64::new(0),
        })
    }

    /// Asks for the next cycle to have a reconcile whatever its delta holds. The one way
    /// to ask.
    fn request_full(&self) {
        self.full.request();
    }

    /// One cycle, and what came of it published.
    ///
    /// Cycles of one `Listing` run one at a time (a `refresh` and the
    /// poller's own, say). One that fails, or whose future is dropped
    /// part-way, leaves the next one a reconcile whatever its delta holds.
    pub async fn cycle(self: &Arc<Self>, cancel: &CancellationToken) -> Result<CycleReport, CycleError> {
        let result = self.take_turn(cancel).await;
        let was_stopped = self.ctx.state.get().cycle.sync_trouble.is_some_and(|t| t.blocking);
        self.publish_outcome(&result);
        // The trouble that closed the write gate is gone only now, after the cycle's own
        // word to the outbox (`Writes::cycled`): the worker is told again. Whatever the
        // cycle came to: one that failed with trouble that is only said opens the gate too.
        let is_stopped = self.ctx.state.get().cycle.sync_trouble.is_some_and(|t| t.blocking);
        if let Some(writes) = self.ctx.mode.read_write().filter(|_| was_stopped && !is_stopped) {
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
        // Taken, not read: a request made while this cycle runs must outlive it.
        let full_requested = self.full.take();
        // Unless this cycle succeeds, the next one is asked for too — also
        // when its future is dropped part-way.
        let mut unless_done = OnDrop(Some(|| self.request_full()));
        let result = self.sync_once(&turn, full_requested, cancel).await;
        if result.is_ok() {
            unless_done.disarm();
        }
        result
    }

    /// One cycle, in three steps: what OneDrive says is fetched
    /// ([`Self::fetch`]), staged and reconciled ([`Self::reconcile_fetched`]),
    /// and what a cycle that went through leaves to do is done
    /// ([`Self::after_cycle`]). The mode is told once, before the fetch.
    async fn sync_once(self: &Arc<Self>, turn: &Turn, full_requested: bool, cancel: &CancellationToken) -> Result<CycleReport, CycleError> {
        self.check_account(turn, cancel).await?;
        let mode = self.begin(turn).await?;
        let fetched = self.fetch(turn, mode, cancel).await?;
        // Whether this cycle may have changed the tree: its counts are
        // published then, and not in an idle cycle.
        let listed = !matches!(fetched, Fetched::News(stage::News::Changes { .. }));
        let (reconciled, changes) = self.reconcile_fetched(turn, mode, fetched, full_requested, cancel).await?;
        self.after_cycle(turn, reconciled, changes, listed || full_requested).await
    }

    /// What a cycle that went through leaves to do: the counts and the
    /// time published, the conflicts looked over, and the outbox told. `changed`:
    /// the cycle may have changed the tree whatever its delta held.
    async fn after_cycle(self: &Arc<Self>, turn: &Turn, reconciled: Reconciled, changes: usize, changed: bool) -> Result<CycleReport, CycleError> {
        let Reconciled { applied } = reconciled;
        if changed || changes > 0 {
            self.publish_counts(turn).await?;
        }
        // `LastChecked`: this cycle succeeded. Kept in the store,
        // so a restart still knows when the folder was last in step.
        let now = crate::clock::unix_now();
        self.on_store(turn, move |s| s.set_last_checked(now)).await?;
        self.ctx.state.update(|s| s.cycle.last_checked = now);
        // A conflict whose rescued file is gone drops off by itself (`docs/design/sync.md`
        // §10.3), whether or not anyone asks for the list: a batch of them
        // looked over each cycle. Not through `on_store`: the
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
        // "Always keep on this device": the mount keeps the pins, and downloaded what they
        // keep when the cycle was committed.
        if let Some(writes) = self.ctx.mode.read_write() {
            // The base caught up: the outbox sends (`docs/design/writes.md` §9).
            (writes.cycled)();
        }
        Ok(CycleReport { changes, applied })
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
    /// keeps for the account; once known, it is recorded in both.
    async fn check_account(&self, turn: &Turn, cancel: &CancellationToken) -> Result<(), CycleError> {
        let id = cancellable(cancel, self.ctx.drive.drive_id()).await?.map_err(drive_error)?;
        let stored = self.on_store(turn, |s| s.drive_id()).await?;
        let kept = self.ctx.drive_record.as_ref().and_then(|r| r.recorded.clone());
        // Compared as drive ids: one drive is the same however its id is spelled.
        let same_drive = |recorded: &String| crate::config::DriveId::new(recorded.as_str()) == crate::config::DriveId::new(id.as_str());
        if let Some(recorded) = stored.clone().or(kept.clone()).filter(|recorded| !same_drive(recorded)) {
            // The account learns which drive its token reaches now.
            if let Some(neighbours) = &self.ctx.neighbours {
                (neighbours.drive_seen)(&id);
            }
            return Err(CycleError::OtherAccount(recorded));
        }
        // A drive is one account (`docs/design/accounts.md` §6.1): one another account has recorded is not
        // listed a second time into this folder.
        if let Some(record) = self.ctx.drive_record.as_ref().filter(|_| kept.is_none()) {
            let config = record.store.snapshot();
            if let Some(other) = config.accounts.iter().find(|a| a.id != record.account && a.drive_id.as_ref().is_some_and(|drive| *drive == id)) {
                return Err(CycleError::DriveTaken(other.label.clone()));
            }
        }
        if stored.is_none() {
            let recorded = id.clone();
            self.on_store(turn, move |s| s.set_drive_id(&recorded)).await?;
        }
        if let Some(record) = self.ctx.drive_record.as_ref().filter(|_| kept.is_none()) {
            if !self.drive_recorded.swap(true, Ordering::SeqCst) {
                // Written by the next reconcile, which holds the folder's
                // lock anyway: the Graph phase never waits for it.
                *crate::panic::lock(&self.pending_drive) = Some((record.clone(), id));
            }
        }
        Ok(())
    }

    async fn publish_counts(&self, turn: &Turn) -> Result<(), CycleError> {
        let counts = self.on_store(turn, |s| s.counts()).await?;
        self.ctx.state.update(|s| {
            s.cycle.items_listed = counts.listed;
            s.cycle.items_placed = counts.placed;
            s.cycle.skipped_count = counts.skipped;
        });
        Ok(())
    }

    fn publish_outcome(&self, result: &Result<CycleReport, CycleError>) {
        self.ctx.state.update(|s| match result {
            Ok(_) => s.cycle.sync_trouble = None,
            Err(CycleError::Cancelled) => {}
            Err(e) => s.cycle.sync_trouble = Some(SyncTrouble { text: e.to_string(), blocking: e.blocking(), kind: e.kind() }),
        });
    }
}
