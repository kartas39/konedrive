//! What an account's folder is, as a type ([`Folder`]), and the one way to change it
//! ([`SyncService::change`]).
//!
//! The state lives inside the lock that guards it: whoever changes which folder is
//! registered, how, in which mode, or whether the account takes a folder at all holds a
//! [`Stopped`], which is the only way to a `&mut Folder`. A `Stopped` is given out once the
//! folder's sync has stopped, and when it is dropped the state is published
//! ([`publish`](super::publish)): the bus's `Path`, `State`, the registration's part of
//! `LastError`, and the [`View`] every reader that only looks reads.
//!
//! What runs for a OneDrive folder that is up is part of the state too
//! ([`Content::OneDrive`], [`Sync`](super::running_sync::Sync)): its sync is started only by a
//! holder of a `Stopped`, and stopped by [`SyncService::change`], which takes it out of
//! the state. So there is no sync without a folder that is up and shows OneDrive, and no
//! tree store open for a folder that is forgotten.

use std::sync::Arc;

use konedrive_tree::Store;
use tokio::sync::RwLockWriteGuard;

use super::running_sync::{Handles, RunningSync, Sync, Why};
use super::{RootSource, SyncService};
use crate::config::Mode;
use crate::folder::root::SyncRoot;
use crate::hydration::recovery::RecoveryReport;
use crate::hydration::source::ContentSource;

/// An account's folder: whether the account takes one, the mode it is to follow, and what
/// it is now.
pub(super) struct Folder {
    pub standing: Standing,
    /// The account's mode as the folder follows it (`docs/design/writes.md` §2, §2.2):
    /// read-only keeps a OneDrive folder under the lock, read-write lifts it.
    pub wanted: Mode,
    pub is: Is,
}

/// Whether the account takes a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Standing {
    Active,
    /// `config.toml` gives the account what an earlier account has (design §3.1): its
    /// folder is not brought up and no registration is made. The sentence that says why.
    HeldBack(String),
    /// `Accounts.Remove` is taking the account away: nothing is registered or brought up
    /// for it. `held` is the reason it was held back for, which a removal that fails gives
    /// back.
    Retiring { held: Option<String> },
}

/// What the folder is now.
pub(super) enum Is {
    /// The account has no folder.
    Absent,
    /// A folder is recorded and not up: no sync runs, and nothing new is placed in it.
    Down(Record, Down),
    /// Registered and recovered.
    Up(Up),
}

/// A folder as the daemon holds it, up or not: what `config.toml` records of it, and the
/// device it is on.
#[derive(Debug, Clone)]
pub(super) struct Record {
    pub root: SyncRoot,
    pub interception: Interception,
    /// What it shows, decided when it was first registered and kept with it for good.
    /// Under [`Down::UnreadSource`] it is a guess, good for a Forget alone.
    pub source: RootSource,
    /// Whether *this daemon* excluded the folder from Baloo, so that a Forget takes
    /// exactly that exclusion off again.
    pub baloo: bool,
    /// The device the folder is on, read once when the record is made, for the hub's
    /// router: never a path looked at per request. `None` when the folder could not be
    /// looked at then.
    pub dev: Option<u64>,
    /// What the daemon keeps for the folder for as long as it is recorded, up or down.
    pub kept: Kept,
}

/// What is kept with a recorded folder until it is forgotten, its account removed, or
/// another folder registered: it does not go when the folder goes down. So a folder that
/// is down still fills an open the helper intercepts, still says what waits to be
/// uploaded and whether it is paused, and its tree still claims its items for the other
/// accounts.
#[derive(Clone, Default)]
pub(super) struct Kept {
    /// Where the folder's files are filled from: the drive for a OneDrive folder, from
    /// its first bring-up on (`None` when the daemon was given no drive); the directory a
    /// local folder was last populated from (`PopulateFromDirectory`), in this run of the
    /// daemon.
    pub source: Option<Arc<dyn ContentSource>>,
    /// A OneDrive folder's tree store: opened when its sync first starts, or when a change
    /// first needs it, and the one connection to it from then on.
    pub store: Option<Store>,
    /// The per-root tree lock (`docs/design/writes.md` §9): the outbox worker holds it
    /// across each commit that touches `items`, and a cycle must hold it from staging to
    /// swap, or the swap reverts the commit. One for as long as the folder is recorded, so
    /// that what took it before a sync was started again still keeps the new sync's cycle
    /// out.
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kept").field("source", &self.source.is_some()).field("store", &self.store.is_some()).finish()
    }
}

