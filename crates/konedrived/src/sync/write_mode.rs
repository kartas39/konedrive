//! The folder's side of the account's mode (`docs/design/writes.md` §2, §2.2): a OneDrive folder follows
//! the mode its account runs in (`Account1.Mode`). Read-only keeps it under the lock, as the
//! read phase did; read-write lifts the lock, looks for local changes and uploads them.
//!
//! A switch stops the folder's sync, changes the mode under the lifecycle lock, walks the
//! folder — the lock off, or back on — and starts the sync again, whose first cycle is a Full
//! reconcile under the new mode.
//!
//! The write phase's later tasks fill the hooks here, each named for what it does:
//!
//! - [`SyncService::start_watcher`] and [`SyncService::stop_watcher`] — the notification
//!   watcher (`sync::watcher`), started with a read-write folder's sync and kept in it
//!   ([`Watcher`]), so that it stops exactly when that sync does. Its bring-up walk marks every
//!   directory and then runs the Full local scan (the examination of `local::Batch::full()`,
//!   whose rows the outbox worker sends): at bring-up, and right after a switch to read-write, whose lock
//!   comes off only once that walk is done;
//! - [`PendingUploads`] — the outbox worker's outbox: how many changes wait, asked before a switch to
//!   read-only (the watcher hands over what it holds first), and dropping them when that
//!   switch is forced;
//! - [`SyncService::start_outbox`] — the outbox worker (`sync::upload`), which sends those
//!   rows: started beside the watcher, kept in the same sync, stopped with it, and woken by
//!   the watcher's examination whenever it records rows.

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::watch;

