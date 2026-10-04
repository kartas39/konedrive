//! Everything this sub-project adds to the daemon: the helper link, the
//! content source, the hydration loop, and `SyncService` — the `org.konedrive.Folder`
//! D-Bus surface's own half of the work (`dbus/folder.rs` is the thin zbus wrapper
//! around it, the same split `crate::account`/`crate::dbus` uses for
//! `Account`). There is one `SyncService` per account; the helper link, its
//! supervisor and the per-inode locks are the daemon's, in `hub.rs`.

pub mod forget;
pub mod free_up;
pub mod hub;
pub mod hydrate;
pub mod move_outs;
pub mod outbox;
pub mod pause;
pub mod pins;
pub mod populate;
pub mod queries;
pub mod registration;
pub mod resume;
pub mod settings;
pub mod start_stop;
#[cfg(any(test, feature = "fault-injection"))]
pub mod testing;
pub mod watcher;
pub mod wiring;
pub mod write_mode;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::status::activity::Report;
use crate::helper::{Clearance, HelperLink};
use crate::folder::root::DehydrateError;
use crate::folder::root::{RegisterError, SyncRoot};
use crate::hydration::source::ContentSource;
use crate::config::Mode;
use crate::folder::locks::InodeLocks;
use crate::status::snapshot::{SyncSnapshot, SyncStateHandle, published_error, published_state};
use crate::conditions::running;
use crate::hydration::pin;
use crate::hydration::source;
use crate::local;
use crate::remote::listing;
use crate::upload;
use crate::upload::kept_back;

pub use wiring::{OneDrive, Persist, SyncPaths, Transfers, Wiring};

// --- the folder's interfaces' own half of the work ------------------------
//
// `dbus/folder.rs` is the thin zbus wrapper (the same split `crate::account` /
// `crate::dbus` uses for `Account`); everything that actually does
// something lives here, so it can be exercised without a bus at all.

/// What a registered folder shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSource {
    /// Filled from a directory with `PopulateFromDirectory`, as in part 1.
    Local,
    /// Listed from the signed-in drive, locked, and kept in step with it.
    OneDrive,
}

impl RootSource {
    fn as_str(self) -> &'static str {
        match self {
            RootSource::Local => "local",
            RootSource::OneDrive => "onedrive",
        }
    }

    /// One of the two words `config.toml` has for it; `None` for anything else, which
    /// is never taken for either (quality finding `SY6`).
    fn parse(value: &str) -> Option<Self> {
        match value {
            "onedrive" => Some(RootSource::OneDrive),
            "local" => Some(RootSource::Local),
            _ => None,
        }
    }
}