impl Record {
    /// The record as a reader gets it: without what is kept with the folder. A reader may
    /// hold it for as long as a download lasts, which a Forget does not wait for, and must
    /// not keep the folder's tree store open past the Forget with it. What a reader needs
    /// of the kept parts is in the [`View`], beside the record.
    pub fn bare(&self) -> Record {
        Record { kept: Kept::default(), ..self.clone() }
    }

    /// Whether opens inside the folder are intercepted: false only for a folder registered
    /// through `RegisterWithoutInterception`.
    pub fn intercepted(&self) -> bool {
        self.interception == Interception::Intercepted
    }

    /// Whether the folder switches to interception when a helper connects: it was
    /// registered without only because none was connected, or it shows OneDrive, which is
    /// kept in step only with interception (HS2).
    pub fn switches_with_helper(&self) -> bool {
        match self.interception {
            Interception::Intercepted => false,
            Interception::Without { switch_when_helper } => switch_when_helper || self.source == RootSource::OneDrive,
        }
    }

    /// Whether the folder needs the helper to be what it is: it is intercepted, or shows
    /// OneDrive.
    pub fn needs_helper(&self) -> bool {
        self.intercepted() || self.source == RootSource::OneDrive
    }
}

/// How, or whether, opens inside a folder are intercepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Interception {
    Intercepted,
    /// Nothing intercepts opens in it. `switch_when_helper`: it was registered so only
    /// because no helper was connected, and switches when one connects.
    Without { switch_when_helper: bool },
}

/// Why a recorded folder is not up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Down {
    /// Recorded without interception, and not brought up yet in this run of the daemon.
    NotYetUp,
    /// Recorded with interception; it is brought up when the helper connects.
    WaitsForHelper,
    /// A registration, or a switch to interception, failed, and the helper could not
    /// confirm it let go: the helper may still hold the folder, so the daemon keeps it,
    /// intercepted, and brings it up at the next connect. The sentence that says so.
    Kept { why: String },
    /// `config.toml` says what the folder shows in a word that is neither of its two
    /// (`written`): it is never brought up on a guess (`SY6`).
    UnreadSource { written: String },
    /// A bring-up failed, or the folder itself was moved or deleted. The sentence that
    /// says so.
    Failed { why: String },
}

impl Down {
    /// The sentence `LastError` and a `NotUp` refusal say of a folder at `root` that is
    /// down for this; empty while it only waits.
    pub fn why(&self, root: &SyncRoot) -> String {
        match self {
            Down::NotYetUp | Down::WaitsForHelper => String::new(),
            Down::Kept { why } | Down::Failed { why } => why.clone(),
            Down::UnreadSource { written } => format!(
                "cannot bring up the sync folder {}: config.toml has source = {written:?} for it, which is neither \
                 \"onedrive\" nor \"local\"; correct it and start konedrive again, or forget the folder and add it again",
                root.path.display()
            ),
        }
    }
}

impl Down {
    /// What a folder that only waits waits for, for a refusal that has to say why the
    /// folder is not up; `None` when it is down for a reason of its own ([`why`](Self::why)).
    pub fn waits_for(&self) -> Option<&'static str> {
        match self {
            Down::NotYetUp => Some("it is being brought up"),
            Down::WaitsForHelper => Some("it waits for the konedrive helper to connect"),
            Down::Kept { .. } | Down::UnreadSource { .. } | Down::Failed { .. } => None,
        }
    }
}

/// A folder that is up.
pub(super) struct Up {
    pub record: Record,
    pub recovery: Recovery,
    /// Why the last switch to interception did not go through, for a folder that stays
    /// without: said behind the registration's own text until a switch does, or the
    /// folder is brought up again.
    pub switch_failed: Option<String>,
    pub content: Content,
}

/// What a folder that is up holds besides its record.
pub(super) enum Content {
    /// Filled by hand (`PopulateFromDirectory`); nothing runs for it.
    Local,
    OneDrive(OneDriveFolder),
}

