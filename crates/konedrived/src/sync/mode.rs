//! The folder's side of the account's mode (`docs/design/writes.md` §2, §2.2): a OneDrive folder follows
//! the mode its account runs in (`Account.Mode`). Read-only keeps it under the lock, as the
//! read phase did; read-write lifts the lock, looks for local changes and uploads them.
//!
//! A switch stops the folder's sync, changes the mode inside a change of its state, walks the
//! folder — the lock off, or back on — and starts the sync again, whose first cycle is a Full
//! reconcile under the new mode.
//!
//! What the mode starts and stops with the folder's sync is beside this file: the watcher
//! of a read-write folder (`watcher`), whose bring-up walk the lock waits for before it
//! comes off, and the outbox worker with its rows (`outbox`), which [`PendingUploads`]
//! counts before a switch to read-only and drops when that switch is forced.

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Weak};

use crate::local::watcher::WatchHandle;
use crate::upload::OutboxHandle;
use super::running_sync::Lock;
use tokio_util::sync::CancellationToken;
use super::folder::View;

use tokio::sync::watch;

use crate::folder::disk::Disk;
use crate::local::ScanReason;
use super::folder::Stopped;
use crate::folder::root::SyncRoot;
use crate::local::watcher::WalkState;
use crate::folder::locks::InodeKey;
use super::{RootSource, SyncService};
use crate::account::PendingUploads;
use crate::config::{Mode, WriteStanding};
use crate::account::state::AccountSnapshot;
use crate::status::snapshot::OutboxNote;

impl SyncService {
    /// The mode the folder follows now, as last published.
    pub fn mode(&self) -> Mode {
        self.view().wanted
    }

    /// Follows the account to `mode` (`docs/design/writes.md` §2, §2.2). The folder's sync is stopped as
    /// a Forget stops it — which stops the watcher too ([`stop_watcher`](Self::stop_watcher))
    /// — and the mode changed inside one change of the folder's state, so no reconcile,
    /// registration or free-up runs meanwhile. Then, for a OneDrive folder that is up:
    ///
    /// - to read-write, the lock comes off with the walk a Forget uses (files `0644`,
    ///   directories `0755`), and every placeholder is made `0644` from then on. When the
    ///   sync runs, it comes off as the sync starts again, once the watcher has marked every
    ///   directory, so none is made in the folder before it is watched (write design Z2);
    /// - to read-only, the lock goes back on, over every file and directory of ours.
    ///
    /// The sync then starts again, if it ran: its first cycle is a Full reconcile, and in
    /// read-write mode it starts the watcher, whose walk ends in the Full local scan
    /// ([`start_watcher`](Self::start_watcher)).
    /// A folder that is not up, and a walk the daemon did not finish, are walked when
    /// the folder's sync next starts, in either mode
    /// ([`ensure_unlocked`](Self::ensure_unlocked), [`ensure_locked`](Self::ensure_locked)).
    /// A local folder only takes the mode.
    ///
    /// The accounts manager calls this once before the folder is restored, with the mode
    /// the account starts in: nothing is up then, so the folder only takes the mode, and
    /// its bring-up lifts the lock and starts the watcher.
    pub async fn follow_mode(&self, mode: Mode) {
        if self.mode() == mode {
            return;
        }
        let mut stopped = self.change().await;
        // Looked at again inside the change: a forced drop may have turned the folder
        // already.
        let mut reason = ScanReason::Start;
        if stopped.folder().wanted != mode {
            // Nothing is dropped: a sign-out, the gate, `config.toml`, a narrower grant keep
            // the changes waiting to upload — the folder is locked, and its sync holds its
            // cycles while they wait, so no read-only reconcile puts back what they describe
            // (`listing`'s poller). Only a forced switch drops them, and turns the folder
            // itself (`PendingUploads::drop_pending_uploads`).
            self.turn(&mut stopped, mode, false).await;
            if mode == Mode::ReadWrite {
                // The watcher that starts next says why its Full local scan runs.
                reason = ScanReason::ReadWrite;
            }
        }
        if stopped.ran() && stopped.folder().syncs() {
            self.start_sync(&mut stopped, reason).await;
        }
    }