/// Everything the folder can refuse, flattened from `RegisterError` and
/// `DehydrateError` plus the failures that only exist at this layer (no
/// root registered, no helper connected, a path outside the root).
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("the folder must be empty")]
    NotEmpty,
    #[error("{0}")]
    Unsupported(String),
    #[error("the file is in use")]
    InUse,
    #[error("no sync root is registered")]
    NoRoot,
    #[error("the konedrive helper is not connected")]
    NoHelper,
    #[error("not a OneDrive file")]
    NotManaged,
    #[error("the file is not downloaded")]
    NotHydrated,
    #[error("the file was modified locally")]
    ModifiedLocally,
    #[error("not a plain file inside this sync root")]
    OutsideRoot,
    /// A folder that remembers another account's drive (design §8.3);
    /// `Folder` answers it `NotEmpty`.
    #[error("this folder holds another OneDrive account's files; choose an empty folder")]
    ForeignFolder,
    /// A folder that is, is inside, or contains another account's folder
    /// (design §8.3); the other account's label.
    #[error("this folder is, is inside, or contains the folder of the account '{0}'")]
    Overlaps(String),
    #[error("a sync root is already registered; forget it first")]
    AlreadyRegistered,
    #[error("nobody is signed in")]
    NotSignedIn,
    #[error("no content source is registered; call PopulateFromDirectory first")]
    NoSource,
    #[error("no conflict is recorded for {0}")]
    NoConflict(String),
    /// A free-up of something a pin keeps on this device: the message is
    /// [`pin::refusal`]'s, naming the path refused and what pins it.
    #[error("{0}")]
    NotAllowed(String),
    /// A free-up of a file with a change waiting to be uploaded (write design
    /// §3.8): the message names it.
    #[error("{0} is not uploaded yet, so freeing it up would lose the changes made here")]
    NotUploaded(String),
    /// `WebUrl` of a file or folder that carries no item id: OneDrive does not
    /// have it yet, so it has no page there. `Files` answers it `NotUploaded`.
    #[error("{0} is not uploaded yet, so it has no page in OneDrive")]
    NotInOneDrive(String),
    /// OneDrive did not answer (`WebUrl`): no network, Graph kept refusing, the
    /// secret storage is locked, or the answer could not be read. The message
    /// is that cause alone; the clients put their own sentence in front of it.
    #[error("{0}")]
    Unreachable(String),
    /// An argument no value of which makes sense (`SetIgnorePatterns`).
    #[error("{0}")]
    InvalidArgs(String),
    /// A Forget, or `Accounts.Remove`, while changes wait to be uploaded:
    /// the tree store holding them would go. The message says how many, and what to do.
    #[error("{0}")]
    PendingUploads(String),
    #[error("{0}")]
    Io(String),
}

impl From<RegisterError> for SyncError {
    fn from(e: RegisterError) -> Self {
        match e {
            RegisterError::NotADirectory => SyncError::Unsupported("not a directory".into()),
            RegisterError::NotEmpty => SyncError::NotEmpty,
            RegisterError::Unsupported(why) => SyncError::Unsupported(why),
            RegisterError::Helper(why) => SyncError::Io(why),
        }
    }
}

impl From<DehydrateError> for SyncError {
    fn from(e: DehydrateError) -> Self {
        match e {
            DehydrateError::NotManaged => SyncError::NotManaged,
            DehydrateError::NotHydrated => SyncError::NotHydrated,
            DehydrateError::ModifiedLocally => SyncError::ModifiedLocally,
            DehydrateError::InUse => SyncError::InUse,
            DehydrateError::OutsideRoot => SyncError::OutsideRoot,
            DehydrateError::HelperNotConnected => SyncError::NoHelper,
            DehydrateError::Io(why) => SyncError::Io(why),
        }
    }
}