/// A folder that is up and shows OneDrive.
pub(super) struct OneDriveFolder {
    pub sync: Sync,
    /// The folder's watcher ended by itself, with nobody stopping it: the sentence. Until
    /// the folder is brought up again or its mode is switched, its sync runs locked, as
    /// one whose watcher could not start, and no other watcher is started.
    pub watcher_ended: Option<String>,
}

/// What the recovery walk of the bring-up left behind (design §4.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Recovery {
    /// Nothing to say.
    Clean,
    /// Interrupted files could not be reset: the folder reads `error`. The sentence.
    Unreset(String),
    /// Part of the folder could not be looked at. The sentence.
    Uninspected(String),
    /// Interrupted files were left as found because a helper runs that this daemon has no
    /// link to: the walk runs again when there is one. The sentence.
    Deferred(String),
}

impl Recovery {
    /// What `report` leaves to say, logged as it is worked out.
    pub fn of(report: &RecoveryReport) -> Self {
        if report.failed > 0 {
            let text = format!(
                "startup recovery could not reset {} of {} managed file(s); they are left \
                 exactly as found for the next start (reset {}, skipped {})",
                report.failed, report.scanned, report.reset, report.skipped
            );
            tracing::error!("{text}");
            Recovery::Unreset(text)
        } else if report.skipped > 0 {
            let text = format!(
                "startup recovery could not inspect {} item(s); the root may not be fully \
                 recovered",
                report.skipped
            );
            tracing::warn!("{text}");
            Recovery::Uninspected(text)
        } else if report.busy > 0 {
            // Not an error: what has such a file open is, as often as not, the very open
            // that will fill it. So it is logged and not said in `LastError` (B-M11): said
            // there, it stayed for the whole session, long after the file was filled.
            tracing::info!(
                "startup recovery left {} interrupted file(s) as they were because they were in \
                 use; each is filled when it is next opened, or reset at the next start",
                report.busy
            );
            Recovery::Clean
        } else if report.deferred > 0 {
            // Not an error either: recovery runs again the moment the link is up.
            let text = format!(
                "startup recovery left {} interrupted file(s) as they were: a konedrive helper \
                 is running and this daemon is not connected to it yet; they are reset once it is",
                report.deferred
            );
            tracing::info!("{text}");
            Recovery::Deferred(text)
        } else {
            Recovery::Clean
        }
    }

    /// The sentence, if there is one.
    pub fn text(&self) -> Option<&str> {
        match self {
            Recovery::Clean => None,
            Recovery::Unreset(text) | Recovery::Uninspected(text) | Recovery::Deferred(text) => Some(text),
        }
    }
}

impl Folder {
    pub fn new() -> Self {
        Self { standing: Standing::Active, wanted: Mode::ReadOnly, is: Is::Absent }
    }

    /// The folder the daemon holds, up or not.
    pub fn record(&self) -> Option<&Record> {
        match &self.is {
            Is::Absent => None,
            Is::Down(record, _) => Some(record),
            Is::Up(up) => Some(&up.record),
        }
    }

    /// The folder this account's calls act on, up or not: none for an account held back,
    /// whose folder nothing acts on but a Forget.
    pub fn acted_on(&self) -> Option<&Record> {
        match self.standing {
            Standing::HeldBack(_) => None,
            _ => self.record(),
        }
    }

    /// The folder, when it is up.
    pub fn up(&self) -> Option<&Up> {
        match &self.is {
            Is::Up(up) => Some(up),
            _ => None,
        }
    }

    /// The folder's OneDrive side, when it is up and shows OneDrive.
    pub fn onedrive(&self) -> Option<&OneDriveFolder> {
        match self.up().map(|up| &up.content) {
            Some(Content::OneDrive(onedrive)) => Some(onedrive),
            _ => None,
        }
    }

    pub fn onedrive_mut(&mut self) -> Option<&mut OneDriveFolder> {
        match &mut self.is {
            Is::Up(Up { content: Content::OneDrive(onedrive), .. }) => Some(onedrive),
            _ => None,
        }
    }

