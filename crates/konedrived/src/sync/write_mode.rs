//! The folder's side of the account's mode (`docs/design/writes.md` §2, §2.2): a OneDrive folder follows
//! the mode its account runs in (`Account.Mode`). Read-only keeps it under the lock, as the
//! read phase did; read-write lifts the lock, looks for local changes and uploads them.
//!
//! A switch stops the folder's sync, changes the mode under the lifecycle lock, walks the
//! folder — the lock off, or back on — and starts the sync again, whose first cycle is a Full
//! reconcile under the new mode.
//!
//! What the mode starts and stops with the folder's sync is beside this file: the watcher
//! of a read-write folder (`watcher`), whose bring-up walk the lock waits for before it
//! comes off, and the outbox worker with its rows (`outbox`), which [`PendingUploads`]
//! counts before a switch to read-only and drops when that switch is forced.

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

use tokio::sync::watch;

use crate::folder::disk::Disk;
use crate::folder::root::SyncRoot;
use crate::local::watcher::WalkState;
use crate::folder::locks::InodeKey;
use super::{RootSource, SyncService};
use crate::account::PendingUploads;
use crate::config::Mode;
use crate::account::state::AccountSnapshot;
use crate::status::snapshot::OutboxNote;

impl SyncService {
    /// The mode the folder follows now.
    pub fn mode(&self) -> Mode {
        *self.mode.lock().unwrap()
    }

    /// The mode the account runs in as the daemon starts, set before its folder is brought
    /// up: no walk, and no hook. The bring-up itself lifts a lock a read-write folder still
    /// has ([`ensure_unlocked`](Self::ensure_unlocked)) and starts the watcher and the Full
    /// local scan.
    pub fn start_in_mode(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
        self.state.update(|s| s.scan.follow(mode));
    }

    /// Follows the account to `mode` (`docs/design/writes.md` §2, §2.2). The folder's sync is stopped as
    /// a Forget stops it — which stops the watcher too ([`stop_watcher`](Self::stop_watcher))
    /// — and the mode changed with `lifecycle` held for writing, so no reconcile, registration
    /// or free-up runs meanwhile. Then, for a OneDrive folder that is brought up:
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
    /// A folder not brought up yet, and a walk the daemon did not finish, are walked when
    /// the folder's sync next starts, in either mode
    /// ([`ensure_unlocked`](Self::ensure_unlocked), [`ensure_locked`](Self::ensure_locked)).
    /// A local folder only takes the mode.
    pub async fn follow_mode(&self, mode: Mode) {
        if self.mode() == mode {
            return;
        }
        let was_syncing = self.stop_tasks().await;
        let _lifecycle = self.lifecycle.write().await;
        let was_syncing = self.stop_tasks().await || was_syncing;
        if was_syncing {
            self.let_go_of_activity().await;
        }
        // Looked at again under the lock: a forced drop may have turned the folder already.
        if self.mode() != mode {
            // Nothing is dropped: a sign-out, the gate, `config.toml`, a narrower grant keep
            // the changes waiting to upload — the folder is locked, and its sync holds its
            // cycles while they wait, so no read-only reconcile puts back what they describe
            // (`listing`'s poller). Only a forced switch drops them, and turns the folder
            // itself (`PendingUploads::drop_pending_uploads`).
            self.turn(mode, false).await;
        }
        if was_syncing {
            self.start_sync().await;
        }
    }

