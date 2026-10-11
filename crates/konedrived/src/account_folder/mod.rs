//! `SyncService`: one account's folder — the `org.konedrive.Folder` D-Bus surface's own
//! half of the work (`dbus/folder.rs` is the thin zbus wrapper around it, the same split
//! `crate::account`/`crate::dbus` uses for `Account`). There is one `SyncService` per
//! account; the list of them is the daemon's, in `registry.rs`.

pub mod bring_up;
mod folder;
pub mod free_up;
pub mod hydrate;
pub mod menu;
pub mod mode;
pub mod upload_queue;
pub mod pause;
mod persisted;
pub mod pins;
mod publish;
pub mod queries;
pub mod registry;
pub mod settings;
pub mod start_stop;
pub mod take_down;
#[cfg(any(test, feature = "fault-injection"))]
pub mod testing;
pub mod wiring;
/// Test code: a folder put into a state by hand.
#[cfg(test)]
mod staged_folder;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use tokio::sync::watch;

use crate::status::report::Report;
use crate::folder::root::{RegisterError, SyncRoot};
use crate::status::snapshot::{published_error, published_state, SyncSnapshot, SyncStateHandle};
use crate::conditions::running;
use crate::content_source::source;
use crate::local;
use crate::listing::listing_cycle;

use folder::{Folder, View};
pub use wiring::{OneDrive, Persist, SyncPaths, Transfers, Wiring};

// --- the folder's interfaces' own half of the work ------------------------
//
// `dbus/folder.rs` is the thin zbus wrapper (the same split `crate::account` /
// `crate::dbus` uses for `Account`); everything that actually does
// something lives here, so it can be exercised without a bus at all.

/// Everything the folder can refuse.
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
    #[error("not a OneDrive file")]
    NotManaged,
    #[error("the file is not downloaded")]
    NotHydrated,
    #[error("not a file or folder inside this sync root")]
    OutsideRoot,
    /// A folder that remembers another account's drive (`docs/design/accounts.md` §6.3);
    /// `Folder` answers it `NotEmpty`.
    #[error("this folder holds another OneDrive account's files; choose an empty folder")]
    ForeignFolder,
    /// A folder that is, is inside, or contains another account's folder
    /// (§6.3); the other account's label.
    #[error("this folder is, is inside, or contains the folder of the account '{0}'")]
    Overlaps(String),
    #[error("a sync root is already registered; forget it first")]
    AlreadyRegistered,
    #[error("nobody is signed in")]
    NotSignedIn,
    #[error("no conflict is recorded for {0}")]
    NoConflict(String),
    /// A free-up of something a pin keeps on this device: the message is
    /// [`pins::refusal`]'s, naming the path refused and what pins it.
    #[error("{0}")]
    NotAllowed(String),
    /// A free-up of a file with a change waiting to be uploaded (`docs/design/writes.md`
    /// §11): the message names it.
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
    /// `config.toml` could not be read or written.
    #[error("{0}")]
    Config(String),
    /// The folder's tree store could not be read or written.
    #[error("{0}")]
    Store(String),
    /// A folder is recorded and not up — a bring-up failed, `config.toml` does not say
    /// what it shows — or its sync has not started: the call needs a folder whose sync runs. The whole sentence, with why
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
        }
    }
}