    /// The folder's sync, when one runs.
    pub fn running(&self) -> Option<&RunningSync> {
        match self.onedrive().map(|onedrive| &onedrive.sync) {
            Some(Sync::Running(sync)) => Some(sync),
            _ => None,
        }
    }

    /// The recorded folder, to change what is kept with it.
    pub fn record_mut(&mut self) -> Option<&mut Record> {
        match &mut self.is {
            Is::Absent => None,
            Is::Down(record, _) => Some(record),
            Is::Up(up) => Some(&mut up.record),
        }
    }

    /// The tree store of the recorded OneDrive folder, once it has been opened.
    pub fn store(&self) -> Option<Store> {
        self.record().and_then(|record| record.kept.store.clone())
    }

    /// Where the folder's files are filled from ([`Kept::source`]).
    pub fn source(&self) -> Option<Arc<dyn ContentSource>> {
        self.acted_on().and_then(|record| record.kept.source.clone())
    }

    /// Whether a OneDrive folder's sync may run: the folder is up, shows OneDrive and is
    /// intercepted (HS2).
    pub fn syncs(&self) -> bool {
        self.up().is_some_and(|up| up.record.source == RootSource::OneDrive && up.record.intercepted())
    }
}

/// What the readers that only look, and the synchronous callers, read of the folder: a
/// copy made by [`publish`](super::publish) each time a [`Stopped`] says so or is dropped.
/// It cannot change the folder: of a running sync it holds only what can wake its parts
/// or tell them to stop ([`Handles`]), never what waits for them.
#[derive(Clone, Default)]
pub(super) struct View {
    /// The folder this account's calls act on, up or not, without what is kept with it
    /// ([`Record::bare`]). `None` with no folder, and for an
    /// account held back, whose folder nothing acts on but a Forget.
    pub record: Option<Record>,
    /// Why the recorded folder is not up, when it is not: what `LastError` says, or what
    /// it waits for.
    pub down: Option<String>,
    /// The mode the folder follows.
    pub wanted: Mode,
    /// Where the folder's files are filled from ([`Folder::source`]).
    pub source: Option<Arc<dyn ContentSource>>,
    /// The tree lock of the folder, for `RestoreDeletes`, which takes it with no state
    /// lock.
    pub tree_lock: Option<Arc<tokio::sync::Mutex<()>>>,
    /// The tree store of the recorded OneDrive folder ([`Folder::store`]).
    pub store: Option<Store>,
    pub sync: SyncView,
}

/// The sync of the folder, as the readers see it.
#[derive(Clone, Default)]
pub(super) enum SyncView {
    /// The folder is not one that has a sync: there is none, it is not up, or it is local.
    #[default]
    None,
    /// A OneDrive folder is up and its sync does not run: why, when it could not start.
    Stopped(Option<String>),
    Running(Handles),
}

impl SyncView {
    /// The number of the sync in the view, told to stop or not.
    pub fn id(&self) -> Option<u64> {
        match self {
            SyncView::Running(handles) => Some(handles.id),
            _ => None,
        }
    }
}

impl View {
    /// The sync running now, as far as a reader may reach it. One that has been told to
    /// stop is not running, whatever this view was made from.
    pub fn running(&self) -> Option<&Handles> {
        match &self.sync {
            SyncView::Running(running) if !running.told_to_stop() => Some(running),
            _ => None,
        }
    }
}

/// The folder's state, held for writing with the folder's sync stopped: the only way to
/// change what the folder is. Every function that changes it, or starts its sync, takes
/// one.
///
/// Dropped, it publishes the state. A function that starts a part which reads the
/// [`View`] publishes first ([`publish`](Self::publish)).
pub(super) struct Stopped<'a> {
    service: &'a SyncService,
    folder: RwLockWriteGuard<'a, Folder>,
    ran: bool,
    /// Which sync the change took out of the state, if one ran ([`Handles::id`]).
    took: Option<u64>,
}

impl<'a> Stopped<'a> {
    pub fn folder(&self) -> &Folder {
        &self.folder
    }

    pub fn folder_mut(&mut self) -> &mut Folder {
        &mut self.folder
    }

    /// Whether a sync ran when the change began — or when an earlier change, cut before
    /// its end, stopped it: one the change starts again if the folder is still one that
    /// syncs.
    pub fn ran(&self) -> bool {
        self.ran
    }


