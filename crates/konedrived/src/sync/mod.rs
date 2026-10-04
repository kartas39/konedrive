//! Everything this sub-project adds to the daemon: the helper link, the
//! content source, the hydration loop, and `SyncService` — the `org.konedrive.Folder`
//! D-Bus surface's own half of the work (`dbus/folder.rs` is the thin zbus wrapper
//! around it, the same split `crate::account`/`crate::dbus` uses for
//! `Account`). There is one `SyncService` per account; the list of them and the per-inode
//! locks are the daemon's, in `registry.rs`, and so are the helper link and its supervisor,
//! in `helper::hub`.

pub mod bring_up;
mod folder;
pub mod free_up;
pub mod hydrate;
pub mod mode;
pub mod move_outs;
pub mod outbox;
pub mod pause;
mod persisted;
pub mod pins;
pub mod populate;
mod publish;
pub mod queries;
pub mod registry;
mod running_sync;
pub mod settings;
pub mod start_stop;
pub mod take_down;
#[cfg(any(test, feature = "fault-injection"))]
pub mod testing;
pub mod watcher;
pub mod wiring;

use std::sync::Arc;

use tokio::sync::watch;

use crate::status::report::Report;
use crate::helper::{Clearance, HelperLink};
use crate::folder::root::OpenError;
use crate::hydration::dehydrate::DehydrateError;
use crate::folder::root::{RegisterError, SyncRoot};
use crate::folder::locks::InodeLocks;
use crate::status::snapshot::{FolderStatus, SyncSnapshot, SyncStateHandle, published_error, published_state};
use crate::conditions::running;
use crate::hydration::pin;
use crate::hydration::source;
use crate::local;
use crate::remote::listing;

use folder::{Folder, Record, View};
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
    /// The helper refused, or did not answer; the message says what it was asked.
    #[error("{0}")]
    Helper(String),
    /// `config.toml` could not be read or written.
    #[error("{0}")]
    Config(String),
    /// The folder's tree store could not be read or written.
    #[error("{0}")]
    Store(String),
    /// A folder is recorded and not up — it waits for the helper, a registration or a
    /// bring-up failed, `config.toml` does not say what it shows — or its sync has not
    /// started: the call needs a folder whose sync runs. The whole sentence, with why
    /// ([`SyncError::not_up`]).
    #[error("{0}")]
    NotUp(String),
    /// The account is held back (`config.toml` gives it what an earlier account has): why.
    #[error("{0}")]
    HeldBack(String),
    /// `Accounts.Remove` is taking the account away.
    #[error("this account is being removed")]
    Removing,
    /// The daemon is stopping, and gave up the work the call needed.
    #[error("the daemon is stopping")]
    Stopping,
    /// A file of the folder could not be read or changed, or a task of the daemon's own
    /// failed.
    #[error("{0}")]
    Io(String),
}

impl SyncError {
    /// The refusal of a call that needs a folder that is up, with its sync running: `why`
    /// it is not.
    pub(crate) fn not_up(why: &str) -> Self {
        SyncError::NotUp(format!("the folder is not up: {why}"))
    }
}

impl From<RegisterError> for SyncError {
    fn from(e: RegisterError) -> Self {
        match e {
            RegisterError::NotADirectory => SyncError::Unsupported("not a directory".into()),
            RegisterError::NotEmpty => SyncError::NotEmpty,
            // One name on the bus for both, as before the two were told apart.
            RegisterError::Unsupported(why) | RegisterError::Io(why) => SyncError::Unsupported(why),
            RegisterError::Helper(why) => SyncError::Helper(why.to_string()),
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
            DehydrateError::HelperNotConnected => SyncError::NoHelper,
            DehydrateError::Open(e) => e.into(),
            DehydrateError::Io(why) => SyncError::Io(why),
        }
    }
}