/// One account's folder: its registration, its sync with OneDrive, and what is asked of a
/// file in it — what that account's `org.konedrive.Folder` and its sibling interfaces
/// expose, and what `org.konedrive.Files` routes to it.
///
/// A folder that is up shows OneDrive through its mount (`crate::mount`), which answers every
/// read, download, free-up and pin; the service asks the mount, and keeps what the bus
/// shows of the answers.
pub struct SyncService {
    /// What the service was made with: the registry, the account, its entry in `config.toml`,
    /// the drive, and the rest of [`Wiring`]. Never changed.
    wiring: Wiring,
    state: SyncStateHandle,
    /// What the folder is ([`Folder`]), inside the lock that guards it: the only async
    /// lock of the service.
    ///
    /// Taken for writing only by [`change`](Self::change), by everything that changes
    /// which folder is registered, how, in which mode, or whether the account takes one —
    /// for the whole of the change, with the folder's sync stopped. Taken for reading by
    /// what reads the tree store outside the sync (`skipped`, `count_unsent_to_send`), and by a
    /// reconcile, through the lease its listing is given, while it changes the folder.
    ///
    /// zbus runs every method call in a task of its own, so without it two
    /// registrations both passed the "no root yet" check before either had
    /// committed, and the last commit won.
    folder: Arc<tokio::sync::RwLock<Folder>>,
    /// The folder as last published ([`publish`]), for the readers that only look and the
    /// synchronous callers. Written by nothing but `publish`.
    view: watch::Sender<View>,
    /// The activity log, the conflicts, the downloads under way and the
    /// folder's space, shared with the folder's mount and its sync. Its store is a clone of the folder's, attached by
    /// the start that opens it (`sync_tasks::start_order`) and detached before a take-down or a Forget closes it
    /// (`let_go_of_activity`): it stays attached while a stopped sync keeps its parts.
    report: Report,
    /// The account's ignore list (`docs/design/writes.md` §4.2 rule 3), from `config.toml`: the
    /// folder's mount reads it, `SetIgnorePatterns` changes it.
    ignore: local::SharedIgnore,
    /// The pause as it is shown, and the timer that ends a timed one (`pause`).
    clock: pause::PauseClock,
    /// The account's transfer pool (`konedrive_graph::pool`): every download, upload and change of
    /// an item takes a slot of it. The drive of the wiring reports into it.
    pool: Arc<konedrive_graph::pool::TransferPool>,
    /// The large pinned files downloading in parts, and how many streams each has: who is
    /// due the next free large slot of `pool` (`source::parts`).
    parts: Arc<source::Share>,
    /// What background work runs now (`running`): the one place every reader of the pause
    /// asks, with the account's settings from `config.toml`.
    running: Arc<running::Running>,
    /// The daemon's end reached this account (`end_at_daemon_stop`), set before anything of it is waited for:
    /// no sync of its folder is started any more, whoever asks (`start_sync` reads it).
    is_ended_at_daemon_stop: std::sync::atomic::AtomicBool,
    /// The start or the stop of the folder's sync that runs now, and what it left when its caller was cut
    /// ([`folder::SyncChangeTask`]).
    sync_change: folder::SyncChangeTask,
    /// The service itself: a start gives the mount the report of its take-down, which reaches the service by a
    /// weak reference (`take_down_report`). Weak: it keeps nothing alive.
    me: std::sync::Weak<SyncService>,
    /// The task that counts the queue totals into the published state
    /// (`status::totals`): the service's own, from its making to its drop, whether or not
    /// anything shows the account.
    totals: tokio::task::AbortHandle,
}