    /// The sync this change took out of the state, by its number.
    pub fn took(&self) -> Option<u64> {
        self.took
    }

    /// Publishes the state as it is now.
    pub fn publish(&self) {
        self.service.publish(&self.folder, |_| {});
    }

    /// [`publish`](Self::publish), with `also` applied in the same update of what the bus
    /// shows, so that nothing is published in between.
    pub fn publish_with(&self, also: impl FnOnce(&mut crate::status::snapshot::SyncSnapshot)) {
        self.service.publish(&self.folder, also);
    }
}

impl Drop for Stopped<'_> {
    fn drop(&mut self) {
        self.publish();
    }
}

impl SyncService {
    /// The one door to changing the folder: tells the folder's sync to stop, takes the
    /// state for writing, takes the sync out of it and waits until every part has ended.
    ///
    /// The sync is told before the lock is waited for: a reconcile holds the state for
    /// reading while it changes the folder, and checks for a stop between its steps, so
    /// told first it lets go at its next step rather than at the end of the whole
    /// reconcile. Every part is told at once.
    ///
    /// Taking the sync out of the state drops it, which hands its parts to the service
    /// ([`Ended`](super::running_sync::Ended)); they are waited for here, under the lock, with
    /// whatever a change that was cut before its end left there. So a section of a part
    /// that has begun (a file call, a commit) has run to its end before the change goes
    /// on, also when the change that stopped the part was dropped while it waited.
    pub(super) async fn change(&self) -> Stopped<'_> {
        // Told to stop, and gone from the view in the same breath: from here on no reader
        // takes the sync for running, also when this change is cut before it has the lock.
        // The next change, or `Refresh()`, then finds a sync that is told and not waited
        // for, takes it out and starts another.
        let told = self.view.send_if_modified(|view| match &view.sync {
            SyncView::Running(handles) => {
                handles.cancel();
                view.sync = SyncView::Stopped(None);
                true
            }
            _ => false,
        });
        if told {
            self.state.update_if_changed(|s| s.folder.writable = false);
        }
        let folder = self.folder.write().await;
        // Dropped from here on, the change publishes what it leaves.
        let mut stopped = Stopped { service: self, folder, ran: false, took: None };
        if let Some(onedrive) = stopped.folder.onedrive_mut() {
            match std::mem::replace(&mut onedrive.sync, Sync::Stopped(Why::Interrupted)) {
                // Dropped here: told again, and its parts handed over to be waited for.
                Sync::Running(sync) => {
                    stopped.ran = true;
                    stopped.took = Some(sync.handles().id);
                }
                Sync::Stopped(Why::Interrupted) => stopped.ran = true,
                Sync::Stopped(why) => onedrive.sync = Sync::Stopped(why),
            }
        }
        // What the folder is now, before the parts are waited for: another change may have
        // started a sync, and published it, since this one took it out of the view.
        stopped.publish();
        self.ended.join().await;
        if stopped.ran {
            // The worker's counts go with it (the outbox on the bus), and the watcher's
            // note with the watcher; the next ones say their own. Also for a sync an
            // earlier change, cut before its end, stopped.
            self.clear_outbox_counts();
            self.state.update(|s| s.local.watch_note.clear());
            // Under the lock, where no other change can have started a sync meanwhile
            // (B-M1).
            self.let_go_of_activity().await;
        }
        stopped
    }

    /// What the folder is, as last published.
    pub(super) fn view(&self) -> View {
        self.view.borrow().clone()
    }

    /// The folder this account's calls act on, up or not.
    pub(super) fn record(&self) -> Option<Record> {
        self.view.borrow().record.clone()
    }

    pub(super) fn require_record(&self) -> Result<Record, super::SyncError> {
        self.record().ok_or(super::SyncError::NoRoot)
    }

    /// The sync running now, as far as a reader may reach it: what wakes its parts.
    pub(super) fn running(&self) -> Option<Handles> {
        self.view.borrow().running().cloned()
    }

    /// The tree store of the recorded OneDrive folder, as last published.
    pub(super) fn tree_store(&self) -> Option<Store> {
        self.view.borrow().store.clone()
    }
}
