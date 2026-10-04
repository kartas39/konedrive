//! A OneDrive folder's sync started ([`SyncService::start_sync`]: prepare, build, run) and
//! asked for a cycle (`Refresh()`, the network back). How it stops is in `running`.

use std::sync::{Arc, Mutex, OnceLock};

use konedrive_graph::drive::DriveClient;
use konedrive_tree::Store;
use tokio::sync::{watch, Notify};
use tokio_util::sync::CancellationToken;

use crate::account::state::SignInState;
use crate::config::Mode;
use crate::desktop::thumbs;
use crate::folder::root::SyncRoot;
use crate::hydration::source::ContentSource;
use crate::local::ScanReason;
use crate::remote::listing::{self, PollHandle};
use crate::sync::folder::{Down, Is, Record, Standing, Stopped};
use crate::sync::running_sync::{Handles, Lock, Parts, RunningSync, Sync, Why, Writes};
use crate::sync::{RootSource, SyncError, SyncPaths, SyncService};

/// What a sync is built on, once [`SyncService::prepare`] has made the folder ready.
struct Prepared {
    drive: DriveClient,
    paths: SyncPaths,
    store: Store,
    source: Arc<dyn ContentSource>,
    tree_lock: Arc<tokio::sync::Mutex<()>>,
}

impl SyncService {
    /// Starts the sync of the OneDrive folder that is up now. Inside a change of the
    /// folder's state, so that no Forget can come in between, and none runs: the change
    /// stopped it. `reason`: why the watcher of a read-write folder runs its Full local
    /// scan (`LocalScan.Reason`).
    ///
    /// Three steps: the folder is made ready ([`prepare`](Self::prepare)), the parts are
    /// built and linked to each other ([`build`](Self::build)), and they are started and
    /// put into the state, from where only a change takes them out.
    pub(super) async fn start_sync(&self, stopped: &mut Stopped<'_>, reason: ScanReason) {
        if !stopped.folder().syncs() {
            return;
        }
        debug_assert!(stopped.folder().running().is_none(), "a sync runs inside a change");
        // What the parts started here read of the folder is what it is now.
        stopped.publish();
        let Some(reg) = stopped.folder().up().map(|up| up.record.clone()) else { return };
        let wanted = stopped.folder().wanted;
        let prepared = match self.prepare(stopped, &reg, wanted).await {
            Ok(prepared) => prepared,
            Err(why) => return self.cannot_start(stopped, &reg.root, why).await,
        };
        let ended = stopped.folder().onedrive().and_then(|onedrive| onedrive.watcher_ended.clone());
        let (sync, walked) = self.build(prepared, &reg, wanted, reason, ended).await;
        let (lock, stop, tasks) = (sync.lock_cell(), sync.handles().stop.clone(), Arc::clone(&sync.handles().tidying));
        if let Some(onedrive) = stopped.folder_mut().onedrive_mut() {
            onedrive.sync = Sync::Running(Box::new(sync));
        }
        // The readers reach the sync from here on.
        stopped.publish();
        // `Paused` as the store keeps it, and a timer for a pause that ends.
        self.show_pause();
        // The folder is writable only once its watcher has walked it (`Lock::Walking` until
        // then). No directory is made in the folder before it is watched (write design Z2):
        // a root that still has the lock loses it here, inside the change, once the walk is
        // done and before this sync's first cycle can change the folder — the cycle takes
        // the folder's lease, which the caller's change holds. A root that is unlocked
        // already (a daemon start) keeps the change waiting for nothing: a task of the sync
        // says when the walk is over.
        let (Some(mut walked), Some(lock)) = (walked, lock) else { return };
        if self.root_locked(&reg.root).await {
            if let Some(now) = self.lock_after_walk(&reg.root, &mut walked, &stop).await {
                *lock.lock().unwrap() = now;
            }
            stopped.publish();
            return;
        }
        let (me, root) = (self.me.clone(), reg.root.clone());
        let task = tokio::spawn(async move {
            let state = tokio::select! {
                state = walked.wait_for(|state| *state != crate::local::watcher::WalkState::Walking) => state.map_or(crate::local::watcher::WalkState::Cut, |state| *state),
                () = stop.cancelled() => return,
            };
            let Some(service) = me.upgrade() else { return };
            let now = if state == crate::local::watcher::WalkState::Done {
                Lock::Off
            } else if stop.is_cancelled() {
                return;
            } else {
                tracing::warn!("{} is not writable: its watcher did not finish walking it", root.path.display());
                Lock::Stays("the folder stays read-only and nothing is uploaded: local changes could not be watched (see the log)".into())
            };
            // Published under the state held for reading: a change that comes meanwhile
            // has told this sync to stop, and waits for this task.
            let folder = tokio::select! {
                folder = service.folder.read() => folder,
                () = stop.cancelled() => return,
            };
            if stop.is_cancelled() {
                return;
            }
            *lock.lock().unwrap() = now;
            service.publish(&folder, |_| {});
        });
        tasks.lock().unwrap_or_else(|p| p.into_inner()).push(task);
    }