impl SyncService {
    /// One account's folder, made with `wiring`, on a tokio runtime: its totals are counted
    /// from now on. It is not one of the daemon's accounts until whoever made it adds it
    /// to the registry ([`registry::Registry::add`]).
    pub fn new(mut wiring: Wiring) -> Arc<Self> {
        let state = SyncStateHandle::new(SyncSnapshot::default());
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
            pool,
            parts: source::Share::new(),
            ignore: settings::configured_ignore(&wiring.persist),
            running: Arc::new(running::Running::new(settings, Arc::clone(&wiring.clock))),
            clock: pause::PauseClock::new(Arc::clone(&wiring.clock), {
                // The timed pause has run out: shown again, as it is now.
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
            is_ended_at_daemon_stop: std::sync::atomic::AtomicBool::new(false),
            sync_change: folder::SyncChangeTask::default(),
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

    /// The folder's account: its sign-in is part of the account's overall state
    /// (`status::overall`).
    pub fn account(&self) -> &Arc<dyn crate::account::FolderAccount> {
        &self.wiring.account
    }

    /// The account's quota, which the uploads' space check reads and adjusts: the one
    /// `Account` serves.
    pub fn quota(&self) -> crate::account::quota::Quota {
        self.wiring.account.quota()
    }

    /// What this folder's cycles ask of, or tell, the rest of the daemon: a drive that is
    /// not the folder's is told to the account, which records it and works its mode out
    /// again.
    fn neighbours(&self) -> listing_cycle::Neighbours {
        let account = Arc::clone(&self.wiring.account);
        listing_cycle::Neighbours { drive_seen: Arc::new(move |drive| account.drive_seen(drive)) }
    }

    /// The drive a folder shows; without one, none can be registered.
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

    pub fn state(&self) -> &SyncStateHandle {
        &self.state
    }

    /// Where every download, free-up and reconcile reports to.
    pub fn report(&self) -> &Report {
        &self.report
    }

    /// The drive `config.toml` records for this account, if it records one.
    fn account_drive(&self) -> Option<String> {
        let persist = &self.wiring.persist;
        persist.store.account(&persist.account).and_then(|a| a.drive_id).map(|drive| drive.into_string())
    }

    /// Every folder this account holds or records: the one it acts on, and
    /// the one `config.toml` names (held back, or being registered).
    pub(super) fn folders(&self) -> Vec<std::path::PathBuf> {
        let mut folders: Vec<std::path::PathBuf> = self.record().map(|record| record.root.path).into_iter().collect();
        folders.extend(self.persisted_root().map(|p| p.path));
        folders
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

    /// The folder's mount, as last published: there from the folder's first sync on.
    pub(crate) fn mount(&self) -> Option<Arc<crate::mount::ItemMount>> {
        self.view.borrow().parts.as_ref().map(|parts| Arc::clone(&parts.mount))
    }

    /// The daemon is stopping: the ONE call of the daemon's end for this account's
    /// folder. Nothing new is sent, the requests in flight are cut or settled, the sync
    /// is stopped, and then the folder loses its mount and is locked, so that it shows
    /// its downloaded files and takes nothing until the daemon is back. Its path, for
    /// the desktop to be told; `None` for an account whose folder is not mounted. The
    /// caller bounds the wait (`crate::daemon::stop`): the work runs in a task of its
    /// own, which a caller that is cut leaves running until the process exits.
    pub async fn end_at_daemon_stop(&self) -> Option<std::path::PathBuf> {
        // Nothing starts this folder from here on, also when this call is cut before it has the folder's lock: a
        // `Refresh()` during the stop's bound, the report of the mount that is being stopped, a change that starts
        // again what ran — the adoption of this call's own stop among them.
        self.is_ended_at_daemon_stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut stopped = self.change().await;
        // A folder a fault took down is not tried again: the daemon ends.
        if let Some(folder::Sync::Stopped(folder::Why::TakenDown { next_try_at, .. })) = stopped.folder_mut().sync_mut() {
            *next_try_at = None;
        }
        let record = stopped.folder_mut().record_mut()?;
        let path = record.root.path.clone();
        let kept = record.kept.parts.take()?;
        // The view lets go of the parts: nothing of the bus reaches a mount that is being stopped.
        stopped.publish();
        // In a task of its own: a caller cut at the daemon's stop bound leaves it running until the process exits.
        if let Err(error) = tokio::spawn(crate::sync_tasks::stop_order::end_kept_folder_at_daemon_stop(kept)).await {
            tracing::warn!("the task ending the folder {} at the daemon's stop failed: {error}", path.display());
        }
        Some(path)
    }

    /// `Writable`: whether the folder takes changes now, and what is changed in it is
    /// uploaded: its mount says ([`AccessModeSwitch::is_read_write`](crate::mount::AccessModeSwitch::is_read_write)).
    pub fn writable(&self) -> bool {
        self.state.get().folder.writable
    }

    /// `RootSource`.
    pub fn root_source(&self) -> String {
        self.record().map(|_| persisted::ONEDRIVE.to_owned()).unwrap_or_default()
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