    /// The switch itself, to `mode`: the caller holds `lifecycle` for writing, has stopped the
    /// folder's tasks, and starts the sync again. `dropping`: the changes waiting to upload
    /// are dropped as the folder turns read-only, a forced switch's.
    async fn turn(&self, mode: Mode, dropping: bool) {
        *self.mode.lock().unwrap() = mode;
        // The watcher that starts next says why its Full local scan runs.
        self.switched_to_read_write.store(mode == Mode::ReadWrite, Ordering::SeqCst);
        self.state.update(|s| s.scan.follow(mode));
        if dropping {
            self.drop_outbox().await;
        }
        // What the folder said in the other mode goes with it: the watcher's
        // and the handles' notes of a read-write folder, and why the outbox waits in either;
        // the sync starting below says it again if it still holds.
        self.state.update(|s| {
            if mode == Mode::ReadOnly {
                s.watch_note.clear();
                s.handles_note.clear();
            }
            s.outbox_note = None;
        });
        let folder = self.registration().filter(|reg| reg.source == RootSource::OneDrive && reg.brought_up);
        if let Some(reg) = &folder {
            tracing::info!("{} is {} now", reg.root.path.display(), mode.as_str());
            match mode {
                // `start_sync` lifts it once a watcher has walked the folder; a folder whose
                // sync does not run stays locked until one does (the watcher).
                Mode::ReadWrite => {}
                Mode::ReadOnly => self.relock(&reg.root).await,
            }
        }
    }