    /// The switch itself, to `mode`, inside a change; the caller starts the sync again.
    /// `dropping`: the changes waiting to upload are dropped as the folder turns read-only,
    /// a forced switch's.
    async fn turn(&self, stopped: &mut Stopped<'_>, mode: Mode, dropping: bool) {
        stopped.folder_mut().wanted = mode;
        // A switch tries a watcher again.
        if let Some(onedrive) = stopped.folder_mut().onedrive_mut() {
            onedrive.watcher_ended = None;
        }
        // What reads the mode from now on (the write gate) reads the new one.
        stopped.publish();
        if dropping {
            self.drop_outbox(stopped).await;
        }
        // What the folder said in the other mode goes with it: the watcher's
        // and the handles' notes of a read-write folder, and why the outbox waits in either;
        // the sync starting below says it again if it still holds.
        self.state.update(|s| {
            if mode == Mode::ReadOnly {
                s.local.watch_note.clear();
                s.local.handles_note.clear();
            }
            s.outbox.note = None;
        });
        let root = stopped.folder().up().filter(|up| up.record.source == RootSource::OneDrive).map(|up| up.record.root.clone());
        if let Some(root) = root {
            tracing::info!("{} is {} now", root.path.display(), mode.as_str());
            match mode {
                // `start_sync` lifts it once a watcher has walked the folder; a folder whose
                // sync does not run stays locked until one does (the watcher).
                Mode::ReadWrite => {}
                Mode::ReadOnly => self.relock(&root).await,
            }
        }
    }

    /// What became of the lock of a read-write folder once its watcher's walk is over
    /// (`walked` no longer says `Walking`): off — taken off here if the root still had it (a
    /// switch to read-write, one whose walk did not finish, or one made while the folder was
    /// not brought up; the root is unlocked last, `Disk::unlock_tree`) — or staying, with the
    /// sentence, when the walk was cut short: no directory is made in the folder before it is
    /// watched (the watcher). `None` when the walk was cut because the sync was told to
    /// stop (`stop`): nothing is wrong with the watcher, and nothing is said.
    pub(super) async fn lock_after_walk(&self, root: &SyncRoot, walked: &mut watch::Receiver<WalkState>, stop: &CancellationToken) -> Option<Lock> {
        let state = walked.wait_for(|state| *state != WalkState::Walking).await.map_or(WalkState::Cut, |state| *state);
        if state == WalkState::Done {
            if root_writable(root).await == Some(false) {
                self.unlock(root).await;
            }
            Some(Lock::Off)
        } else if stop.is_cancelled() {
            None
        } else {
            tracing::warn!("{} is not writable: its watcher did not finish walking it", root.path.display());
            Some(Lock::Stays("the folder stays read-only and nothing is uploaded: local changes could not be watched (see the log)".into()))
        }
    }

    /// Whether the root of the folder still has the read-only lock: then the lock comes off
    /// inside the change that starts the sync, which waits for the watcher's walk. A root
    /// that is unlocked already (a daemon start) keeps no change waiting for the walk.
    pub(super) async fn root_locked(&self, root: &SyncRoot) -> bool {
        root_writable(root).await == Some(false)
    }

    /// Puts the lock back on a read-only folder whose root does not have it: a
    /// switch to read-only whose walk did not finish (the root is locked last,
    /// `Disk::lock_tree`), or one made while the folder was not brought up. Called as its sync
    /// starts, so the folder is never left writable until a Full reconcile, which needs Graph.
    pub(super) async fn ensure_locked(&self, root: &SyncRoot) {
        if root_writable(root).await == Some(true) {
            self.relock(root).await;
        }
    }

    /// The walk that puts the lock back (`docs/design/writes.md` §2.2), on a blocking thread. A file a
    /// fill or a free-up holds is left for the first Full reconcile, which locks it.
    async fn relock(&self, root: &SyncRoot) {
        let (root, locks) = (root.clone(), self.locks.clone());
        let locked = tokio::task::spawn_blocking(move || {
            Disk::open(&root, true)
                .and_then(|disk| disk.lock_tree(|file| Ok(locks.try_lock(InodeKey::of(file)?))))
                .map_err(|e| format!("cannot put the read-only lock back on {}: {e}", root.path.display()))
        })
        .await
        .unwrap_or_else(|e| Err(format!("the lock task failed: {e}")));
        if let Err(e) = locked {
            tracing::warn!("{e}");
        }
    }