use super::disk::Disk;
use super::root::SyncRoot;
use super::upload::{self, OutboxWorker, WorkerConfig};
use super::watcher::WalkState;
use super::{InodeKey, RootSource, SyncError, SyncService};
use crate::account::PendingUploads;
use crate::config::Mode;
use crate::state::AccountSnapshot;

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
        *self.mode.lock().unwrap() = mode;
        let forced = self.drop_at_read_only.swap(false, Ordering::SeqCst);
        if mode == Mode::ReadOnly {
            // Only a forced switch drops the changes waiting to upload; what
            // the watcher recorded after it dropped the others goes too. Any other
            // way here — a sign-out, the gate, `config.toml`, a narrower grant — keeps them: the
            // folder is locked, and its sync holds its cycles while they wait, so no read-only
            // reconcile puts back what they describe (`listing`'s poller).
            if forced {
                self.drop_outbox().await;
            }
        }
        // What the folder said in the other mode goes with it: the watcher's
        // and the handles' notes of a read-write folder, and why the outbox waits in either;
        // the sync starting below says it again if it still holds.
        self.state.update(|s| {
            if mode == Mode::ReadOnly {
                s.watch_note.clear();
                s.handles_note.clear();
            }
            s.outbox_note.clear();
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
        if was_syncing {
            self.start_sync().await;
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

    /// Starts the notification watcher (`docs/design/writes.md` §3) on the folder at
    /// `root`, and answers its handle. Called whenever a read-write folder's sync starts — at
    /// bring-up, and after a switch to read-write — with `lifecycle` held for writing and
    /// inside the critical section that publishes the sync: the handle is kept in
    /// the sync, and whoever stops the sync gets it for [`stop_watcher`](Self::stop_watcher).
    /// So it does not block and takes no lock: the watcher's bring-up walk (every directory
    /// marked for interception and for events, then the Full local scan) runs on its own
    /// thread, and [`Watcher::walked`] says when it is done. It starts nothing unless the
    /// folder is read-write; a watcher that cannot start is said in `LastError`.
    ///
    /// Called before the sync is published, so that a read-write folder whose watcher cannot
    /// start runs that sync locked, as a read-only one (the watcher).
    #[cfg(test)]
    pub(super) fn start_watcher(&self, root: &SyncRoot, store: &crate::tree::Store) -> Option<Watcher> {
        self.start_watcher_scanned(root, store, None)
    }

    /// [`start_watcher`](Self::start_watcher), and `scanned` is told once the watcher's first
    /// examination — the Full local scan — has been handed over, whatever came of it: the
    /// folder's first delta cycle waits for it (the bring-up order of `docs/design/writes.md` §2.2).
    pub(super) fn start_watcher_scanned(&self, root: &SyncRoot, store: &crate::tree::Store, scanned: Option<watch::Sender<bool>>) -> Option<Watcher> {
        if self.mode() != Mode::ReadWrite {
            return None;
        }
        #[cfg(test)]
        let spawned = if FAIL_WATCHER.with(std::cell::Cell::get) { Err("failed on purpose".to_owned()) } else { self.spawn_watcher(root, store, scanned) };
        #[cfg(not(test))]
        let spawned = self.spawn_watcher(root, store, scanned);
        match spawned {
            Ok(inner) => Some(Watcher { inner }),
            Err(why) => {
                tracing::warn!("{why}; the folder stays locked");
                self.state.update(|s| {
                    s.watch_note = format!("the folder stays read-only and nothing is uploaded: local changes cannot be watched ({why})")
                });
                None
            }
        }
    }

    /// Stops `watcher` and waits for it, with no lock taken. Called by whoever
    /// stops the sync that started it, and only by them: a Forget, a switch to read-only
    /// (before the lock goes back on), a switch to interception.
    pub(super) async fn stop_watcher(&self, watcher: Watcher) {
        self.stop_spawned_watcher(watcher.inner).await;
    }

    /// Starts the outbox worker of the read-write folder at `root` (`docs/design/writes.md`
    /// §5): the sync starting it keeps it, beside the watcher, and stops it with the
    /// watcher, without the lifecycle lock. Called in the critical section that publishes
    /// the sync: it spawns and returns, taking no lock. Rows a previous run left `running`
    /// are replayed first; the rest go as the watcher's examination records them.
    pub(super) fn start_outbox(&self, root: &SyncRoot, store: &crate::tree::Store, drive: &crate::drive::DriveClient) -> Option<OutboxWorker> {
        if self.mode() != Mode::ReadWrite {
            return None;
        }
        let worker = OutboxWorker::new(WorkerConfig {
            root: root.clone(),
            store: store.clone(),
            drive: drive.clone(),
            locks: self.locks.clone(),
            machine_name: self.machine_name(),
            tree_lock: Arc::clone(&self.tree_lock),
            host: Arc::new(super::outbox_api::Host::new(self.me.clone())),
            limits: upload::Limits::default(),
            // Moves out of the folder: the helper over this account's link, fills
            // through its source, and the hub's router.
            moved_out: Some(self.move_outs()),
        });
        // The folder's first delta cycle runs before the outbox (`docs/design/writes.md` §3).
        worker.wait_for_cycle(false);
        worker.start();
        Some(worker)
    }

    /// What a read-write folder's cycle shares with its watcher and its outbox worker
    /// (`docs/design/writes.md` §9): the tree lock, the watcher's first scan to wait for, where to
    /// hand what the reconcile kept or copied for examination, and the word that a cycle
    /// went through.
    pub(super) fn cycle_writes(&self, scanned: Option<watch::Receiver<bool>>) -> super::listing::Writes {
        let me = self.me.clone();
        let examine: Arc<dyn Fn(super::local::Batch) + Send + Sync> = Arc::new(move |batch| {
            let Some(service) = me.upgrade() else { return };
            let handle = service.syncing.lock().unwrap().as_ref().and_then(|s| s.watcher.as_ref()).map(|w| w.inner.handle());
            if let Some(handle) = handle {
                handle.examine(batch);
            }
        });
        let me = self.me.clone();
        let cycled: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let Some(service) = me.upgrade() else { return };
            let syncing = service.syncing.lock().unwrap();
            if let Some(outbox) = syncing.as_ref().and_then(|s| s.outbox.as_ref()) {
                outbox.cycle_done();
            }
        });
        let me = self.me.clone();
        // Off the reconcile's blocking task: it captured this runtime
        // before entering it, as the materializer's fills do.
        let runtime = tokio::runtime::Handle::current();
        let dropped_removed: Arc<dyn Fn(Vec<crate::tree::outbox::OutboxRow>) + Send + Sync> = Arc::new(move |rows| {
            let Some(service) = me.upgrade() else { return };
            // `HeldCount`/`PendingCount` count the drop at once, not at the
            // worker's own next wake (the outbox on the bus).
            service.wake_outbox();
            let Some(reg) = service.registration() else { return };
            let store = service.store.lock().unwrap().clone();
            let Some(store) = store else { return };
            runtime.spawn(async move { service.tidy_dropped(&reg.root, &store, &rows).await });
        });
        super::listing::Writes {
            tree_lock: Arc::clone(&self.tree_lock),
            machine_name: self.machine_name(),
            ignore: Arc::clone(&self.ignore),
            scanned,
            examine,
            cycled,
            dropped_removed,
        }
    }

    /// The network came back (`docs/design/writes.md` §9): the outbox waits for the delta
    /// cycle this asks for, then sends what backed off meanwhile.
    pub(super) fn outbox_after_network(&self) {
        let syncing = self.syncing.lock().unwrap();
        if let Some(outbox) = syncing.as_ref().and_then(|s| s.outbox.as_ref()) {
            outbox.wait_for_cycle(true);
        }
    }

    /// Whether this folder's account may change OneDrive now — asked by
    /// the outbox worker before each row and between an upload's fragments
    /// ([`OutboxHost::may_write`](upload::OutboxHost::may_write)). It may while the folder and
    /// the account are read-write, `config.toml` — read again now — says read-write and lets the
    /// account's drive through, the drive its token was last seen to reach is that one, its
    /// token can write, and the folder's sync is not stopped by something only a person can fix
    /// (another account's drive, a sign-out). Closed, the folder's `LastError`
    /// says why until it opens, and the account's mode is worked out again, which turns it
    /// read-only and says why in the account's `LastError`.
    pub(super) fn write_gate(&self) -> Result<(), String> {
        let refusal = self.gate_refusal();
        let note = refusal.as_ref().map(|why| format!("{GATE_NOTE}{why}")).unwrap_or_default();
        let shown = self.state.get().outbox_note;
        let changed = shown != note && (refusal.is_some() || shown.starts_with(GATE_NOTE));
        if changed {
            self.state.update(|s| s.outbox_note = note);
        }
        match refusal {
            None => Ok(()),
            Some(why) => {
                if changed {
                    let check = self.mode_check.lock().unwrap().clone();
                    if let Some(check) = check {
                        check();
                    }
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
        let (Some(account), Some(persist)) = (self.account.as_ref(), self.persist.as_ref()) else {
            return Some("the folder belongs to no account".into());
        };
        let snapshot = account.get();
        if snapshot.mode != Mode::ReadWrite {
            return Some("the account is read-only".into());
        }
        if !crate::oauth::grants_writes(&snapshot.granted_scopes) {
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

    /// What works the account's mode out again (`AccountService::recheck_mode`), for when the
    /// write gate closes under a running outbox worker ([`write_gate`](Self::write_gate)).
    pub fn set_mode_check(&self, check: Arc<dyn Fn() + Send + Sync>) {
        *self.mode_check.lock().unwrap() = Some(check);
    }

    /// Stops the running sync's outbox worker, if any, and waits for it: a request under way
    /// finishes, nothing more is taken. The rest of the sync goes on.
    async fn stop_outbox(&self) {
        let outbox = self.syncing.lock().unwrap().as_mut().and_then(|s| s.outbox.take());
        if let Some(outbox) = outbox {
            outbox.stop().await;
            self.clear_outbox_counts();
        }
    }

    /// What the watcher's examination calls when it recorded rows: wakes the outbox worker
    /// of the sync running now, if any.
    pub(super) fn outbox_waker(&self) -> Arc<dyn Fn() + Send + Sync> {
        let me = self.me.clone();
        Arc::new(move || {
            let Some(service) = me.upgrade() else { return };
            let syncing = service.syncing.lock().unwrap();
            if let Some(outbox) = syncing.as_ref().and_then(|s| s.outbox.as_ref()) {
                outbox.wake();
            }
        })
    }

    /// Drops the outbox's rows (`docs/design/writes.md` §2): a forced switch to read-only, and only that.
    /// The files stay, as ordinary local changes, and lose their upload
    /// mark (`user.konedrive.sync`). A rename half-done in OneDrive under a temporary name stays:
    /// dropped, the item would stay under that name, and its local object
    /// would go.
    async fn drop_outbox(&self) {
        let store = self.store.lock().unwrap().clone();
        let root = self.registration().map(|reg| reg.root);
        let (Some(store), Some(root)) = (store, root) else { return };
        let (dropping, marked) = (store.clone(), root.clone());
        // Under the tree lock: a cycle's swap must not give a moved-out item back the object
        // it forgets here.
        let tree = self.tree_lock.lock().await;
        let dropped = tokio::task::spawn_blocking(move || {
            let rows = dropping.with(|s| {
                let mut rows = upload::move_out::drop_rows(s)?;
                rows.extend(s.outbox_drop_all()?);
                Ok(rows)
            })?;
            upload::clear_marks(&marked, &rows);
            Ok::<_, crate::tree::TreeError>(rows)
        })
        .await;
        drop(tree);
        self.clear_outbox_counts();
        // What moves out of the folder left outside it is tidied.
        self.forget_moved_out();
        if let Ok(Ok(rows)) = &dropped {
            self.tidy_dropped(&root, &store, rows).await;
        }
        match dropped.map(|rows| rows.map(|rows| rows.len())) {
            Ok(Ok(0)) => {}
            Ok(Ok(n)) => tracing::info!("{n} change(s) waiting to upload were dropped; the files stay as they are"),
            Ok(Err(e)) => tracing::warn!("cannot drop the changes waiting to upload: {e}"),
            Err(e) => tracing::warn!("the task dropping the changes waiting to upload failed: {e}"),
        }
    }

    /// The helper is back (`docs/design/writes.md` §3): what the watcher could not have marked for
    /// interception meanwhile is asked again, and a Full local scan finds what changed.
    pub(super) fn watcher_helper_back(&self) {
        if let Some(watcher) = self.syncing.lock().unwrap().as_ref().and_then(|s| s.watcher.as_ref()) {
            watcher.inner.helper_back();
        }
    }
}

/// How long the switch to read-only waits for the watcher to hand over what it holds.
const FLUSH_WITHIN: Duration = Duration::from_secs(30);

/// How the folder's `LastError` begins while the write gate is closed.
const GATE_NOTE: &str = "nothing is uploaded: ";

/// A read-write folder's watcher (`sync::watcher`), as its sync keeps it. Made only by
/// [`SyncService::start_watcher`], and given back to [`SyncService::stop_watcher`] by whoever
/// stops that sync.
pub struct Watcher {
    inner: super::watcher::Watcher,
}

impl Watcher {
    /// How far the watcher's bring-up walk got: the lock comes off a folder turning
    /// read-write only once it is done.
    pub(super) fn walked(&self) -> watch::Receiver<WalkState> {
        self.inner.walked()
    }

    /// A Full local scan now: the ignore list changed (`docs/design/writes.md` §4.4).
    pub(super) fn full_scan(&self) {
        self.inner.full_scan();
    }
}

#[cfg(test)]
thread_local! {
    /// Makes [`SyncService::start_watcher`] fail on this thread (tests of the watcher).
    pub(super) static FAIL_WATCHER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the folder's root has its owner's write bit: `None` when it cannot be looked at.
async fn root_writable(root: &SyncRoot) -> Option<bool> {
    let path = root.path.clone();
    tokio::task::spawn_blocking(move || std::fs::symlink_metadata(&path).ok().map(|meta| meta.permissions().mode() & 0o200 != 0))
        .await
        .ok()
        .flatten()
}

impl SyncService {
    /// The running watcher, if any, hands over and has examined what it holds (the watcher),
    /// so that a change saved a moment ago is in the outbox.
    pub(super) async fn flush_watcher(&self) {
        let handle = self.syncing.lock().unwrap().as_ref().and_then(|s| s.watcher.as_ref()).map(|w| w.inner.handle());
        if let Some(handle) = handle {
            let flushed = tokio::task::spawn_blocking(move || handle.flush(FLUSH_WITHIN)).await.unwrap_or(false);
            if !flushed {
                tracing::warn!("the latest local changes could not all be examined; they are not counted as waiting");
            }
        }
    }

    /// How many changes wait in this folder's tree store — the running
    /// sync's, or with none running, the one on disk its next sync opens — for a Forget and
    /// `Accounts1.Remove`, which would delete them with the store. Only read, so it needs no
    /// lock. A running store that cannot be read refuses; one on disk that cannot be opened
    /// holds nothing a sync could send (it is rebuilt empty).
    pub(super) async fn changes_in_store(&self) -> Result<u64, SyncError> {
        let running = self.store.lock().unwrap().clone();
        if let Some(store) = running {
            return store
                .run(|s| s.outbox_rows())
                .await
                .map(|rows| rows.len() as u64)
                .map_err(|e| SyncError::Io(format!("cannot tell whether changes wait to be uploaded: {e}")));
        }
        let Some(tree_db) = self.sync_paths.lock().unwrap().as_ref().map(|p| p.tree_db.clone()) else { return Ok(0) };
        if !tree_db.exists() {
            return Ok(0);
        }
        let counted = tokio::task::spawn_blocking(move || crate::tree::TreeStore::open(&tree_db).and_then(|s| s.outbox_rows()))
            .await
            .map_err(|e| e.to_string())
            .and_then(|rows| rows.map_err(|e| e.to_string()));
        Ok(counted.map_or_else(
            |e| {
                tracing::warn!("cannot read the folder's tree store to count its waiting changes: {e}");
                0
            },
            |rows| rows.len() as u64,
        ))
    }
}

#[async_trait::async_trait]
impl PendingUploads for SyncService {
    /// How many changes wait to be uploaded — the outbox's live rows
    /// (`crate::tree::outbox`). A switch to read-only is refused `PendingUploads` while
    /// this is not 0 and the switch is not forced. The watcher hands over and has examined
    /// what it holds first (the watcher), so a change saved a moment ago counts; with no
    /// completed listing to examine it against, it cannot, and is not counted.
    async fn pending_uploads(&self) -> u64 {
        self.flush_watcher().await;
        // Read with the lifecycle lock held, as every clone of the store outside the sync is.
        let _lifecycle = self.lifecycle.read().await;
        let Some(store) = self.store.lock().unwrap().clone() else { return 0 };
        store.run(|s| s.outbox_rows()).await.map_or(0, |rows| rows.len() as u64)
    }

    /// A forced switch to read-only drops the outbox's rows (`docs/design/writes.md`
    /// §2); the files stay, as ordinary local changes, protected by the read phase's stamp
    /// check and rescue. Called once `config.toml` says read-only, before the
    /// folder follows. The worker stops first, so nothing more is sent. A
    /// row the examination adds after it — the watcher still runs until then — is dropped
    /// when the folder turns read-only ([`SyncService::follow_mode`]), and only then: any
    /// other switch to read-only keeps them.
    ///
    /// A folder read-only already — its sync holding its cycles for these changes — starts
    /// its sync again without them, as a read-only start does: what a read-write cycle
    /// deferred is the base's, and the first cycle is a Full reconcile.
    async fn drop_pending_uploads(&self) {
        self.stop_outbox().await;
        if self.mode() == Mode::ReadWrite {
            let _lifecycle = self.lifecycle.read().await;
            self.drop_outbox().await;
            self.drop_at_read_only.store(true, Ordering::SeqCst);
            return;
        }
        let was_syncing = self.stop_tasks().await;
        let _lifecycle = self.lifecycle.write().await;
        let was_syncing = self.stop_tasks().await || was_syncing;
        if was_syncing {
            self.let_go_of_activity().await;
        }
        self.drop_outbox().await;
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