/// One account's folder: registration, the manual `PopulateFromDirectory`
/// fill, and per-file hydrate/dehydrate/state — what that account's
/// `org.konedrive.Folder` and its sibling interfaces expose, and what `org.konedrive.Files` routes to it.
///
/// # Why `hydrate_now` fills directly rather than only through interception
///
/// The original design describes a helper-connected `Hydrate()`
/// as opening the file and letting the kernel's `FAN_OPEN_PERM` interception
/// carry the request to `serve_hydrations`, the same path a real
/// application's `open()` takes — which is the right design *when a real
/// privileged helper has actually marked the root* (production, or a
/// `vng` test). It is unreachable in an unprivileged `cargo test`: nothing
/// unprivileged can hold `CAP_SYS_ADMIN`, so no fanotify group is ever
/// installed, `MarkDir`/`MarkFile` acks from the fake helper this crate's
/// own D-Bus tests use are just protocol replies, and an `open()` under
/// such a "marked" directory is not intercepted at all — it returns
/// immediately with whatever the placeholder already holds, i.e. nothing.
/// Relying on interception here would make `Hydrate()` either silently
/// serve zeros (an open that succeeds without being filled) or hang forever
/// in a real deployment that starts this method before the helper has
/// finished marking — both worse than what this does instead: hydrate_now
/// always fills the file itself, synchronously, through the same
/// `ContentSource`/`source::hydrate` the interception path uses, under the
/// same per-inode lock `serve_hydrations` takes. `SyncService` doubles as
/// that `ContentSource` (see the `ContentSource` impl below) precisely so
/// `serve_hydrations` can be started once at daemon startup, before any
/// root exists, and pick up whatever gets registered later.
pub struct SyncService {
    /// What the service was made with: the hub, the account, its entry in `config.toml`,
    /// the drive, and the rest of [`Wiring`]. Never changed.
    wiring: Wiring,
    /// The hub's link cell: replaceable, because the helper can go away and
    /// come back — [`hub::supervise`] swaps it for `None` the moment the
    /// connection drops and back to a live link when it reconnects. Shared
    /// with a OneDrive folder's sync, which reads it at every reconcile.
    link: crate::helper::LinkCell,
    state: SyncStateHandle,
    root: Mutex<Option<Registration>>,
    /// Taken for writing by everything that changes which root is registered
    /// or how — `register_root`, `register_root_without_interception`,
    /// `unregister_root`, `resume` — for the whole of the change, and for
    /// reading by what decides from the root's mode what to ask of the
    /// helper and then acts on it: `dehydrate` and `populate_from_directory`.
    ///
    /// zbus runs every method call in a task of its own, so without it two
    /// registrations both passed the "no root yet" check before either had
    /// committed, both reached the helper, and the last commit won — leaving
    /// the helper holding a root the daemon did not: a folder still marked,
    /// which a later registration without interception of that folder would
    /// hold with nothing intercepting opens in it.
    ///
    /// A OneDrive folder's sync shares this very lock (`ListingContext::
    /// lifecycle`): a reconcile holds it for reading while it changes the
    /// folder, so no registration changes under it.
    lifecycle: Arc<tokio::sync::RwLock<()>>,
    source: Mutex<Option<Arc<dyn ContentSource>>>,
    /// The hub's lock table: one inode belongs to one account only.
    locks: InodeLocks,
    /// The running sync of a OneDrive folder. Shared with the task that
    /// nudges it when the account signs in ([`nudge_on_sign_in`]). Started
    /// and stopped only under `lifecycle` held for writing — except the stop
    /// a Forget makes before it takes that lock (see `unregister_root`).
    syncing: Arc<Mutex<Option<Syncing>>>,
    /// Its tree store, for `Skipped()`.
    ///
    /// The store's files are removed (`remove_tree_store`) only with
    /// `lifecycle` held for writing and the sync stopped, and nothing may be
    /// reading them then. So no clone of the store outlives
    /// [`stop_sync`](SyncService::stop_sync): the sync's own go when it
    /// returns (`Poller::stop` waits for every task that holds one). Any
    /// other clone is taken, and dropped, with `lifecycle` held for reading
    /// (`skipped`).
    store: Mutex<Option<konedrive_tree::Store>>,
    /// Why this account's folder is held back (design §3.1: `config.toml`
    /// gives it what an earlier account has), if it is: it is not brought
    /// up, and no registration is made.
    held: Mutex<Option<String>>,
    /// `Accounts.Remove` is taking this account away: nothing is registered
    /// or brought up for it. Set by [`retire`](Self::retire) and taken back
    /// by [`unretire`](Self::unretire), both with `lifecycle` held for
    /// writing. Apart from `held`, so that a removal that fails gives a
    /// held-back account its own reason back.
    retiring: std::sync::atomic::AtomicBool,
    /// The activity log, the conflicts, the downloads under way and the
    /// folder's space, shared with the hydration loop and a
    /// OneDrive folder's sync. Its store is a clone of `store`'s, attached by
    /// [`start_sync`](Self::start_sync) and detached by
    /// [`stop_sync`](Self::stop_sync), after which no write holds it.
    report: Report,
    /// "Always keep on this device": the pins and the downloads they ask
    /// for. Shared with a OneDrive folder's sync, which queues what it places
    /// under a pin and sweeps after every Full reconcile.
    pins: Arc<pin::Pins>,
    /// The account's mode as the folder follows it (`docs/design/writes.md` §2, §2.2):
    /// read-only keeps a OneDrive folder under the lock, read-write lifts it.
    /// Changed only by [`write_mode`]'s switch, with `lifecycle` held for
    /// writing and the sync stopped, so a running sync never sees it change.
    mode: Mutex<Mode>,
    /// This service, for the watcher's status hook, which may have to stop
    /// the sync from the watcher's thread (the folder moved or deleted).
    me: std::sync::Weak<SyncService>,
    /// The per-root tree lock (`docs/design/writes.md` §9): the outbox worker holds it
    /// across each commit that touches `items`, and a cycle must hold it from
    /// staging to swap, or the swap reverts the commit.
    tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// The account's ignore list (`docs/design/writes.md` §4.4), from `config.toml`: the
    /// watcher's examination reads it, `SetIgnorePatterns` changes it.
    ignore: local::ignore::SharedIgnore,
    /// The pause as it is shown, and the timer that ends a timed one (`pause`).
    clock: pause::PauseClock,
    /// `NotUploadedSummary()` as the outbox worker last summed it (issue #38):
    /// answered from memory while the worker runs.
    kept_back: Mutex<Option<Vec<kept_back::SummaryRow>>>,
    /// The account was switched to read-write, and the watcher that follows has not started
    /// yet: its Full local scan says so (`LocalScan.Reason`).
    switched_to_read_write: std::sync::atomic::AtomicBool,
    /// The account's transfer pool (`konedrive_graph::pool`): every download, upload and change of
    /// an item takes a slot of it. The drive of the wiring reports into it.
    pool: Arc<konedrive_graph::pool::TransferPool>,
    /// The large pinned files downloading in parts, and how many streams each has: who is
    /// due the next free large slot of `pool` (`source::parts`, issue #28).
    parts: Arc<source::Share>,
    /// What background work runs now (`running`): the one place every reader of the pause
    /// asks, with the account's settings from `config.toml`.
    running: Arc<running::Running>,
}