    /// What a read-write folder's cycle shares with its watcher and its outbox worker
    /// (`docs/design/writes.md` §9): the tree lock, the watcher's first scan to wait for, the
    /// watcher itself, handed what the reconcile kept or copied for examination, and the
    /// worker, told that a cycle went through. Each is the part of the same sync, reached
    /// directly; `tidy` is what tidies after a row the cycle dropped
    /// ([`tidy_after_cycle`](Self::tidy_after_cycle)).
    pub(super) fn cycle_writes(
        &self,
        scanned: Option<watch::Receiver<bool>>,
        tree_lock: &Arc<tokio::sync::Mutex<()>>,
        watcher: WatchHandle,
        outbox: OutboxHandle,
        tidy: Arc<dyn Fn(Vec<konedrive_tree::outbox::OutboxRow>) + Send + Sync>,
    ) -> crate::remote::listing::Writes {
        let (cycled, reopened, counted) = (outbox.clone(), outbox.clone(), outbox);
        crate::remote::listing::Writes {
            tree_lock: Arc::clone(tree_lock),
            machine_name: self.machine_name(),
            ignore: Arc::clone(&self.ignore),
            scanned,
            examine: Arc::new(move |batch| watcher.examine(batch)),
            // The worker, which waits for the folder's first delta cycle, may go.
            cycled: Arc::new(move || cycled.cycle_done()),
            reopened: Arc::new(move || reopened.wake()),
            dropped_removed: Arc::new(move |rows| {
                // `HeldCount`/`PendingCount` count the drop at once, not at the
                // worker's own next wake (the outbox on the bus).
                counted.wake();
                tidy(rows);
            }),
        }
    }
}

/// The write gate of a folder: whether its account may change OneDrive now — asked by
/// the outbox worker before each row and between an upload's fragments
/// ([`OutboxHost::may_write`](crate::upload::OutboxHost::may_write)). It may while the folder and
/// the account are read-write, `config.toml` — read again now — says read-write and lets the
/// account's drive through, the drive its token was last seen to reach is that one, its
/// token can write, and the folder's sync is not stopped by blocking trouble (`CycleError::blocking`:
/// another account's drive, a sign-out, a failure of the tree store; the cycle that
/// clears it wakes the worker, `Writes::reopened`). Closed, the folder's `LastError`
/// says why until it opens, and the account's mode is worked out again, which turns it
/// read-only and says why in the account's `LastError`.
///
/// It holds what it reads, and no way back to the service.
pub(super) struct Gate {
    view: watch::Receiver<View>,
    account: Arc<dyn crate::account::FolderAccount>,
    persist: super::Persist,
    state: crate::status::snapshot::SyncStateHandle,
}

impl Gate {
    /// Blocking: it reads `config.toml`, here and in the mode check. The worker asks its
    /// host from a blocking thread, as one section (`Engine::may_write`).
    pub(super) fn check(&self) -> Result<(), String> {
        let refusal = self.refusal();
        let note = OutboxNote::after_gate(&self.state.get().outbox.note, refusal.as_deref());
        let changed = note.is_some();
        if let Some(note) = note {
            self.state.update(|s| s.outbox.note = note);
        }
        match refusal {
            None => Ok(()),
            Some(why) => {
                if changed {
                    self.account.recheck_mode();
                }
                Err(why)
            }
        }
    }

    /// Why the gate is closed now, if it is.
    fn refusal(&self) -> Option<String> {
        if self.view.borrow().wanted != Mode::ReadWrite {
            return Some("the folder is read-only".into());
        }
        let persist = &self.persist;
        let snapshot = self.account.snapshot();
        if snapshot.mode != Mode::ReadWrite {
            return Some("the account is read-only".into());
        }
        if !konedrive_graph::oauth::grants_writes(&snapshot.granted_scopes) {
            return Some("the account's sign-in does not allow changes".into());
        }
        match persist.store.write_standing(&persist.account) {
            None => return Some("config.toml cannot be read".into()),
            Some(WriteStanding { mode: Mode::ReadOnly, .. }) => return Some("config.toml says the account is read-only".into()),
            Some(WriteStanding { writable_drive: None, .. }) => {
                return Some("write_test_drive_ids in config.toml does not list the account's drive".into())
            }
            Some(WriteStanding { writable_drive: Some(drive), .. }) if drive != snapshot.live_drive => {
                return Some(format!(
                    "the account's token was last seen to reach drive {:?}, not drive {:?}, which config.toml lets through",
                    snapshot.live_drive,
                    drive.as_str()
                ))
            }
            Some(_) => {}
        }
        if let Some(trouble) = self.state.get().cycle.sync_trouble.filter(|t| t.blocking) {
            return Some(format!("the folder's sync is stopped ({})", trouble.text));
        }
        None
    }
}

