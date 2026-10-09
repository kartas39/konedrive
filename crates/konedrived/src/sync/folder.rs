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
//! What runs for a folder that is up is part of the state too
//! ([`Up::sync`], [`Sync`](super::running_sync::Sync)): its sync is started only by a
//! holder of a `Stopped`, and stopped by [`SyncService::change`], which takes it out of
//! the state. So there is no sync without a folder that is up, and no
//! tree store open for a folder that is forgotten.

use std::sync::Arc;

use konedrive_tree::Store;
use tokio::sync::RwLockWriteGuard;

use super::running_sync::{Handles, RunningSync, Sync, Why};
use super::SyncService;
use crate::config::Mode;
use crate::folder::root::SyncRoot;
use crate::fuse::Mount;
use crate::hydration::source::ContentSource;

/// An account's folder: whether the account takes one, the mode it is to follow, and what
/// it is now.
pub(super) struct Folder {
    pub standing: Standing,
    /// The account's mode as the folder follows it (`docs/design/writes.md` §2, §2.2):
    /// a read-only folder's mount takes no change, a read-write one's does.
    pub wanted: Mode,
    pub is: Is,
}

/// Whether the account takes a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Standing {
    Active,
    /// `config.toml` gives the account what an earlier account has (`docs/design/accounts.md` §4.1): its
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
    /// A folder is recorded and not up: no sync runs for it.
    Down(Record, Down),
    /// Registered, and shown through its mount from its first sync on.
    Up(Up),
}

/// A folder as the daemon holds it, up or not: what `config.toml` records of it.
#[derive(Debug, Clone)]
pub(super) struct Record {
    pub root: SyncRoot,
    /// Whether *this daemon* excluded the folder from Baloo, so that a Forget takes
    /// exactly that exclusion off again.
    pub baloo: bool,
    /// What the daemon keeps for the folder for as long as it is recorded, up or down.
    pub kept: Kept,
}

/// What is kept with a recorded folder until it is forgotten, its account removed, or
/// another folder registered: it does not go when the folder goes down. So a folder that
/// is down still shows its names, still says what waits to be uploaded and whether it is
/// paused.
#[derive(Clone, Default)]
pub(super) struct Kept {
    /// Where the folder's files are downloaded from: the drive, from its first bring-up
    /// on (`None` when the daemon was given no drive).
    pub source: Option<Arc<dyn ContentSource>>,
    /// The folder's tree store: opened when its sync first starts, or when a change
    /// first needs it, and the one connection to it from then on.
    pub store: Option<Store>,
    /// The per-root tree lock (`docs/design/writes.md` §9): the outbox worker holds it
    /// across each commit that touches `items`, the mount's recorder across each change it
    /// records, and a cycle must hold it from staging to swap, or the swap reverts the
    /// commit. One for as long as the folder is recorded, so that what took it before a
    /// sync was started again still keeps the new sync's cycle out.
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// The folder's mount, from its first sync on: it stays while the folder is recorded,
    /// so that the folder shows its names also while no sync runs.
    pub mount: Option<Arc<Mount>>,
}

impl std::fmt::Debug for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kept").field("source", &self.source.is_some()).field("store", &self.store.is_some()).field("mount", &self.mount.is_some()).finish()
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
}

/// Why a recorded folder is not up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Down {
    /// Recorded, and not brought up yet in this run of the daemon.
    NotYetUp,
    /// `config.toml` says what the folder shows in a word that is not `"onedrive"`
    /// (`written`): it is never brought up on a guess (`SY6`).
    UnreadSource { written: String },
    /// A bring-up failed. The sentence that says so.
    Failed { why: String },
}

impl Down {
    /// The sentence `LastError` and a `NotUp` refusal say of a folder at `root` that is
    /// down for this; empty while it only waits.
    pub fn why(&self, root: &SyncRoot) -> String {
        match self {
            Down::NotYetUp => String::new(),
            Down::Failed { why } => why.clone(),
            Down::UnreadSource { written } => format!(
                "cannot bring up the sync folder {}: config.toml has source = {written:?} for it, which is not \
                 \"onedrive\"; correct it and start konedrive again, or forget the folder and add it again",
                root.path.display()
            ),
        }
    }

    /// What a folder that only waits waits for, for a refusal that has to say why the
    /// folder is not up; `None` when it is down for a reason of its own ([`why`](Self::why)).
    pub fn waits_for(&self) -> Option<&'static str> {
        match self {
            Down::NotYetUp => Some("it is being brought up"),
            Down::UnreadSource { .. } | Down::Failed { .. } => None,
        }
    }
}