/// A OneDrive folder's sync while it runs.
struct Syncing {
    poller: listing::Poller,
    /// [`nudge_on_sign_in`], stopped with the poller.
    sign_in_watch: Option<tokio::task::JoinHandle<()>>,
    /// The thumbnail filler and the token that stops it: started
    /// and stopped with the poller, so a Forget leaves no clone of the tree
    /// store with it either. `None` when [`SyncPaths::thumbnails`] is.
    thumbnails: Option<(tokio::task::JoinHandle<()>, CancellationToken)>,
    /// A read-write folder's watcher: started in the same critical
    /// section that publishes this `Syncing`, and stopped by whoever takes it,
    /// so it lives exactly as long as the sync. `None` for a
    /// read-only folder.
    watcher: Option<local::watcher::Watcher>,
    /// A read-write folder's outbox worker, which sends the rows the
    /// watcher's examination records: started and stopped with the watcher,
    /// in the same places. `None` for a read-only folder.
    outbox: Option<upload::OutboxWorker>,
}

/// A registered root and how — or whether — opens inside it are intercepted.
#[derive(Clone)]
struct Registration {
    root: SyncRoot,
    /// False only for a root registered through
    /// `RegisterWithoutInterception`.
    intercepted: bool,
    /// Whether its last recovery left interrupted files as found because a
    /// helper was running that this daemon had no link to, so
    /// the next link runs it again ([`SyncService::resume`]).
    recovery_deferred: bool,
    /// What it shows, decided when it was first registered and
    /// kept with it for good — unless it is only a guess (`source_guessed`).
    source: RootSource,
    /// `source` is the guess made for a folder held with a `source` that `config.toml`
    /// does not say in either of its two words ([`Persisted::source_as_written`]): good
    /// for a Forget, never for a bring-up, which reads `config.toml` again
    /// ([`SyncService::source_brought_back`]). False for every folder that is up.
    source_guessed: bool,
    /// Registered and recovered ([`SyncService::commit`]), so that a OneDrive
    /// folder's sync may run. False for a root only held until its helper is
    /// back ([`SyncService::hold`]), and for one kept after a registration
    /// that failed ([`SyncService::abandon`]).
    brought_up: bool,
    /// Whether *this daemon* excluded the root from Baloo, so
    /// [`unregister_root`](SyncService::unregister_root) knows whether to
    /// take that exclusion back off. Decided by [`SyncService::commit`] for a
    /// folder that is up. Before that, [`SyncService::hold`] carries what
    /// `config.toml` records, for the Forget of a folder that stays held; a
    /// registration kept after it failed (`abandon`, a failed switch) says
    /// false, since nothing was added to Baloo for it.
    baloo_excluded: bool,
    /// Registered without interception only because no helper was connected
    ///, so it switches to interception when one connects
    /// ([`SyncService::upgrade`]). False for every intercepted root, and for
    /// one registered without interception on purpose — with a helper
    /// connected.
    upgrade_when_helper: bool,
    /// The device the folder is on, read once when the registration is made,
    /// for the hub's router: never a path looked at per request. `None` when
    /// the folder could not be looked at then.
    dev: Option<u64>,
}