impl SyncService {
    /// The folder's write gate ([`Gate`]).
    pub(super) fn gate(&self) -> Gate {
        Gate {
            view: self.view.subscribe(),
            account: Arc::clone(&self.wiring.account),
            persist: self.wiring.persist.clone(),
            state: self.state.clone(),
        }
    }
}

/// Whether the folder's root has its owner's write bit: `None` when it cannot be looked at.
async fn root_writable(root: &SyncRoot) -> Option<bool> {
    let path = root.path.clone();
    tokio::task::spawn_blocking(move || std::fs::symlink_metadata(&path).ok().map(|meta| meta.permissions().mode() & 0o200 != 0))
        .await
        .ok()
        .flatten()
}

#[async_trait::async_trait]
impl PendingUploads for SyncService {
    /// `RefreshInfo` read the quota: the outbox decides by it whether OneDrive is
    /// still full (issue #2).
    fn quota_read(&self, quota: &konedrive_graph::drive::DriveQuota) {
        self.quota_seen(quota);
    }

    /// How many changes wait to be uploaded — the outbox's live rows
    /// (`konedrive_tree::outbox`). A switch to read-only is refused `PendingUploads` while
    /// this is not 0 and the switch is not forced. The watcher hands over and has examined
    /// what it holds first (the watcher), so a change saved a moment ago counts; with no
    /// completed listing to examine it against, it cannot, and is not counted.
    async fn pending_uploads(&self) -> u64 {
        self.flush_watcher().await;
        // Read with the folder's state held for reading: the store is the running sync's,
        // and no Forget removes it under the read.
        let folder = self.folder.read().await;
        let Some(store) = folder.store() else { return 0 };
        store.call(|s| s.outbox_len()).await.map_or(0, |n| n as u64)
    }

    /// A forced switch to read-only drops the outbox's rows (`docs/design/writes.md`
    /// §2); the files stay, as ordinary local changes, protected by the read phase's stamp
    /// check and rescue. Called once `config.toml` says read-only. The worker is told to
    /// stop with every other part, before anything is waited for, so nothing more is sent.
    ///
    /// The folder's sync stops for the drop and starts again after it: the rows go inside
    /// a change of the folder's state, with no task running, as at any switch, so nothing
    /// here holds the state or the tree lock while it waits for the other with a cycle
    /// under way (`docs/design/writes.md` §9).
    ///
    /// A read-write folder turns read-only here, in the same step, rather than when it
    /// follows its account ([`SyncService::follow_mode`], which then finds it turned): its
    /// watcher is stopped and no other starts, so no scan records again what was dropped,
    /// whether or not the folder is ever told to follow.
    ///
    /// A folder read-only already — its sync holding its cycles for these changes — starts
    /// its sync again without them, as a read-only start does: what a read-write cycle
    /// deferred is the base's, and the first cycle is a Full reconcile.
    async fn drop_pending_uploads(&self) {
        let mut stopped = self.change().await;
        if stopped.folder().wanted == Mode::ReadWrite {
            self.turn(&mut stopped, Mode::ReadOnly, true).await;
        } else {
            self.drop_outbox(&mut stopped).await;
        }
        self.start_again(&mut stopped).await;
    }
}

/// Makes `sync` follow its account's mode (`AccountSnapshot::mode`, which the account works
/// out from `config.toml`, the gate and its token) for as long as both exist. The accounts
/// manager starts one per account, once the folder has taken the mode the account started
/// in.
pub async fn follow(mut account: watch::Receiver<AccountSnapshot>, sync: Weak<SyncService>) {
    loop {
        let mode = account.borrow_and_update().mode;
        let Some(service) = sync.upgrade() else { return };
        service.follow_mode(mode).await;
        drop(service);
        if account.changed().await.is_err() {
            return;
        }
    }
}
