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
//! ([`Up::sync`], [`Sync`]): its sync is started only by a
//! holder of a `Stopped`, and stopped by [`SyncService::change`], which takes it out of
//! the state. So there is no sync without a folder that is up, and no
//! tree store open for a folder that is forgotten.

use std::sync::Arc;

use konedrive_tree::Store;
use tokio::sync::RwLockWriteGuard;
use tokio_util::sync::CancellationToken;

use crate::sync_tasks::start_order::{KeptFolderParts, RefusedStart, StartedFolder};

use super::SyncService;
use crate::config::Mode;
use crate::folder::root::SyncRoot;
use crate::mount::ItemMount;
use crate::content_source::source::ContentSource;

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
    /// The per-root tree lock (`docs/design/writes.md` §9): the plan's task holds it
    /// across each commit that touches `onedrive_items`, the mount's recorder across each change it
    /// records, and a cycle must hold it from staging to swap, or the swap reverts the
    /// commit. One for as long as the folder is recorded, so that what took it before a
    /// sync was started again still keeps the new sync's cycle out.
    pub tree_lock: Arc<tokio::sync::Mutex<()>>,
    /// What the folder's sync left when it was stopped: the store, the mount — still
    /// served, so that the folder shows its names also while no sync runs — and what the
    /// mount was made with. The very value `stop_order::stop_folder_sync` answered (or a
    /// refused start gave back), held whole and given whole to the next start
    /// (`FolderStartInput::kept`); never its fields apart. `None` before the folder's
    /// first sync, while a sync runs (the running sync holds them, `Sync::Running`), and
    /// after a take-down or a Forget ended them.
    pub parts: Option<KeptFolderParts>,
}

impl std::fmt::Debug for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kept").field("source", &self.source.is_some()).field("parts", &self.parts.is_some()).finish()
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

/// Whether the sync of a OneDrive folder that is up runs. A running sync is what the
/// start answered (`sync_tasks::start_order::StartedFolder`), held whole; a stopped one
/// left its parts in the folder's record (`Kept::parts`, `KeptFolderParts`).
pub(super) enum Sync {
    Stopped(Why),
    Running(Box<StartedFolder>),
}

/// Why a OneDrive folder's sync does not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Why {
    /// Nothing has started it yet: the folder came up a moment ago.
    NotStarted,
    /// A change of the folder stopped it and has not started it again: the change is
    /// under way, or was cut before its end. The next change starts it.
    Interrupted,
    /// It could not start. The sentence; `Refresh()` tries again.
    CannotStart(String),
    /// It could not start because no drive is known for the account yet
    /// (`StartRefusal::DriveNotKnown`). The sentence. The owner's watcher of the account
    /// starts it again when the account changes (`mode::watch_account`); `Refresh()`
    /// tries too.
    DriveNotKnown(String),
    /// A fault took the folder's mount down (`TakeDownCause`): the sync was stopped, the
    /// mount and the store ended, and the folder shows the trouble with its cause.
    /// `cause` is the sentence shown. The owner starts the folder again by itself:
    /// `attempt` counts the starts tried since the last one that stayed up (0: none
    /// yet), `next_try_at` is when the next is due, Unix seconds
    /// (`take_down::decide_retry_wait`) — `None` when none is (the store could not be
    /// closed: `TakeDownOutcome::StoreStillOpen`). `Refresh()` tries at once and leaves
    /// the count; a start that stayed up (`take_down::STAYED_UP_AFTER`) forgets it. The
    /// timer and the count are this state and nothing else's.
    TakenDown { cause: String, attempt: u32, next_try_at: Option<i64> },
}

/// A folder that is up.
pub(super) struct Up {
    pub record: Record,
    pub sync: Sync,
    /// For a sync that was started after a fault took the folder's mount down — by the
    /// owner's own timer, or by the user's `Refresh()` of a folder that was
    /// `Why::TakenDown`, which is written with the attempt as it was, not counted up —:
    /// which try this start was, and when it was made. `None` for every other start, and
    /// forgotten once the start stayed up (`take_down::decide_take_down_attempt`). Kept
    /// here, beside the sync, because a running sync (`StartedFolder`) knows nothing of
    /// why it was started: what `take_folder_down_for_fault` reads when the mount is
    /// taken down again.
    pub take_down_try: Option<TakeDownTry>,
}