    /// Makes the folder ready for a sync: the lock as the mode wants it, the tree store
    /// open, what a read-write cycle deferred applied in a read-only folder, and the
    /// activity log kept in the store. Why not, when the sync cannot start.
    async fn prepare(&self, stopped: &mut Stopped<'_>, reg: &Record, wanted: Mode) -> Result<Prepared, String> {
        let Some(crate::sync::OneDrive { drive, paths }) = self.wiring.onedrive.clone() else {
            return Err(format!("{} shows OneDrive, but no drive is configured; it is not kept in step", reg.root.path.display()));
        };
        let Some(source) = reg.kept.source.clone() else {
            return Err(format!("{} shows OneDrive, but no drive is configured; it is not kept in step", reg.root.path.display()));
        };
        // The lock as the mode wants it (`docs/design/writes.md` §2.2): a walk a switch did not finish —
        // the daemon stopped, or the folder was not up — is finished here. A read-only folder
        // is locked before anything else, never left writable until a Full reconcile, which
        // needs Graph. A read-write one is unlocked once its watcher has marked every
        // directory (`start_sync`).
        if wanted != Mode::ReadWrite {
            self.ensure_locked(&reg.root).await;
        }
        let tree_lock = Arc::clone(&reg.kept.tree_lock);
        // The folder's store, the one connection to it: open already, or opened now, made
        // if there is none, and kept with the folder until it is forgotten.
        let store = self.kept_store(stopped, &paths).await?;
        // A read-only folder that still holds changes waiting to upload (a switch nobody
        // forced) runs no cycle while they wait: what a read-write cycle
        // deferred stays deferred for the read-write cycle that sends them.
        let rows_wait = wanted != Mode::ReadWrite && store.call(|s| s.outbox_len()).await.map_or(true, |n| n > 0);
        if wanted != Mode::ReadWrite && !rows_wait {
            // Changes a read-write cycle deferred are the base's now: a read-only cycle knows
            // none. Nothing at all for a folder that never was read-write.
            match store.call(|s| s.apply_deferred()).await {
                Ok(0) => {}
                Ok(n) => tracing::info!("{n} change(s) from OneDrive that waited for local changes are applied now"),
                Err(e) => tracing::warn!("cannot apply the changes from OneDrive that waited: {e}"),
            }
        }
        // The activity log and the conflicts are kept in this
        // store from now on, and `LastChecked` is where the last run left it.
        // Every caller is inside a change of the folder's state, so no other start can
        // attach a store of its own meanwhile.
        let (report, attached, folder) = (self.report.clone(), store.clone(), reg.root.path.clone());
        let last_checked = tokio::task::spawn_blocking(move || {
            report.activity.attach(attached.clone(), &folder);
            attached.call_blocking(move |s| s.last_checked()).ok().flatten()
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(0);
        self.state.update(|s| s.cycle.last_checked = last_checked);
        Ok(Prepared { drive, paths, store, source, tree_lock })
    }

    /// Builds the parts of a sync, linked to each other and not through the service, and
    /// starts them: the outbox worker and the watcher of a read-write folder, the listing
    /// and its poller, the sign-in nudge, the thumbnail filler. With the sync, what says
    /// when a read-write folder's watcher has walked it.
    ///
    /// Local changes are looked for in a read-write folder only (`docs/design/writes.md`
    /// §3.1): by the watcher, and once in full, for what changed while nothing watched —
    /// at every bring-up, and after a switch to read-write; the watcher's walk ends in
    /// that Full local scan, which the folder's first delta cycle waits for. A read-write
    /// folder whose watcher cannot start runs this sync locked, as a read-only one, and
    /// says so: no directory is ever made in it unwatched.
    async fn build(
        &self,
        prepared: Prepared,
        reg: &Record,
        wanted: Mode,
        reason: ScanReason,
        watcher_ended: Option<String>,
    ) -> (RunningSync, Option<watch::Receiver<crate::local::watcher::WalkState>>) {
        let Prepared { drive, paths, store, source, tree_lock } = prepared;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let stop = CancellationToken::new();
        let kept_back = Arc::new(Mutex::new(None));
        let tidying = Arc::new(Mutex::new(Vec::new()));
        // The worker asks the poller for a cycle, and the cycle wakes the worker: the
        // worker is built first, and is told its poller once that runs.
        let poll: Arc<OnceLock<PollHandle>> = Arc::new(OnceLock::new());
        let mut note = None;
        let mut first_scan = None;
        let open = if let (Mode::ReadWrite, Some(why)) = (wanted, watcher_ended) {
            // Its watcher ended by itself: no other is started until the folder is
            // brought up again or its mode is switched.
            note = Some(format!("the folder stays read-only and nothing is uploaded: local changes are no longer watched ({why})"));
            self.ensure_locked(&reg.root).await;
            None
        } else if wanted == Mode::ReadWrite {
            let outbox = self.outbox_worker(&reg.root, &store, &drive, &tree_lock, &source, Arc::clone(&kept_back), Arc::clone(&poll));
            let (scanned, scan) = watch::channel(false);
            match self.spawn_watcher(&reg.root, &store, &tree_lock, outbox.handle(), Some(scanned), reason, id) {
                Ok(watcher) => {
                    first_scan = Some(scan);
                    Some((watcher, outbox))
                }
                Err(why) => {
                    tracing::warn!("{why}; the folder stays locked");
                    note = Some(format!("the folder stays read-only and nothing is uploaded: local changes cannot be watched ({why})"));
                    self.ensure_locked(&reg.root).await;
                    None
                }
            }
        } else {
            None
        };
        let handles = open.as_ref().map(|(watcher, outbox)| (watcher.handle(), outbox.handle()));
        // The account's drive, as `config.toml` keeps it (A-M5, design §8.1):
        // the same-account check then survives a tree store rebuilt empty.
        let drive_record = {
            let persist = self.wiring.persist.clone();
            let recorded = persist.store.account(&persist.account).and_then(|a| a.drive_id).map(|drive| drive.into_string());
            Some(listing::DriveRecord { store: persist.store, account: persist.account, recorded })
        };
        // Nudges the thumbnail filler right after a cycle, rather than making
        // it wait out its own idle timer.
        let kick = Arc::new(Notify::new());
        // A read-write folder's cycle shares the tree lock with its outbox worker, and its
        // first one waits for the watcher's Full local scan (`docs/design/writes.md` §3, §9).
        let writes = handles.clone().map(|(watcher, outbox)| {
            self.cycle_writes(first_scan, &tree_lock, watcher, outbox, self.tidy_after_cycle(&reg.root, &store, &source, Arc::clone(&tidying)))
        });
        let listing = listing::Listing::new(listing::ListingContext {
            root: reg.root.clone(),
            intercepted: reg.intercepted(),
            store: store.clone(),
            drive: drive.clone(),
            drive_record,
            source,
            link: Arc::clone(&self.link),
            locks: self.locks.clone(),
            state: self.state.clone(),
            lease: listing::Lease::on(&self.folder),
            rescue_dir: paths.rescue_dir.clone(),
            full_threshold: listing::FULL_THRESHOLD,
            after_cycle: Some(Arc::clone(&kick)),
            report: self.report.clone(),
            pins: Arc::clone(&self.pins),
            locked: open.is_none(),
            writes,
            // Another account's objects are never removed here, and the
            // account hears of a drive that is not the folder's (m2).
            neighbours: Some(self.neighbours()),
            running: Arc::clone(&self.running),
        });
        let poller = listing::Poller::start(listing, self.wiring.schedule.clone());
        let _ = poll.set(poller.handle());
        let sign_in = tokio::spawn(nudge_on_sign_in(self.wiring.account.changes(), poller.handle(), stop.clone()));
        // Its own task, stopped with the poller: a slow thumbnail request
        // never holds up the reconcile. None at all without a cache to fill.
        let thumbnails = paths.thumbnails.clone().map(|cache| {
            thumbs::ThumbnailFiller::new(drive, store.clone(), reg.root.clone(), cache, Arc::clone(&self.running)).spawn(kick, stop.clone())
        });
        let walked = open.as_ref().map(|(watcher, _)| watcher.walked());
        let writes = match open {
            Some((watcher, outbox)) => {
                // The folder's first delta cycle runs before the outbox
                // (`docs/design/writes.md` §3). Rows a previous run left `running` are
                // replayed first; the rest go as the watcher's examination records them.
                outbox.wait_for_cycle(false);
                outbox.start();
                Writes::Open { watcher, outbox, lock: Arc::new(Mutex::new(Lock::Walking)) }
            }
            None => Writes::Locked { note },
        };
        let running = Handles { id, poll: poller.handle(), writes: handles, kept_back, stop, tidying };
        (RunningSync::new(running, Parts { poller, sign_in, thumbnails, writes }, self.ended.clone()), walked)
    }

    /// Why a OneDrive folder is not kept in step, said as blocking trouble:
    /// nothing retries it on its own; a `Refresh()` does, as
    /// does bringing the folder up again. The lock is put back on
    /// the folder, whatever its mode: with no sync, no watcher looks at it, so
    /// a read-write folder a run left unlocked must not stay so (the watcher re-review
    /// R2-1), and a read-only one never is.
    async fn cannot_start(&self, stopped: &mut Stopped<'_>, root: &SyncRoot, why: String) {
        tracing::error!("{why}");
        if let Some(onedrive) = stopped.folder_mut().onedrive_mut() {
            onedrive.sync = Sync::Stopped(Why::CannotStart(why));
        }
        stopped.publish();
        self.ensure_locked(root).await;
    }

    /// The activity log's clone of the store goes, once no write holds it, and so does
    /// the walker measuring the folder — a kick starts
    /// it again. Only where no sync can start meanwhile: inside a change of the folder's
    /// state.
    pub(super) async fn let_go_of_activity(&self) {
        self.report.space.stop();
        let report = self.report.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || report.activity.detach()).await {
            tracing::warn!("the task letting go of the activity failed: {e}");
        }
    }

    /// `Refresh()`: a cycle now, for a folder that shows OneDrive.
    ///
    /// A folder whose sync is not running — it could not start: its tree
    /// store could not be opened (F18) — has it started again here, the way
    /// every start is made, inside a change of the folder's state. `Ok` means a
    /// sync runs; when it still cannot, the refusal says why.
    ///
    /// A folder that is down because a bring-up failed, because the folder itself was moved
    /// or deleted, or because a registration was kept, is brought up here: this is the way
    /// back that does not wait for the helper's next connect. When that fails too, and for
    /// a folder that only waits (for the helper, or for a `source` that can be read), the
    /// refusal is `NotUp`, with why.
    ///
    /// Refused `NoHelper` whenever the folder has no helper to keep it in
    /// step with (HS2): no link, or no interception yet.
    pub async fn refresh(&self) -> Result<(), SyncError> {
        if self.view().down.is_none() {
            self.require_helper_for(&self.require_onedrive()?)?;
            // The outbox too (`docs/design/writes.md` §11): rows in backoff go now,
            // and, while a sync runs, the quota is read again, which may end a
            // full OneDrive (issue #2).
            self.retry_outbox();
            if self.nudge() {
                self.refresh_quota().await;
                return Ok(());
            }
        }
        let syncs = {
            let mut stopped = self.change().await;
            let refreshed = self.refresh_in(&mut stopped).await;
            if refreshed.is_err() {
                // A refusal leaves the folder as it was, its sync included.
                self.start_again(&mut stopped).await;
            }
            refreshed?
        };
        if !syncs {
            return Ok(());
        }
        if self.running().is_some() {
            self.refresh_quota().await;
            return Ok(());
        }
        Err(self.sync_not_running())
    }

    /// [`refresh`](Self::refresh), inside its change: the folder looked at again (a Forget
    /// may have come first), brought up if it is down for a failure, and its sync started.
    /// Whether the folder is one that syncs; a local folder brought up has nothing more to
    /// refresh.
    async fn refresh_in(&self, stopped: &mut Stopped<'_>) -> Result<bool, SyncError> {
        if matches!(stopped.folder().standing, Standing::HeldBack(_)) {
            return Err(SyncError::NoRoot);
        }
        let retry = match &stopped.folder().is {
            Is::Absent => return Err(SyncError::NoRoot),
            Is::Down(record, Down::Failed { .. } | Down::Kept { .. }) => {
                if record.intercepted() && self.link().is_none() {
                    return Err(SyncError::NoHelper);
                }
                true
            }
            Is::Down(..) | Is::Up(_) => false,
        };
        if retry {
            self.bring_up(stopped).await;
        }
        let record = match &stopped.folder().is {
            Is::Absent => return Err(SyncError::NoRoot),
            Is::Down(record, down) => {
                if !retry {
                    if record.source != RootSource::OneDrive {
                        return Err(SyncError::Unsupported("this folder is not connected to OneDrive".into()));
                    }
                    self.require_helper_for(record)?;
                }
                let why = down.waits_for().map_or_else(|| down.why(&record.root), str::to_owned);
                return Err(if retry {
                    SyncError::not_up(&format!("bringing it up was tried just now and failed: {why}"))
                } else {
                    SyncError::not_up(&why)
                });
            }
            Is::Up(up) => up.record.clone(),
        };
        if record.source != RootSource::OneDrive {
            return if retry { Ok(false) } else { Err(SyncError::Unsupported("this folder is not connected to OneDrive".into())) };
        }
        self.require_helper_for(&record)?;
        // A bring-up has started it already.
        if stopped.folder().running().is_none() {
            self.start_sync(stopped, ScanReason::Start).await;
        }
        Ok(true)
    }

    /// `Refresh()`'s part for the quota (issue #2): read now, one request, into the account's
    /// one quota (`Account.QuotaRemaining`, `QuotaState`, …), and handed to the outbox, which
    /// ends a full OneDrive and lets the files that fit now go. A quota that cannot be read
    /// changes nothing.
    async fn refresh_quota(&self) {
        let Some(drive) = self.drive() else { return };
        match drive.quota().await {
            Ok(quota) if crate::upload::space::known(&quota) => {
                self.quota().read(&quota);
                self.quota_seen(&quota);
            }
            Ok(_) => tracing::warn!("OneDrive gave no quota"),
            Err(e) => tracing::warn!("cannot read the OneDrive quota: {e}"),
        }
    }

    /// A cycle now, if a OneDrive folder is syncing (the network came back). A read-write
    /// folder's outbox waits for that cycle (`docs/design/writes.md` §9).
    pub fn refresh_now(&self) {
        self.outbox_after_network();
        self.nudge();
    }

    /// [`refresh_now`](Self::refresh_now); whether a sync was running to
    /// nudge.
    pub(super) fn nudge(&self) -> bool {
        match self.running() {
            Some(running) => {
                running.poll.refresh();
                // The notification socket too: closed while stopped, opened again when not,
                // tried again at once when the network came back (`live`).
                running.poll.wake_live();
                true
            }
            None => false,
        }
    }

    /// A cycle now with a Full reconcile, which places again what is missing
    /// here (`RestoreDeletes`, a forgotten local object). Whether a sync runs.
    pub(super) fn nudge_full(&self) -> bool {
        match self.running() {
            Some(running) => {
                running.poll.refresh_full();
                true
            }
            None => false,
        }
    }
}

/// Opens the tree store of the folder: the one on disk, made if there is none.
pub(super) async fn open_store(paths: &SyncPaths) -> Result<Store, String> {
    let tree_db = paths.tree_db.clone();
    match tokio::task::spawn_blocking(move || konedrive_tree::TreeStore::open(&tree_db)).await {
        Ok(Ok(store)) => Ok(Store::new(store)),
        Ok(Err(e)) => Err(format!("the tree store cannot be opened: {e}")),
        Err(e) => Err(format!("the tree store cannot be opened: {e}")),
    }
}

/// Asks a OneDrive folder's sync for a cycle each time the account becomes
/// signed in from any other state. A cycle that finds the account signed out
/// fails as blocking trouble and is retried only on the poller's schedule, so
/// without this a folder reading "signed out" would keep saying so for up to
/// a poll interval after the sign-in. Runs as long as the sync does (`stop`).
async fn nudge_on_sign_in(mut account: watch::Receiver<crate::account::state::AccountSnapshot>, poll: PollHandle, stop: CancellationToken) {
    let mut last = account.borrow_and_update().state;
    loop {
        tokio::select! {
            changed = account.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = stop.cancelled() => return,
        }
        let now = account.borrow_and_update().state;
        if now == SignInState::SignedIn && last != SignInState::SignedIn {
            poll.refresh();
        }
        last = now;
    }
}
