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
//! The running parts of a OneDrive folder's sync (the poller, the watcher, the outbox
//! worker, the tree store, the content source) are not in the state yet: they stay the
//! service's own fields, started and stopped only by a holder of a `Stopped`.

use tokio::sync::{MutexGuard, RwLockWriteGuard};

use super::{RootSource, SyncService};
use crate::config::Mode;
use crate::folder::root::SyncRoot;
use crate::hydration::recovery::RecoveryReport;

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
}

impl Record {
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

    /// Whether a OneDrive folder's sync may run: the folder is up, shows OneDrive and is
    /// intercepted (HS2).
    pub fn syncs(&self) -> bool {
        self.up().is_some_and(|up| up.record.source == RootSource::OneDrive && up.record.intercepted())
    }
}

/// What the readers that only look, and the synchronous callers, read of the folder: a
/// copy made by [`publish`](super::publish) each time a [`Stopped`] says so or is dropped.
/// It cannot change the folder.
#[derive(Debug, Clone, Default)]
pub(super) struct View {
    /// The folder this account's calls act on, up or not. `None` with no folder, and for an
    /// account held back, whose folder nothing acts on but a Forget.
    pub record: Option<Record>,
    /// Why the recorded folder is not up, when it is not: what `LastError` says, or what
    /// it waits for.
    pub down: Option<String>,
    /// The mode the folder follows.
    pub wanted: Mode,
}

/// The folder's state, held for writing with the folder's sync stopped: the only way to
/// change what the folder is. Every function that changes it, or starts or stops a part of
/// its sync, takes one.
///
/// Dropped, it publishes the state. A function that starts a part which reads the
/// [`View`] publishes first ([`publish`](Self::publish)).
pub(super) struct Stopped<'a> {
    service: &'a SyncService,
    folder: RwLockWriteGuard<'a, Folder>,
    ran: bool,
}

impl<'a> Stopped<'a> {
    pub fn folder(&self) -> &Folder {
        &self.folder
    }

    pub fn folder_mut(&mut self) -> &mut Folder {
        &mut self.folder
    }

    /// Whether a sync ran when the change began: one the change stopped, and starts again
    /// if the folder is still one that syncs.
    pub fn ran(&self) -> bool {
        self.ran
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

    /// The tree lock (`docs/design/writes.md` §9), under the state lock: safe here and only
    /// here, because the folder's sync is stopped, and only a cycle holds the tree lock
    /// while it waits for the state lock (§3.7 of the design; `F198`).
    pub async fn tree(&self) -> MutexGuard<'a, ()> {
        self.service.tree_lock.lock().await
    }
}

impl Drop for Stopped<'_> {
    fn drop(&mut self) {
        self.publish();
    }
}

impl SyncService {
    /// The one door to changing the folder: stops the folder's sync, takes the state for
    /// writing, and stops a sync another change started meanwhile.
    ///
    /// The sync is stopped before the lock is waited for: a reconcile holds the state for
    /// reading while it changes the folder, and checks for a stop between its steps, so
    /// stopped first it lets go at its next step rather than at the end of the whole
    /// reconcile. The activity log lets go of the store under the lock, where no other
    /// change can have started a sync meanwhile (B-M1).
    pub(super) async fn change(&self) -> Stopped<'_> {
        let ran = self.stop_tasks().await;
        let folder = self.folder.write().await;
        let ran = self.stop_tasks().await || ran;
        if ran {
            self.let_go_of_activity().await;
        }
        Stopped { service: self, folder, ran }
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
}