/// A start of the folder that the owner made by itself after a take-down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TakeDownTry {
    /// How many such starts were made since one stayed up, this one counted: 1 for the
    /// first.
    pub attempt: u32,
    /// When this start was made, Unix seconds, by the account's clock.
    pub started_at: i64,
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

    /// The folder that is up, to change its sync and the try it was started as.
    pub fn up_mut(&mut self) -> Option<&mut Up> {
        match &mut self.is {
            Is::Up(up) => Some(up),
            _ => None,
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
    pub fn running(&self) -> Option<&StartedFolder> {
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

    /// The tree store of the recorded folder, once it has been opened: the running
    /// sync's, else the one a stopped sync kept.
    pub fn store(&self) -> Option<Store> {
        match self.running() {
            Some(started) => Some(started.store.clone()),
            None => self.record().and_then(|record| record.kept.parts.as_ref()).map(|parts| parts.store.clone()),
        }
    }

    /// The folder's mount, once it has been mounted: the running sync's, else the one a
    /// stopped sync kept.
    pub fn mount(&self) -> Option<&Arc<ItemMount>> {
        match self.running() {
            Some(started) => Some(&started.mount),
            None => self.record().and_then(|record| record.kept.parts.as_ref()).map(|parts| &parts.mount),
        }
    }

    /// What the queue's answers and the `Files` interface read of the folder's sync,
    /// running or stopped: the parts a stopped sync kept, or those of the running one
    /// (a clone: every part is a handle). `None` for a folder that keeps none.
    pub fn parts(&self) -> Option<KeptFolderParts> {
        match self.running() {
            Some(started) => Some(KeptFolderParts {
                store: started.store.clone(),
                mount: Arc::clone(&started.mount),
                sync_plan_wake: Arc::clone(&started.sync_plan_wake),
                running_steps: Arc::clone(&started.running_steps),
                published_plan: Arc::clone(&started.published_plan),
                drive_id: started.drive_id.clone(),
            }),
            None => self.record().and_then(|record| record.kept.parts.clone()),
        }
    }
}

/// What the readers that only look, and the synchronous callers, read of the folder: a
/// copy made by [`publish`](super::publish) each time a [`Stopped`] says so or is dropped.
/// It cannot change the folder: of a running sync it holds only the token that tells its
/// tasks to stop (`SyncTasks::stop_handle`), never what waits for them.
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
    /// The parts of the folder's sync, running or stopped ([`Folder::parts`]), held as
    /// the one type they are: the store, the mount (with its `Files` interface), the
    /// published plan and the running steps the queue's answers are made over, and the
    /// wake of the plan, for the user's answers and a change of the ignore list. `None`
    /// for a folder that keeps none. The owner publishes a view without them before a
    /// take-down or a Forget closes the store: each holds it open.
    pub parts: Option<KeptFolderParts>,
    /// The poll of a RUNNING sync (`SyncTasks::poll`): what asks its listing for a cycle
    /// and has its live socket looked at again, for the callers that are not async and
    /// hold no lock of the folder (`show_pause`, `refresh_now`, `nudge`). `None` while no
    /// sync runs.
    pub poll: Option<crate::listing::listing_poller::ListingPollHandle>,
    /// What holds the sending of a RUNNING sync, and whether its OneDrive is full: `None`
    /// while no sync runs — the queue's answers then say them as not known
    /// (`upload_queue::describe_stopped_sync`).
    pub sending_holds: Option<Arc<crate::sync_parts::sending_holds::SendingHolds>>,
    pub onedrive_space: Option<Arc<crate::sync_parts::onedrive_space::OneDriveSpace>>,
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
    Running(CancellationToken),
}

impl View {
    /// The sync running now, as far as a reader may reach it. One that has been told to
    /// stop is not running, whatever this view was made from.
    pub fn running(&self) -> Option<&CancellationToken> {
        match &self.sync {
            SyncView::Running(running) if !running.is_cancelled() => Some(running),
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
        self.service.tell_desktop_to_read_folder_again(&self.folder);
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

impl Stopped<'_> {
    /// Puts the parts a stopped sync keeps, or a refused start gave back, into the folder's record, whole.
    fn keep_parts(&mut self, kept: Option<KeptFolderParts>) {
        match (kept, self.folder.record_mut()) {
            (Some(kept), Some(record)) => record.kept.parts = Some(kept),
            // No folder is recorded to keep them: their mount is stopped where a thread may be joined.
            (Some(kept), None) => drop(tokio::task::spawn_blocking(move || drop(kept))),
            (None, _) => {}
        }
    }
}

/// What a start or a stop of the folder's sync answered.
pub(super) enum SyncChange {
    /// `stop_order::stop_folder_sync`'s answer.
    Stopped(KeptFolderParts),
    /// `start_order::start_folder_sync`'s answers.
    Started(Box<StartedFolder>),
    Refused(RefusedStart),
}

/// The start or the stop of the folder's sync that runs now, in a task of its own, and what it
/// answered when nobody waited for it to its end.
///
/// **State owned:** the gate one such task holds while it runs, and its answer until
/// somebody takes it. A start and a stop are never cut (`sync_tasks`' module doc): the
/// caller that is cut leaves the task running, the task leaves its answer here, and the
/// next change of the folder waits for the task and takes the answer
/// ([`SyncService::change`]). Only a holder of the folder's write lock runs one, and it
/// waits first: one task at a time.
#[derive(Default)]
pub(super) struct SyncChangeTask {
    gate: Arc<tokio::sync::Mutex<()>>,
    left: Arc<std::sync::Mutex<Option<SyncChange>>>,
}

impl SyncChangeTask {
    /// Waits for the task that runs, if one does, and takes what it left.
    pub async fn wait(&self) -> Option<SyncChange> {
        let _ended = self.gate.lock().await;
        crate::panic::lock(&self.left).take()
    }

    /// Runs `work` in a task of its own and waits for it; its answer, `None` when it
    /// panicked. Cut while it waits, the task goes on and its answer is the next
    /// [`wait`](Self::wait)'s.
    ///
    /// `adopt` is run when this call is cut while it waits, and never otherwise: it takes
    /// the answer into the folder's state without waiting for the next change of the
    /// folder to come by itself ([`SyncService::adopt_left_sync_change`]). Until then a
    /// sync that was started runs outside the state and the view, where nothing of the
    /// account reaches it.
    pub async fn run(&self, work: impl std::future::Future<Output = SyncChange> + Send + 'static, adopt: impl FnOnce() + Send + 'static) -> Option<SyncChange> {
        // Nothing is awaited before the task is spawned: `work` owns what it starts or stops, and is not dropped
        // unrun. The gate is free: the caller waited under the folder's write lock.
        let gate = Arc::clone(&self.gate).try_lock_owned().ok();
        if gate.is_none() {
            tracing::error!("a start or a stop of the folder's sync begins while another runs");
        }
        let left = Arc::clone(&self.left);
        let task = tokio::spawn(async move {
            let answer = work.await;
            *crate::panic::lock(&left) = Some(answer);
            drop(gate);
        });
        let mut waiter = CutWaiter { adopt: Some(Box::new(adopt)) };
        let ended = task.await;
        waiter.adopt = None;
        if let Err(error) = ended {
            tracing::error!("a start or a stop of the folder's sync failed inside konedrive: {error}");
        }
        crate::panic::lock(&self.left).take()
    }
}

/// Who waits for a start or a stop: dropped while it still waits — its caller was cut —
/// it has the answer adopted.
struct CutWaiter {
    adopt: Option<Box<dyn FnOnce() + Send>>,
}

impl Drop for CutWaiter {
    fn drop(&mut self) {
        if let Some(adopt) = self.adopt.take() {
            adopt();
        }
    }
}

/// The folder's state, held for writing with the folder's sync LEFT RUNNING: the second
/// door, for the changes that stop no task — the access mode
/// (`sync_tasks::access_mode_change`, with `Folder::wanted`) and nothing else. It is what
/// serializes the two that switch the mode: the watcher's `follow_mode` and the account's
/// `hold_unsent_changes` each hold one for the whole of their switch, so neither sees the
/// other half done, and the mode applied is the one the account says when the lock is
/// held. A step towards the folder waits for the folder's lease — a read of this lock —
/// while one is held, and is not cut; no step towards OneDrive and no listing waits for
/// it. Whoever holds one must not stop or start the sync: that is [`Stopped`]'s.
///
/// Dropped, it publishes the state as a `Stopped` does: `Writable`, the view, and the
/// desktop told to read the folder again when the folder takes changes where it took
/// none, or the other way round.
pub(super) struct HeldFolder<'a> {
    service: &'a SyncService,
    folder: RwLockWriteGuard<'a, Folder>,
    /// Whether the folder's mount took changes when the lock was taken.
    was_writable: bool,
}

impl HeldFolder<'_> {
    pub fn folder(&self) -> &Folder {
        &self.folder
    }

    /// The mode the folder follows from now on.
    pub fn set_wanted(&mut self, wanted: Mode) {
        self.folder.wanted = wanted;
    }
}

impl Drop for HeldFolder<'_> {
    fn drop(&mut self) {
        // A switch that leaves the folder taking changes when it took none, or the other way round, has the desktop
        // read it again, as a `Stopped` does: the modes its mount shows are others.
        let is_writable = self.folder.mount().is_some_and(|mount| mount.access_mode_switch().is_read_write());
        if is_writable != self.was_writable {
            self.service.tell_desktop_to_read_folder_again(&self.folder);
        }
        self.service.publish(&self.folder, |_| {});
    }
}