impl From<OpenError> for SyncError {
    fn from(e: OpenError) -> Self {
        match e {
            OpenError::OutsideRoot => SyncError::OutsideRoot,
            OpenError::NotManaged => SyncError::NotManaged,
            OpenError::Io(why) => SyncError::Io(why),
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
/// same per-inode lock `serve_hydrations` takes. `serve_hydrations` itself is started once,
/// at daemon startup, before any root exists: the registry's router answers each open with the
/// source the file's folder has at that moment (`registry::filler`).
pub struct SyncService {
    /// What the service was made with: the registry, the account, its entry in `config.toml`,
    /// the drive, and the rest of [`Wiring`]. Never changed.
    wiring: Wiring,
    /// The hub's link cell: replaceable, because the helper can go away and
    /// come back — `helper::hub::supervise` swaps it for `None` the moment the
    /// connection drops and back to a live link when it reconnects. Shared
    /// with a OneDrive folder's sync, which reads it at every reconcile.
    link: crate::helper::LinkCell,
    state: SyncStateHandle,
    /// What the folder is ([`Folder`]), inside the lock that guards it: the only async
    /// lock of the service.
    ///
    /// Taken for writing only by [`change`](Self::change), by everything that changes
    /// which folder is registered, how, in which mode, or whether the account takes one —
    /// for the whole of the change, with the folder's sync stopped. Taken for reading by
    /// what decides from the folder what to ask of the helper and then acts on it
    /// (`dehydrate`, `populate_from_directory`), by what reads the tree store outside the
    /// sync (`skipped`, `pending_uploads`, the registry's router), and by a reconcile, through
    /// the lease its listing is given, while it changes the folder.
    ///
    /// zbus runs every method call in a task of its own, so without it two
    /// registrations both passed the "no root yet" check before either had
    /// committed, both reached the helper, and the last commit won — leaving
    /// the helper holding a root the daemon did not.
    folder: Arc<tokio::sync::RwLock<Folder>>,
    /// The folder as last published ([`publish`]), for the readers that only look and the
    /// synchronous callers. Written by nothing but `publish`.
    view: watch::Sender<View>,
    /// The registry's lock table: one inode belongs to one account only.
    locks: InodeLocks,
    /// The parts of syncs that were told to stop and are not waited for yet
    /// ([`running_sync`]): [`change`](Self::change) waits for them.
    ended: running_sync::Ended,
    /// The activity log, the conflicts, the downloads under way and the
    /// folder's space, shared with the hydration loop and a
    /// OneDrive folder's sync. Its store is a clone of the running sync's, attached as
    /// the sync starts and detached by [`change`](Self::change), after which no write
    /// holds it.
    report: Report,
    /// "Always keep on this device": the pins and the downloads they ask
    /// for. Shared with a OneDrive folder's sync, which queues what it places
    /// under a pin and sweeps after every Full reconcile.
    pins: Arc<pin::Pins>,
    /// This service, for the watcher's status hook, which may have to stop
    /// the sync from the watcher's thread (the folder moved or deleted).
    me: std::sync::Weak<SyncService>,
    /// The account's ignore list (`docs/design/writes.md` §4.4), from `config.toml`: the
    /// watcher's examination reads it, `SetIgnorePatterns` changes it.
    ignore: local::SharedIgnore,
    /// The pause as it is shown, and the timer that ends a timed one (`pause`).
    clock: pause::PauseClock,
    /// The account's transfer pool (`konedrive_graph::pool`): every download, upload and change of
    /// an item takes a slot of it. The drive of the wiring reports into it.
    pool: Arc<konedrive_graph::pool::TransferPool>,
    /// The large pinned files downloading in parts, and how many streams each has: who is
    /// due the next free large slot of `pool` (`source::parts`, issue #28).
    parts: Arc<source::Share>,
    /// What background work runs now (`running`): the one place every reader of the pause
    /// asks, with the account's settings from `config.toml`.
    running: Arc<running::Running>,
    /// The task that counts the queue totals into the published state
    /// (`status::totals`): the service's own, from its making to its drop, whether or not
    /// anything shows the account.
    totals: tokio::task::AbortHandle,
}

/// What `LastError` says while a root is registered without interception.
/// Spelled out rather than hinted at: this mode's whole risk is that a file
/// looks present and reads as zeros, so the one thing a user must not have
/// to infer is that they are in it.
pub const NO_INTERCEPTION_WARNING: &str =
    "this folder is registered WITHOUT interception: nothing fills a placeholder when it is \
     opened, so files in this folder read as zeros until they are explicitly hydrated";

impl SyncService {
    /// One account's folder, made with `wiring`, on a tokio runtime: its totals are counted
    /// from now on. It is not one of the daemon's accounts until whoever made it adds it
    /// to the registry ([`registry::Registry::add`]).
    pub fn new(mut wiring: Wiring) -> Arc<Self> {
        let registry = Arc::clone(&wiring.registry);
        let hub = Arc::clone(registry.hub());
        let helper_state = hub.state();
        let state = SyncStateHandle::new(SyncSnapshot { folder: FolderStatus { helper_state, ..FolderStatus::default() }, ..SyncSnapshot::default() });
        let pool = konedrive_graph::pool::TransferPool::new(konedrive_graph::pool::DEFAULT_CEILING);
        let shown = state.clone();
        pool.set_observer(Arc::new(move |throughput| shown.set_throughput(throughput)));
        pool.set_limits(wiring.transfers.ceiling, wiring.transfers.large);
        // The drive reports into the account's pool.
        if let Some(onedrive) = wiring.onedrive.take() {
            wiring.onedrive = Some(OneDrive { drive: onedrive.drive.with_pool(Arc::clone(&pool)), paths: onedrive.paths });
        }
        let settings = wiring.persist.store.account(&wiring.persist.account).map(|a| running::Settings::of(&a)).unwrap_or_default();
        let report = Report::new(state.clone());
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
            totals: tokio::spawn(crate::status::totals::run(state.clone(), report.transfers.clone())).abort_handle(),
            report,
            state,
            folder: Arc::new(tokio::sync::RwLock::new(Folder::new())),
            view: watch::Sender::new(View::default()),
            locks: registry.locks(),
            ended: running_sync::Ended::default(),
            me: me.clone(),
            wiring,
        })
    }

    /// The account this is the folder of.
    pub fn id(&self) -> &crate::config::AccountId {
        &self.wiring.persist.account
    }

    /// The daemon's accounts, as their folders see each other.
    pub fn registry(&self) -> &Arc<registry::Registry> {
        &self.wiring.registry
    }

    /// The account's quota, which the uploads' space check reads and adjusts: the one
    /// `Account` serves.
    pub fn quota(&self) -> crate::account::quota::Quota {
        self.wiring.account.quota()
    }

    /// The link to the helper this account shares with the daemon's others.
    pub fn hub(&self) -> &Arc<crate::helper::hub::HelperHub> {
        self.wiring.registry.hub()
    }

    /// `HelperState` (HS1), the hub's.
    pub fn helper_state(&self) -> String {
        self.hub().state().as_str().to_owned()
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
        self.hub().clearance()
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
        self.link.get()
    }

    /// The drive `config.toml` records for this account, if it records one.
    fn account_drive(&self) -> Option<String> {
        let persist = &self.wiring.persist;
        persist.store.account(&persist.account).and_then(|a| a.drive_id).map(|drive| drive.into_string())
    }

    /// The device the folder is on, as it was when its record was made, for the registry's
    /// router; `None` with no folder, or one that could not be looked at.
    fn root_device(&self) -> Option<u64> {
        self.record().and_then(|record| record.dev)
    }

    /// Whether this account has a folder the router cannot place: one whose device is
    /// unknown, or one that `config.toml` records and the daemon does not act on — an
    /// account held back, or a registration under way, which writes its folder down
    /// first. An open in such a folder could be taken for another account's by device
    /// alone.
    fn has_unplaced_folder(&self) -> bool {
        match self.record() {
            Some(record) => record.dev.is_none(),
            None => self.persisted_root().is_some(),
        }
    }

    /// Every folder this account holds or records: the one it acts on, and
    /// the one `config.toml` names (held back, or being registered).
    pub(super) fn folders(&self) -> Vec<std::path::PathBuf> {
        let mut folders: Vec<std::path::PathBuf> = self.record().map(|record| record.root.path).into_iter().collect();
        folders.extend(self.persisted_root().map(|p| p.path));
        folders
    }

    fn require_link(&self) -> Result<HelperLink, SyncError> {
        self.link().ok_or(SyncError::NoHelper)
    }

    pub fn root(&self) -> Option<SyncRoot> {
        self.record().map(|r| r.root)
    }

    /// `RootState` as published ([`published_state`]).
    pub fn root_state(&self) -> String {
        published_state(&self.state.get()).to_owned()
    }

    /// `LastError` as published ([`published_error`]).
    pub fn last_error(&self) -> String {
        published_error(&self.state.get())
    }

    /// Where the folder's files are filled from now, as last published: the drive for a
    /// OneDrive folder that is up, the directory a local folder was populated from.
    pub(crate) fn content_source(&self) -> Option<Arc<dyn crate::hydration::source::ContentSource>> {
        self.view.borrow().source.clone()
    }

    /// `Writable`: whether what is changed in the folder is uploaded now. False for a
    /// folder whose account is read-write and which runs locked all the same — its
    /// watcher could not start, or did not finish walking it; `LastError` says which.
    pub fn writable(&self) -> bool {
        self.state.get().folder.writable
    }

    /// `RootSource`.
    pub fn root_source(&self) -> String {
        self.record().map(|r| r.source.as_str().to_owned()).unwrap_or_default()
    }

    /// HS2: a folder that shows OneDrive is kept in step only when it is
    /// intercepted and the helper is connected.
    fn require_helper_for(&self, record: &Record) -> Result<(), SyncError> {
        if record.intercepted() && self.link().is_some() {
            Ok(())
        } else {
            Err(SyncError::NoHelper)
        }
    }

    fn require_onedrive(&self) -> Result<Record, SyncError> {
        let record = self.require_record()?;
        if record.source != RootSource::OneDrive {
            return Err(SyncError::Unsupported("this folder is not connected to OneDrive".into()));
        }
        Ok(record)
    }

    /// Why a call that needs the folder's sync cannot have it, for a OneDrive folder
    /// whose sync does not run: the folder is not up, or its sync could not start, or has
    /// not started.
    fn sync_not_running(&self) -> SyncError {
        let view = self.view();
        if let Some(why) = view.down {
            return SyncError::not_up(&why);
        }
        match view.sync {
            folder::SyncView::Stopped(Some(why)) => SyncError::NotUp(format!("the folder's sync is not running: {why}")),
            _ => SyncError::NotUp("the folder's sync has not started yet".into()),
        }
    }
}

impl Drop for SyncService {
    /// The view lets go of the running sync's handles: a part of that sync reads the view
    /// (the write gate), so left there they would keep each other, and the tree store,
    /// alive after the service. The sync itself goes with the folder's state, which tells
    /// its parts to stop. The totals' task holds the published state, and would count for
    /// nobody.
    fn drop(&mut self) {
        self.totals.abort();
        self.view.send_replace(View::default());
    }
}

#[cfg(test)]
pub(crate) mod tests;