/// A root as `config.toml` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Persisted {
    path: PathBuf,
    /// Empty in a config written before the id was recorded.
    root_id: String,
    intercepted: bool,
    source: RootSource,
    /// What `config.toml` has for `source` when it is neither of its two words (a hand
    /// edit: `"OneDrive"`). Such a folder is never brought up
    /// ([`unread_source`](Persisted::unread_source)); `source` then reads `OneDrive`, for
    /// a Forget alone, which so takes off everything a OneDrive folder may carry. Written
    /// back as it was read, never as a guess.
    source_as_written: Option<String>,
    /// Whether this daemon is the one that excluded the root from Baloo
    ///; `false` in a config written before this existed.
    baloo_excluded: bool,
    /// [`Registration::upgrade_when_helper`].
    upgrade_when_helper: bool,
}

impl Persisted {
    fn of(
        root: &SyncRoot,
        intercepted: bool,
        source: RootSource,
        baloo_excluded: bool,
        upgrade_when_helper: bool,
    ) -> Self {
        Self {
            path: root.path.clone(),
            root_id: root.root_id.clone(),
            intercepted,
            source,
            source_as_written: None,
            baloo_excluded,
            upgrade_when_helper,
        }
    }

    fn read(root: crate::config::RootConfig) -> Self {
        let upgrade_when_helper = root.upgrades_when_helper();
        let source = RootSource::parse(&root.source);
        Self {
            path: root.path,
            root_id: root.id,
            intercepted: root.intercepted,
            // Unreadable: held for a Forget as a OneDrive folder, and never brought up.
            source: source.unwrap_or(RootSource::OneDrive),
            source_as_written: source.is_none().then_some(root.source),
            baloo_excluded: root.baloo_excluded,
            upgrade_when_helper,
        }
    }

    /// Why this folder is not brought up, when `config.toml` does not say what it shows.
    fn unread_source(&self) -> Option<String> {
        let written = self.source_as_written.as_ref()?;
        Some(format!(
            "config.toml has source = {written:?} for it, which is neither \"onedrive\" nor \"local\"; \
             correct it and start konedrive again, or forget the folder and add it again"
        ))
    }
}

/// What `LastError` says while a root is registered without interception.
/// Spelled out rather than hinted at: this mode's whole risk is that a file
/// looks present and reads as zeros, so the one thing a user must not have
/// to infer is that they are in it.
pub const NO_INTERCEPTION_WARNING: &str =
    "this folder is registered WITHOUT interception: nothing fills a placeholder when it is \
     opened, so files in this folder read as zeros until they are explicitly hydrated";

