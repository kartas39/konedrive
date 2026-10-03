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
pub mod outbox_api;
pub mod pins;
pub mod populate;
pub mod queries;
pub mod registration;
pub mod resume;
pub mod run_settings;
pub mod start_stop;
pub mod watching;
pub mod write_mode;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::status::activity::Report;
use crate::desktop::baloo::Baloo;
use crate::helper::{Clearance, HelperLink};
use crate::helper::status::HelperUnit;
use crate::folder::root::DehydrateError;
use crate::folder::root::{RegisterError, SyncRoot};
use crate::hydration::source::ContentSource;
use crate::config::{ConfigStore, Mode};
use crate::account::state::StateHandle;
use crate::folder::locks::InodeLocks;
use crate::status::snapshot::{SyncSnapshot, SyncStateHandle, published_error, published_state};
use crate::conditions::running;
use crate::hydration::pin;
use crate::hydration::source;
use crate::local;
use crate::remote::listing;
use crate::upload;
use crate::upload::kept_back;

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

    fn parse(value: &str) -> Self {
        if value == "onedrive" {
            RootSource::OneDrive
        } else {
            RootSource::Local
        }
    }
}

/// Where a OneDrive folder's own files live.
#[derive(Debug, Clone)]
pub struct SyncPaths {
    pub tree_db: PathBuf,
    pub rescue_dir: PathBuf,
    /// The freedesktop thumbnail cache. `None` runs no thumbnail
    /// filler at all: the VM suite's real-account run, which must not fetch
    /// a thumbnail of every image in the drive.
    pub thumbnails: Option<PathBuf>,
}