impl SyncService {
    /// The second door ([`HeldFolder`]): takes the folder's state for writing and leaves
    /// its sync running. Nothing is told to stop.
    pub(super) async fn hold_folder(&self) -> HeldFolder<'_> {
        let folder = self.folder.write().await;
        let was_writable = folder.mount().is_some_and(|mount| mount.access_mode_switch().is_read_write());
        HeldFolder { service: self, folder, was_writable }
    }

    /// The one door to changing the folder: tells the folder's sync to stop, takes the
    /// state for writing, takes the sync out of it and waits until every part has ended.
    ///
    /// The sync is told before the lock is waited for (the stop's token, from the view): a
    /// step towards the folder holds the folder's lease, a read of this lock, and one that
    /// still waits for it ends unrun once told. Every task is told at once.
    ///
    /// The stop itself (`sync_tasks::stop_order::stop_folder_sync`) runs in a task of its
    /// own and is waited for here, under the lock, with whatever a start or a stop left
    /// whose caller was cut before its end ([`SyncChangeTask`]). So a step that has begun
    /// has run to its end before the change goes on, also when the change that stopped
    /// the sync was dropped while it waited. What the stopped sync keeps — the store, the
    /// mount, still served — goes into the folder's record, whole (`Kept::parts`): the
    /// folder shows its names while no sync runs, and the next start is given them back.
    pub(super) async fn change(&self) -> Stopped<'_> {
        // Told to stop, and gone from the view in the same breath: from here on no reader takes the sync for
        // running, also when this change is cut before it has the lock. The token is cancelled BEFORE the lock is
        // waited for: a step towards the folder that waits for the folder's lease then ends unrun, and the stop
        // awaited below, under the lock, waits for no step that waits for this very lock.
        self.view.send_if_modified(|view| match &view.sync {
            SyncView::Running(stop) => {
                stop.cancel();
                view.sync = SyncView::Stopped(None);
                view.poll = None;
                view.sending_holds = None;
                view.onedrive_space = None;
                true
            }
            _ => false,
        });
        let folder = self.folder.write().await;
        let was_writable = folder.mount().is_some_and(|mount| mount.access_mode_switch().is_read_write());
        // Dropped from here on, the change publishes what it leaves.
        let mut stopped = Stopped { service: self, folder, ran: false, was_writable, told: false };
        // What a start or a stop left whose caller was cut before it ended: waited for here, under the lock, and taken
        // into the state as that caller would have taken it.
        let mut running = None;
        match self.sync_change.wait().await {
            Some(SyncChange::Stopped(kept)) => stopped.keep_parts(Some(kept)),
            // A sync that was started and never in the state: stopped below, as one that ran.
            Some(SyncChange::Started(started)) => running = Some(started),
            // Said as the start's own caller would have said it: by what the sync was before the start.
            Some(SyncChange::Refused(refused)) => {
                stopped.keep_parts(refused.kept);
                if let Some(root) = stopped.folder.up().map(|up| up.record.root.clone()) {
                    self.refuse_start(&mut stopped, refused.refusal, &root);
                }
            }
            None => {}
        }
        if let Some(sync) = stopped.folder.sync_mut() {
            match std::mem::replace(sync, Sync::Stopped(Why::Interrupted)) {
                Sync::Running(started) => {
                    stopped.ran = true;
                    running = Some(started);
                }
                Sync::Stopped(Why::Interrupted) => stopped.ran = true,
                Sync::Stopped(why) => *sync = Sync::Stopped(why),
            }
        }
        if let Some(started) = running {
            stopped.ran = true;
            // Again: the sync of a start nobody waited for was never in the view.
            started.tasks.stop_handle().cancel();
            // In a task of its own: a caller that is cut does not cut the stop, and the next change waits for it.
            let stop = async move { SyncChange::Stopped(crate::sync_tasks::stop_order::stop_folder_sync(*started).await) };
            match self.sync_change.run(stop, self.adopt_left_sync_change()).await {
                Some(SyncChange::Stopped(kept)) => stopped.keep_parts(Some(kept)),
                Some(SyncChange::Started(_) | SyncChange::Refused(_)) | None => tracing::error!("the stop of the folder's sync failed inside konedrive: what it kept is gone, and the folder is started with nothing kept"),
            }
        }
        // What the folder is now: its sync stopped, its parts kept.
        stopped.publish();
        stopped
    }

    /// What a start or a stop is given to run when its caller is cut (`SyncChangeTask::run`): a task of the
    /// service's own makes one change of the folder — which waits for the start or the stop, takes its answer
    /// into the state — and starts again what ran, so that the folder is as the cut caller found it. By a weak
    /// reference: nothing when the service is gone, or no runtime is left to run it.
    pub(super) fn adopt_left_sync_change(&self) -> impl FnOnce() + Send + 'static {
        let (service, runtime) = (self.me.clone(), tokio::runtime::Handle::try_current());
        move || {
            let Ok(runtime) = runtime else { return };
            runtime.spawn(async move {
                let Some(service) = service.upgrade() else { return };
                let mut stopped = service.change().await;
                service.start_again(&mut stopped).await;
            });
        }
    }

    /// Tells the desktop's programs to read the directories `folder` shows again, in a task of its own, on the
    /// bus the service was made with (`Wiring::desktop_bus`); nothing with none, and nothing for a folder that
    /// keeps no open store to read its directories from.
    fn tell_desktop_to_read_folder_again(&self, folder: &Folder) {
        if let (Some(desktop_bus), Some(store), Some(record)) = (self.wiring.desktop_bus.clone(), folder.store(), folder.record()) {
            tokio::spawn(crate::desktop::dirnotify::read_folder_again(desktop_bus, store, record.root.path.clone()));
        }
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

    /// The sync running now, as far as a reader may reach it: the token that stops it.
    pub(super) fn running(&self) -> Option<CancellationToken> {
        self.view.borrow().running().cloned()
    }

    /// The tree store of the recorded folder, as last published.
    pub(super) fn tree_store(&self) -> Option<Store> {
        self.view.borrow().parts.as_ref().map(|parts| parts.store.clone())
    }
}

#[cfg(test)]
mod tests;