impl SyncService {
    /// One account's folder, made with `wiring` and joined to its hub after every account
    /// the hub has already.
    pub fn new(mut wiring: Wiring) -> Arc<Self> {
        let hub = Arc::clone(&wiring.hub);
        hub.join(|helper_state| {
            let state = SyncStateHandle::new(SyncSnapshot { helper_state, ..SyncSnapshot::default() });
            let pool = konedrive_graph::pool::TransferPool::new(konedrive_graph::pool::DEFAULT_CEILING);
            let shown = state.clone();
            pool.set_observer(Arc::new(move |throughput| shown.set_throughput(throughput)));
            pool.set_limits(wiring.transfers.ceiling, wiring.transfers.large);
            // The drive reports into the account's pool.
            if let Some(onedrive) = wiring.onedrive.take() {
                wiring.onedrive = Some(OneDrive { drive: onedrive.drive.with_pool(Arc::clone(&pool)), paths: onedrive.paths });
            }
            let settings = wiring.persist.store.account(&wiring.persist.account).map(|a| running::Settings::of(&a)).unwrap_or_default();
            // The pins' downloads go through this very service, which they must
            // not keep alive: a weak reference.
            Arc::new_cyclic(|me: &std::sync::Weak<Self>| Self {
                pins: pin::Pins::new(state.clone(), me.clone(), Arc::clone(&pool)),
                pool,
                parts: source::Share::new(),
                link: hub.link_cell(),
                ignore: settings::configured_ignore(&wiring.persist),
                running: Arc::new(running::Running::new(settings, Arc::clone(&wiring.clock))),
                clock: pause::PauseClock::new(Arc::clone(&wiring.clock), {
                    // The timed pause has run out: shown again, as the store has it now.
                    let me = me.clone();
                    move || {
                        if let Some(service) = me.upgrade() {
                            service.show_pause();
                        }
                    }
                }),
                kept_back: Mutex::new(None),
                switched_to_read_write: std::sync::atomic::AtomicBool::new(false),
                report: Report::new(state.clone()),
                state,
                root: Mutex::new(None),
                lifecycle: Arc::new(tokio::sync::RwLock::new(())),
                source: Mutex::new(None),
                locks: hub.locks(),
                syncing: Arc::new(Mutex::new(None)),
                store: Mutex::new(None),
                held: Mutex::new(None),
                retiring: std::sync::atomic::AtomicBool::new(false),
                mode: Mutex::new(Mode::ReadOnly),
                me: me.clone(),
                tree_lock: Arc::new(tokio::sync::Mutex::new(())),
                wiring,
            })
        })
    }

    /// The account's quota, which the uploads' space check reads and adjusts: the one
    /// `Account` serves.
    pub fn quota(&self) -> crate::account::quota::Quota {
        self.wiring.account.quota()
    }

    /// The link to the helper this account shares with the daemon's others.
    pub fn hub(&self) -> &Arc<hub::HelperHub> {
        &self.wiring.hub
    }

    /// `HelperState` (HS1), the hub's.
    pub fn helper_state(&self) -> String {
        self.wiring.hub.state().as_str().to_owned()
    }

    /// The drive a folder registered while signed in shows; without one, every folder is
    /// local.
    fn drive(&self) -> Option<&konedrive_graph::drive::DriveClient> {
        self.wiring.onedrive.as_ref().map(|onedrive| &onedrive.drive)
    }

    /// Where a OneDrive folder's tree store, rescues and thumbnails go.
    fn sync_paths(&self) -> Option<&SyncPaths> {
        self.wiring.onedrive.as_ref().map(|onedrive| &onedrive.paths)
    }

    /// The account's transfer pool.
    pub fn pool(&self) -> &Arc<konedrive_graph::pool::TransferPool> {
        &self.pool
    }