    /// Takes the lock off a read-write folder whose root still has it: a switch to read-write,
    /// one whose walk did not finish (the root is unlocked last, `Disk::unlock_tree`), or one
    /// made while the folder was not brought up. Called as its sync starts, once `walked` says
    /// its watcher has marked every directory, so that none is made in the folder before it is
    /// watched; a walk cut short leaves the lock on, and says so (the watcher). A root that is
    /// unlocked already (a daemon start) waits for nothing (the watcher).
    pub(super) async fn ensure_unlocked(&self, root: &SyncRoot, mut walked: watch::Receiver<WalkState>) {
        if root_writable(root).await != Some(false) {
            return;
        }
        let state = walked.wait_for(|state| *state != WalkState::Walking).await.map_or(WalkState::Cut, |state| *state);
        if state == WalkState::Done {
            self.unlock(root).await;
        } else {
            tracing::warn!("{} stays locked: its watcher did not finish walking it", root.path.display());
            self.state.update(|s| {
                s.watch_note = "the folder stays read-only and nothing is uploaded: local changes could not be watched \
                                (see the log)"
                    .into()
            });
        }
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

    /// The walk that takes the lock off (`docs/design/writes.md` §2.2), on a blocking thread.
    async fn unlock(&self, root: &SyncRoot) {
        let root = root.clone();
        let unlocked = tokio::task::spawn_blocking(move || {
            Disk::open(&root, false)
                .and_then(|disk| disk.unlock_tree())
                .map_err(|e| format!("cannot take the read-only lock off {}: {e}", root.path.display()))
        })
        .await
        .unwrap_or_else(|e| Err(format!("the unlock task failed: {e}")));
        if let Err(e) = unlocked {
            tracing::warn!("{e}");
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
    /// (`docs/design/writes.md` §9): the tree lock, the watcher's first scan to wait for, where to
    /// hand what the reconcile kept or copied for examination, and the word that a cycle
    /// went through.
    pub(super) fn cycle_writes(&self, scanned: Option<watch::Receiver<bool>>) -> crate::remote::listing::Writes {
        let me = self.me.clone();
        // Off the reconcile's blocking task: it captured this runtime
        // before entering it, as the materializer's fills do.
        let runtime = tokio::runtime::Handle::current();
        let dropped_removed: Arc<dyn Fn(Vec<konedrive_tree::outbox::OutboxRow>) + Send + Sync> = Arc::new(move |rows| {
            let Some(service) = me.upgrade() else { return };
            // `HeldCount`/`PendingCount` count the drop at once, not at the
            // worker's own next wake (the outbox on the bus).
            service.wake_outbox();
            let Some(reg) = service.registration() else { return };
            let store = service.store.lock().unwrap().clone();
            let Some(store) = store else { return };
            runtime.spawn(async move { service.tidy_dropped(&reg.root, &store, &rows).await });
        });
        crate::remote::listing::Writes {
            tree_lock: Arc::clone(&self.tree_lock),
            machine_name: self.machine_name(),
            ignore: Arc::clone(&self.ignore),
            scanned,
            examine: self.examine_hook(),
            cycled: self.cycled_hook(),
            reopened: self.outbox_waker(),
            dropped_removed,
            #[cfg(test)]
            before_swap: None,
        }
    }

    /// Whether this folder's account may change OneDrive now — asked by
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
    /// Blocking: it reads `config.toml`, here and in the mode check. The worker asks its
    /// host from a blocking thread, as one section (`Engine::may_write`).
    pub(super) fn write_gate(&self) -> Result<(), String> {
        let refusal = self.gate_refusal();
        let note = OutboxNote::after_gate(&self.state.get().outbox_note, refusal.as_deref());
        let changed = note.is_some();
        if let Some(note) = note {
            self.state.update(|s| s.outbox_note = note);
        }
        match refusal {
            None => Ok(()),
            Some(why) => {
                if changed {
                    self.wiring.account.recheck_mode();
                }
                Err(why)
            }
        }
    }

    /// Why the write gate is closed now, if it is ([`write_gate`](Self::write_gate)).
    fn gate_refusal(&self) -> Option<String> {
        if self.mode() != Mode::ReadWrite {
            return Some("the folder is read-only".into());
        }
        let persist = &self.wiring.persist;
        let snapshot = self.wiring.account.snapshot();
        if snapshot.mode != Mode::ReadWrite {
            return Some("the account is read-only".into());
        }
        if !konedrive_graph::oauth::grants_writes(&snapshot.granted_scopes) {
            return Some("the account's sign-in does not allow changes".into());
        }
        match persist.store.write_standing(&persist.account) {
            None => return Some("config.toml cannot be read".into()),
            Some((Mode::ReadOnly, _)) => return Some("config.toml says the account is read-only".into()),
            Some((_, None)) => return Some("write_test_drive_ids in config.toml does not list the account's drive".into()),
            Some((_, Some(drive))) if drive != snapshot.live_drive => {
                return Some(format!(
                    "the account's token was last seen to reach drive {:?}, not drive {drive:?}, which config.toml lets through",
                    snapshot.live_drive
                ))
            }
            Some(_) => {}
        }
        if let Some(trouble) = self.state.get().sync_trouble.filter(|t| t.blocking) {
            return Some(format!("the folder's sync is stopped ({})", trouble.text));
        }
        None
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
        // Read with the lifecycle lock held, as every clone of the store outside the sync is.
        let _lifecycle = self.lifecycle.read().await;
        let Some(store) = self.store.lock().unwrap().clone() else { return 0 };
        store.call(|s| s.outbox_len()).await.map_or(0, |n| n as u64)
    }

    /// A forced switch to read-only drops the outbox's rows (`docs/design/writes.md`
    /// §2); the files stay, as ordinary local changes, protected by the read phase's stamp
    /// check and rescue. Called once `config.toml` says read-only. The worker stops first, so
    /// nothing more is sent.
    ///
    /// The folder's sync stops for the drop and starts again after it: the rows go with
    /// `lifecycle` held for writing and no task running, as at any switch, so nothing here
    /// holds `lifecycle` or the tree lock while it waits for the other with a cycle under
    /// way (`docs/design/writes.md` §9). The mode is read under that lock, which every change
    /// of it holds.
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
        self.stop_outbox().await;
        let was_syncing = self.stop_tasks().await;
        let _lifecycle = self.lifecycle.write().await;
        let was_syncing = self.stop_tasks().await || was_syncing;
        if was_syncing {
            self.let_go_of_activity().await;
        }
        if self.mode() == Mode::ReadWrite {
            self.turn(Mode::ReadOnly, true).await;
        } else {
            self.drop_outbox().await;
        }
        if was_syncing {
            self.start_sync().await;
        }
    }
}


/// Makes `sync` follow its account's mode (`AccountSnapshot::mode`, which the account works
/// out from `config.toml`, the gate and its token) for as long as both exist. The accounts
/// manager starts one per account, once the folder has taken the mode the account started
/// in ([`SyncService::start_in_mode`]).
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