/// A folder that is up.
pub(super) struct Up {
    pub record: Record,
    pub sync: Sync,
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

    /// The sync of the folder that is up, running or not, to put another in its place.
    pub fn sync_mut(&mut self) -> Option<&mut Sync> {
        match &mut self.is {
            Is::Up(up) => Some(&mut up.sync),
            _ => None,
        }
    }

    /// The folder's sync, when one runs.
    pub fn running(&self) -> Option<&RunningSync> {
        match self.up().map(|up| &up.sync) {
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

    /// The tree store of the recorded folder, once it has been opened.
    pub fn store(&self) -> Option<Store> {
        self.record().and_then(|record| record.kept.store.clone())
    }

    /// The folder's mount, once it has been mounted.
    pub fn mount(&self) -> Option<&Arc<Mount>> {
        self.record().and_then(|record| record.kept.mount.as_ref())
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
    /// The tree lock of the folder, for `RestoreDeletes`, which takes it with no state
    /// lock.
    pub tree_lock: Option<Arc<tokio::sync::Mutex<()>>>,
    /// The tree store of the recorded folder ([`Folder::store`]).
    pub store: Option<Store>,
    /// The folder's mount ([`Folder::mount`]).
    pub mount: Option<Arc<Mount>>,
    pub sync: SyncView,
}

/// The sync of the folder, as the readers see it.
#[derive(Clone, Default)]
pub(super) enum SyncView {
    /// There is no folder, or it is not up.
    #[default]
    None,
    /// The folder is up and its sync does not run: why, when it could not start.
    Stopped(Option<String>),
    Running(Handles),
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
    /// Whether the folder's mount took changes when the change began.
    was_writable: bool,
    /// The desktop was told to read the folder again in this change.
    told: bool,
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

    /// Tells the desktop's programs to read the folder's directories again: its mount
    /// came, or shows other modes now.
    pub fn read_again(&mut self) {
        self.told = true;
        if let (Some(store), Some(record)) = (self.folder.store(), self.folder.record()) {
            tokio::spawn(crate::desktop::dirnotify::read_folder_again(store, record.root.path.clone()));
        }
    }

    /// Whether the folder's mount takes changes now.
    fn writable(&self) -> bool {
        self.folder.mount().is_some_and(|mount| mount.access_mode_switch().is_read_write())
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
    /// A change that leaves the folder taking changes when it took none, or the other way
    /// round, has the desktop read it again: the modes its mount shows are others.
    fn drop(&mut self) {
        if self.writable() != self.was_writable && !self.told {
            self.read_again();
        }
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
        let mut stopped = Stopped { service: self, folder, ran: false, was_writable: false, told: false };
        if let Some(sync) = stopped.folder.sync_mut() {
            match std::mem::replace(sync, Sync::Stopped(Why::Interrupted)) {
                // Dropped here: told again, and its parts handed over to be waited for.
                Sync::Running(_) => stopped.ran = true,
                Sync::Stopped(Why::Interrupted) => stopped.ran = true,
                Sync::Stopped(why) => *sync = Sync::Stopped(why),
            }
        }
        // No sync, no uploads: the mount takes no change until a read-write sync runs again.
        // The switch is made on the mount's write-operation thread and waited for: when it
        // returns, every change a program asked for before it is recorded, and none is
        // recorded after it, so that what a forced switch drops from the outbox next
        // (`drop_outbox`) leaves no row behind. That thread runs under the tree lock, which a
        // cycle holds while it waits for the folder's state, held here: so the switch comes
        // only now, with the sync taken out and told to stop, which a cycle gives up on.
        if let Some(mount) = stopped.folder.mount() {
            stopped.was_writable = mount.switch_to_read_only().await;
        }
        // What the folder is now, before the parts are waited for: another change may have
        // started a sync, and published it, since this one took it out of the view.
        stopped.publish();
        self.ended.join().await;
        if stopped.ran {
            // The worker's counts go with it (the outbox on the bus); the next one says its
            // own. Also for a sync an earlier change, cut before its end, stopped.
            self.clear_outbox_counts();
            // Under the lock, where no other change can have started a sync meanwhile.
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

    /// The tree store of the recorded folder, as last published.
    pub(super) fn tree_store(&self) -> Option<Store> {
        self.view.borrow().store.clone()
    }
}