    /// What this folder's cycles ask of, or tell, the rest of the daemon: a drive that is
    /// not the folder's is told to the account, which records it and works its mode out
    /// again.
    fn neighbours(&self) -> listing::Neighbours {
        let account = Arc::clone(&self.wiring.account);
        listing::Neighbours { claimed: self.claims(), drive_seen: Arc::new(move |drive| account.drive_seen(drive)) }
    }

    /// What a punch goes by when nothing ties it to a link of its own
    /// (local rule, on [`Clearance`]): the live link if there
    /// is one, the helper's socket if not.
    fn clearance(&self) -> Clearance {
        self.wiring.hub.clearance()
    }

    pub fn state(&self) -> &SyncStateHandle {
        &self.state
    }

    /// Where every download, free-up and reconcile reports to.
    pub fn report(&self) -> &Report {
        &self.report
    }

    /// The lock table `serve_hydrations` must share with this service, so
    /// that a real interception-driven hydration and a `dehydrate()` (or a
    /// direct `hydrate_now()`) of the same inode can never run at once.
    pub fn locks(&self) -> InodeLocks {
        self.locks.clone()
    }

    /// The live helper link, if there is one right now.
    pub fn link(&self) -> Option<HelperLink> {
        self.link.lock().unwrap().clone()
    }

    /// The drive `config.toml` records for this account, if it records one.
    fn account_drive(&self) -> Option<String> {
        let persist = &self.wiring.persist;
        persist.store.account(&persist.account).map(|a| a.drive_id).filter(|drive| !drive.is_empty())
    }

    /// The device the registered folder is on, as it was when it was
    /// registered, for the hub's router; `None` with no folder, or one that
    /// could not be looked at.
    fn root_device(&self) -> Option<u64> {
        self.registration().and_then(|reg| reg.dev)
    }

    /// Whether this account has a folder the router cannot place: one that is
    /// held back, recorded but not registered yet (a registration under way
    /// writes its folder down first), or whose device is unknown. An open in
    /// such a folder could be taken for another account's by device alone.
    fn has_unplaced_folder(&self) -> bool {
        match self.registration() {
            Some(reg) => reg.dev.is_none(),
            None => self.persisted_root().is_some(),
        }
    }

    fn registration(&self) -> Option<Registration> {
        self.root.lock().unwrap().clone()
    }

    fn require_registration(&self) -> Result<Registration, SyncError> {
        self.registration().ok_or(SyncError::NoRoot)
    }

    fn require_link(&self) -> Result<HelperLink, SyncError> {
        self.link().ok_or(SyncError::NoHelper)
    }

    pub fn root(&self) -> Option<SyncRoot> {
        self.registration().map(|r| r.root)
    }

    /// `RootState` as published ([`published_state`]).
    pub fn root_state(&self) -> String {
        published_state(&self.state.get()).to_owned()
    }

    /// `LastError` as published ([`published_error`]).
    pub fn last_error(&self) -> String {
        published_error(&self.state.get())
    }

    /// `RootSource`.
    pub fn root_source(&self) -> String {
        self.registration().map(|r| r.source.as_str().to_owned()).unwrap_or_default()
    }

    /// HS2: a folder that shows OneDrive is kept in step only when it is
    /// intercepted and the helper is connected.
    fn require_helper_for(&self, reg: &Registration) -> Result<(), SyncError> {
        if reg.intercepted && self.link().is_some() {
            Ok(())
        } else {
            Err(SyncError::NoHelper)
        }
    }

    fn require_onedrive(&self) -> Result<Registration, SyncError> {
        let reg = self.require_registration()?;
        if reg.source != RootSource::OneDrive {
            return Err(SyncError::Unsupported("this folder is not connected to OneDrive".into()));
        }
        Ok(reg)
    }
}

/// The longest [`hub::supervise`] ever waits between attempts.
pub const MAX_HELPER_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(test)]
pub(crate) mod tests;