/// Where a folder is recorded so that it survives a restart: its account's
/// `[accounts.root]` in `config.toml`, written only through the daemon's one
/// [`ConfigStore`].
#[derive(Clone)]
pub struct Persist {
    pub store: Arc<ConfigStore>,
    /// The account's id.
    pub account: String,
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
    /// The link to the helper, its state and the per-inode locks, shared by
    /// every account of the daemon (design §2.1).
    hub: Arc<hub::HelperHub>,
    /// The hub's link cell: replaceable, because the helper can go away and
    /// come back — [`hub::supervise`] swaps it for `None` the moment the
    /// connection drops and back to a live link when it reconnects. Shared
    /// with a OneDrive folder's sync, which reads it at every reconcile.
    link: crate::helper::LinkCell,
    /// The account interface's own state, on the same object path. §3.1
    /// refuses `RegisterRoot` when nobody is signed in, and
    /// this is what it asks. `None` only where nothing wired it up.
    account: Option<StateHandle>,
    /// The account's one quota (`crate::account::quota`), which the outbox's space check reads and
    /// adjusts ([`set_quota`](Self::set_quota)): until one is set, the quota kept in
    /// `account`'s state, or one of its own without an account.
    quota: Mutex<crate::account::quota::Quota>,
    /// Where the registered root is persisted, so it survives a restart
    /// (§3.1): the account's entry in `config.toml`. `None` disables
    /// persistence entirely.
    persist: Option<Persist>,
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
    /// A read-only Graph client, for a folder that shows OneDrive. `None`
    /// until `main` sets it; without it every folder is local.
    drive: Mutex<Option<konedrive_graph::drive::DriveClient>>,
    sync_paths: Mutex<Option<SyncPaths>>,
    schedule: Mutex<listing::Schedule>,
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
    /// Keeps KDE's Baloo indexer out of a fresh OneDrive folder, and lets a
    /// forgotten one back in (`desktop::baloo`). Starts as
    /// [`Baloo::disabled`], which runs no program at all — only `main`
    /// installs the real `balooctl6`; a test that forgets `set_baloo` must
    /// never reach the user's own indexer settings.
    baloo: Mutex<Arc<Baloo>>,
    /// Why this account's folder is held back (design §3.1: `config.toml`
    /// gives it what an earlier account has), if it is: it is not brought
    /// up, and no registration is made.
    held: Mutex<Option<String>>,
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
    /// The one timer that ends a timed pause on the bus (`outbox_api`).
    pause_timer: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// How many times the pause was shown: the timer ends it on the bus only
    /// if no `Pause` or `Resume` came after it read the store (the outbox on the bus).
    pause_shown: std::sync::atomic::AtomicU64,
    /// `NotUploadedSummary()` as the outbox worker last summed it (issue #38):
    /// answered from memory while the worker runs.
    kept_back: Mutex<Option<Vec<kept_back::SummaryRow>>>,
    /// The account was switched to read-write, and the watcher that follows has not started
    /// yet: its Full local scan says so (`LocalScan.Reason`).
    switched_to_read_write: std::sync::atomic::AtomicBool,
    /// Works the account's mode out again when the write gate closes under the outbox worker
    /// ([`set_mode_check`](Self::set_mode_check)). None in tests.
    mode_check: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Told the drive the account's token reaches when a cycle finds it is not the folder's:
    /// the account's own, set where the account is wired up.
    drive_seen: Mutex<Option<listing::DriveSeen>>,
    /// The account's transfer pool (`konedrive_graph::pool`): every download, upload and change of
    /// an item takes a slot of it. The drive set with [`set_drive`](Self::set_drive) reports
    /// into it.
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
    watcher: Option<write_mode::Watcher>,
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
    /// kept with it for good.
    source: RootSource,
    /// Registered and recovered ([`SyncService::commit`]), so that a OneDrive
    /// folder's sync may run. False for a root only held until its helper is
    /// back ([`SyncService::hold`]), and for one kept after a registration
    /// that failed ([`SyncService::abandon`]).
    brought_up: bool,
    /// Whether *this daemon* excluded the root from Baloo, so
    /// [`unregister_root`](SyncService::unregister_root) knows whether to
    /// take that exclusion back off. Always false outside
    /// [`SyncService::commit`]: `hold` and `abandon`'s kept-registered branch
    /// construct a `Registration` before `commit` has run, so nothing has
    /// been added to Baloo yet either.
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
            baloo_excluded,
            upgrade_when_helper,
        }
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
    /// `account` gates `RegisterRoot` on somebody being signed in (§3.1);
    /// `persist` is where the registered root is persisted so it survives a
    /// restart. Both are `None` in tests that exercise neither. The service
    /// has a hub of its own, holding `link`: the one account of a daemon.
    pub fn new(
        link: Option<HelperLink>,
        account: Option<StateHandle>,
        persist: Option<Persist>,
    ) -> Arc<Self> {
        Self::on_hub(&hub::HelperHub::with_link(link), account, persist)
    }

    /// One account's folder, on the daemon's `hub`, after every account the
    /// hub has already.
    pub fn on_hub(hub: &Arc<hub::HelperHub>, account: Option<StateHandle>, persist: Option<Persist>) -> Arc<Self> {
        hub.join(|helper_state| {
            let state = SyncStateHandle::new(SyncSnapshot { helper_state, ..SyncSnapshot::default() });
            let pool = konedrive_graph::pool::TransferPool::new(konedrive_graph::pool::DEFAULT_CEILING);
            let shown = state.clone();
            pool.set_observer(Arc::new(move |throughput| shown.set_throughput(throughput)));
            // The pins' downloads go through this very service, which they must
            // not keep alive: a weak reference.
            Arc::new_cyclic(|me: &std::sync::Weak<Self>| Self {
                pins: pin::Pins::new(state.clone(), me.clone(), Arc::clone(&pool)),
                pool,
                parts: source::Share::new(),
                hub: Arc::clone(hub),
                link: hub.link_cell(),
                quota: Mutex::new(match &account {
                    Some(account) => crate::account::quota::Quota::new(account.clone(), None),
                    None => crate::account::quota::Quota::detached(),
                }),
                account,
                ignore: outbox_api::configured_ignore(persist.as_ref()),
                running: Arc::new(running::Running::new(
                    persist.as_ref().and_then(|p| p.store.account(&p.account)).map(|a| running::Settings::of(&a)).unwrap_or_default(),
                )),
                pause_timer: Mutex::new(None),
                pause_shown: std::sync::atomic::AtomicU64::new(0),
                kept_back: Mutex::new(None),
                switched_to_read_write: std::sync::atomic::AtomicBool::new(false),
                mode_check: Mutex::new(None),
                persist,
                report: Report::new(state.clone()),
                state,
                root: Mutex::new(None),
                lifecycle: Arc::new(tokio::sync::RwLock::new(())),
                source: Mutex::new(None),
                locks: hub.locks(),
                drive: Mutex::new(None),
                sync_paths: Mutex::new(None),
                schedule: Mutex::new(listing::Schedule::default()),
                syncing: Arc::new(Mutex::new(None)),
                store: Mutex::new(None),
                baloo: Mutex::new(Arc::new(Baloo::disabled())),
                held: Mutex::new(None),
                mode: Mutex::new(Mode::ReadOnly),
                me: me.clone(),
                tree_lock: Arc::new(tokio::sync::Mutex::new(())),
                drive_seen: Mutex::new(None),
            })
        })
    }

    /// The account's quota, which the uploads' space check reads and adjusts: the one
    /// `Account` serves (`AccountService::quota`).
    pub fn set_quota(&self, quota: crate::account::quota::Quota) {
        *self.quota.lock().unwrap() = quota;
    }

    /// The account's quota.
    pub fn quota(&self) -> crate::account::quota::Quota {
        self.quota.lock().unwrap().clone()
    }

    /// The link to the helper this account shares with the daemon's others.
    pub fn hub(&self) -> &Arc<hub::HelperHub> {
        &self.hub
    }

    /// What `HelperState` asks while there is no link (HS1), for the hub.
    pub fn set_helper_unit(&self, unit: Arc<dyn HelperUnit>) {
        self.hub.set_unit(unit);
    }

    /// `HelperState` (HS1), the hub's.
    pub fn helper_state(&self) -> String {
        self.hub.state().as_str().to_owned()
    }

    /// Works the hub's `HelperState` out again ([`hub::HelperHub::check`]).
    pub async fn check_helper(&self) {
        self.hub.check().await;
    }

    /// The drive a folder registered while signed in shows.
    /// Without one, every folder is local.
    pub fn set_drive(&self, drive: konedrive_graph::drive::DriveClient) {
        *self.drive.lock().unwrap() = Some(drive.with_pool(Arc::clone(&self.pool)));
    }

    /// The account's transfer pool.
    pub fn pool(&self) -> &Arc<konedrive_graph::pool::TransferPool> {
        &self.pool
    }

    /// The emergency ceiling of the account's transfer pool (`[transfers] max`) and its
    /// large-file limit (`[transfers] large`).
    pub fn set_transfer_limits(&self, ceiling: usize, large: usize) {
        self.pool.set_limits(ceiling, large);
    }

    /// What a cycle tells when the account's token reaches another drive than the folder's:
    /// the account records it and works its mode out again.
    pub fn set_drive_seen(&self, seen: listing::DriveSeen) {
        *self.drive_seen.lock().unwrap() = Some(seen);
    }

    /// What this folder's cycles ask of, or tell, the rest of the daemon.
    fn neighbours(&self) -> listing::Neighbours {
        let me = self.me.clone();
        listing::Neighbours {
            claimed: self.claims(),
            drive_seen: Arc::new(move |drive| {
                let seen = me.upgrade().and_then(|service| service.drive_seen.lock().unwrap().clone());
                if let Some(seen) = seen {
                    seen(drive);
                }
            }),
        }
    }

    /// What keeps a fresh OneDrive folder out of KDE's Baloo indexer.
    /// Without this call it is [`Baloo::disabled`], which runs no
    /// program at all: `main` installs [`Baloo::default`] (`balooctl6`);
    /// tests point this at a fake so the real indexer settings are never
    /// touched.
    pub fn set_baloo(&self, baloo: Baloo) {
        *self.baloo.lock().unwrap() = Arc::new(baloo);
    }

    /// Where a OneDrive folder's tree store, rescues and thumbnails go.
    /// Without them, every folder is local.
    pub fn set_sync_paths(&self, paths: SyncPaths) {
        *self.sync_paths.lock().unwrap() = Some(paths);
    }

    /// Puts `source` in place of the folder's content source — the VM suite's
    /// real-account scenarios wrap the Graph source to record and
    /// break fetches. Nothing in the daemon calls it.
    pub fn replace_content_source(&self, source: Arc<dyn ContentSource>) {
        *self.source.lock().unwrap() = Some(source);
    }

    /// How often a OneDrive folder is synced; takes effect at the next start
    /// of its sync.
    pub fn set_schedule(&self, schedule: listing::Schedule) {
        *self.schedule.lock().unwrap() = schedule;
    }

    /// Where the helper's socket is, for the hub. Defaults to
    /// `konedrive_proto::SOCKET_PATH`.
    pub fn set_helper_socket(&self, path: impl Into<PathBuf>) {
        self.hub.set_socket(path);
    }

    /// What a punch goes by when nothing ties it to a link of its own
    /// (local rule, on [`Clearance`]): the live link if there
    /// is one, the helper's socket if not.
    fn clearance(&self) -> Clearance {
        self.hub.clearance()
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

    /// Publishes a new helper link, or its loss — and so
    /// `HelperState` (HS1): `connected` at once, or, on a loss, `unknown`
    /// until [`watch_helper`] has asked systemd.
    pub fn set_link(&self, link: Option<HelperLink>) {
        self.hub.set_link(link);
    }

    /// The drive `config.toml` records for this account, if it records one.
    fn account_drive(&self) -> Option<String> {
        let persist = self.persist.as_ref()?;
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

/// Keeps `service`'s helper link alive for the life of the daemon: its hub's
/// [`hub::supervise`], which brings up every account on the hub.
pub async fn supervise_helper(service: Arc<SyncService>, socket_path: PathBuf, backoff: Duration) {
    let hub = Arc::clone(service.hub());
    drop(service);
    hub::supervise(hub, socket_path, backoff).await
}

/// The longest [`hub::supervise`] ever waits between attempts.
pub const MAX_HELPER_BACKOFF: Duration = Duration::from_secs(30);

/// Keeps the `HelperState` of `service`'s hub current ([`hub::watch`]).
pub async fn watch_helper(service: Arc<SyncService>) {
    watch_helper_every(service, crate::helper::status::RECHECK).await
}

/// [`watch_helper`], asking systemd again every `every` while there is no
/// link (tests: well under a second).
pub async fn watch_helper_every(service: Arc<SyncService>, every: Duration) {
    let hub = Arc::clone(service.hub());
    drop(service);
    hub::watch_every(hub, every).await
}

#[cfg(test)]
pub(crate) mod tests;
