use std::sync::{Arc, Mutex};

use tokio::sync::{watch, Notify};
use tokio_util::sync::CancellationToken;

use crate::sync::SyncService;
use crate::folder::root::SyncRoot;
use crate::hydration::source::ContentSource;
use crate::config::Mode;
use crate::account::state::SignInState;
use crate::local::ScanReason;
use crate::sync::folder::{Down, Is, Standing, Stopped};
use crate::sync::{RootSource, SyncError, Syncing};
use crate::status::snapshot::SyncTrouble;
use crate::desktop::thumbs;
use crate::remote::listing;

impl SyncService {
    /// Starts the sync of the OneDrive folder that is up now. Inside a change of the
    /// folder's state, so that no Forget can come in between, and none runs: the change
    /// stopped it. `reason`: why the watcher of a read-write folder runs its Full local
    /// scan (`LocalScan.Reason`).
    pub(super) async fn start_sync(&self, stopped: &mut Stopped<'_>, reason: ScanReason) {
        if !stopped.folder().syncs() {
            return;
        }
        debug_assert!(self.syncing.lock().unwrap().is_none(), "a sync runs inside a change");
        // What the parts started here read of the folder is what it is now.
        stopped.publish();
        let Some(reg) = stopped.folder().up().map(|up| up.record.clone()) else { return };
        let wanted = stopped.folder().wanted;
        let Some(crate::sync::OneDrive { drive, paths }) = self.wiring.onedrive.clone() else {
            let text = format!("{} shows OneDrive, but no drive is configured; it is not kept in step", reg.root.path.display());
            return self.cannot_start(&reg.root, text).await;
        };
        // The lock as the mode wants it (`docs/design/writes.md` §2.2): a walk a switch did not finish —
        // the daemon stopped, or the folder was not up — is finished here. A read-only folder
        // is locked before anything else, never left writable until a Full reconcile, which
        // needs Graph. A read-write one is unlocked below, once its watcher has
        // marked every directory, and before this sync's first cycle can change the folder:
        // the cycle takes the folder's lease, which the caller's change holds (the watcher).
        let mut writable = wanted == Mode::ReadWrite;
        if !writable {
            self.ensure_locked(&reg.root).await;
        }
        // Files are downloaded from the drive whether or not the folder can
        // be kept in step.
        let source: Arc<dyn ContentSource> = self.wiring.sources.onedrive(&drive);
        *self.source.lock().unwrap() = Some(Arc::clone(&source));
        let tree_db = paths.tree_db.clone();
        let store = match tokio::task::spawn_blocking(move || konedrive_tree::TreeStore::open(&tree_db)).await {
            Ok(Ok(store)) => konedrive_tree::Store::new(store),
            Ok(Err(e)) => return self.cannot_start(&reg.root, format!("the tree store cannot be opened: {e}")).await,
            Err(e) => return self.cannot_start(&reg.root, format!("the tree store cannot be opened: {e}")).await,
        };
        // A read-only folder that still holds changes waiting to upload (a switch nobody
        // forced) runs no cycle while they wait: what a read-write cycle
        // deferred stays deferred for the read-write cycle that sends them.
        let waiting = !writable && store.call(|s| s.outbox_len()).await.map_or(true, |n| n > 0);
        if !writable && !waiting {
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
        self.state.update(|s| s.last_checked = last_checked);
        // Local changes are looked for in a read-write folder only (`docs/design/writes.md` §3.1): from
        // now on by the watcher, and once in full, for what changed while nothing watched —
        // at every bring-up, and after a switch to read-write; the watcher's walk ends in that
        // Full local scan. Started before the sync is published, and kept in it: whoever
        // stops the sync stops it. A read-write folder whose watcher cannot start
        // runs this sync locked, as a read-only one: no directory is ever made in it unwatched
        // (the watcher).
        // Its first examination is the Full local scan, which the folder's first delta cycle
        // waits for (`docs/design/writes.md` §3).
        let (scanned, first_scan) = tokio::sync::watch::channel(false);
        let watcher = if writable { self.start_watcher(&reg.root, &store, Some(scanned), reason) } else { None };
        if writable && watcher.is_none() {
            writable = false;
            self.ensure_locked(&reg.root).await;
        }
        // The account's drive, as `config.toml` keeps it (A-M5, design §8.1):
        // the same-account check then survives a tree store rebuilt empty.
        let drive_record = {
            let persist = self.wiring.persist.clone();
            let recorded = persist.store.account(&persist.account).map(|a| a.drive_id).filter(|d| !d.is_empty());
            Some(listing::DriveRecord { store: persist.store, account: persist.account, recorded })
        };
        // Nudges the thumbnail filler right after a cycle, rather than making
        // it wait out its own idle timer.
        let kick = Arc::new(Notify::new());
        // A read-write folder's cycle shares the tree lock with its outbox worker, and its
        // first one waits for the watcher's Full local scan (`docs/design/writes.md` §3, §9).
        let writes = writable.then(|| self.cycle_writes(Some(first_scan)));
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
            locked: !writable,
            writes,
            // Another account's objects are never removed here, and the
            // account hears of a drive that is not the folder's (m2).
            neighbours: Some(self.neighbours()),
            running: Arc::clone(&self.running),
        });
        let schedule = self.wiring.schedule.clone();
        // Published in one critical section. No sync runs here: the change this start is
        // inside stopped it, and nothing starts one outside a change.
        let walked = {
            let mut syncing = self.syncing.lock().unwrap();
            *self.store.lock().unwrap() = Some(store.clone());
            let poller = listing::Poller::start(listing, schedule);
            let sign_in_watch = Some(tokio::spawn(nudge_on_sign_in(self.wiring.account.changes(), Arc::clone(&self.syncing))));
            // The outbox worker: it sends the rows the watcher's
            // examination records, and looks at those already there as it
            // starts.
            let outbox = if writable { self.start_outbox(&reg.root, &store, &drive) } else { None };
            // Its own task, stopped with the poller: a slow thumbnail request
            // never holds up the reconcile. None at all without a cache to fill.
            let thumbnails = paths.thumbnails.clone().map(|cache| {
                let cancel = CancellationToken::new();
                let task = thumbs::ThumbnailFiller::new(drive, store, reg.root.clone(), cache, Arc::clone(&self.running))
                    .spawn(kick, cancel.clone());
                (task, cancel)
            });
            let walked = watcher.as_ref().map(crate::local::watcher::Watcher::walked);
            *syncing = Some(Syncing { poller, sign_in_watch, thumbnails, watcher, outbox });
            walked
        };
        // `Paused` as the store keeps it, and a timer for a pause that ends.
        self.show_pause();
        // No directory is made in the folder before it is watched (write design Z2).
        if let Some(walked) = walked {
            self.ensure_unlocked(&reg.root, walked).await;
        }
    }

    /// Why a OneDrive folder is not kept in step, said as blocking trouble:
    /// nothing retries it on its own; a `Refresh()` does, as
    /// does bringing the folder up again.
    /// [`sync_cannot_start`](Self::sync_cannot_start), and the lock put back on
    /// the folder, whatever its mode: with no sync, no watcher looks at it, so
    /// a read-write folder a run left unlocked must not stay so (the watcher re-review
    /// R2-1), and a read-only one never is.
    async fn cannot_start(&self, root: &SyncRoot, text: String) {
        self.sync_cannot_start(text);
        self.ensure_locked(root).await;
    }

    fn sync_cannot_start(&self, text: String) {
        tracing::error!("{text}");
        self.state.update(|s| s.sync_trouble = Some(SyncTrouble { text, blocking: true }));
    }

    /// Stops the sync and waits for it: a Forget's, and tests'. (At the
    /// daemon's stop only the outbox is wound down, [`close_outbox`]; the
    /// rest ends with the process.)
    ///
    /// [`close_outbox`]: Self::close_outbox
    /// Whether one was running. Once this returns, no clone of the tree store
    /// is left with the sync (see `store`).
    #[cfg(any(test, feature = "fault-injection"))]
    pub async fn stop_sync(&self) -> bool {
        let stopped = self.stop_tasks().await;
        if stopped {
            self.let_go_of_activity().await;
        }
        stopped
    }

    /// The first half of [`stop_sync`](Self::stop_sync): the poller, the
    /// sign-in watch and the thumbnail filler, stopped and waited for.
    /// Safe without the state lock ([`change`](Self::change)'s first stop).
    pub(super) async fn stop_tasks(&self) -> bool {
        let syncing = self.syncing.lock().unwrap().take();
        let Some(syncing) = syncing else { return false };
        // Told before the poller is waited for, so that both wind down at
        // once.
        if let Some((_, cancel)) = &syncing.thumbnails {
            cancel.cancel();
        }
        // The poller first (the read-write reconcile): a cycle may hold the tree lock while it waits for
        // the folder's state, which the caller may hold for writing, and the watcher's examination
        // waits for that tree lock — stopping the poller ends that cycle, and its lock with it.
        syncing.poller.stop().await;
        // Its outbox worker next: a request under way is cut off, and its row replayed when
        // the worker starts again (`docs/design/writes.md` §10) — and a commit it holds the tree lock for,
        // waiting on a file a fill holds, does not keep the examination below waiting for as
        // long as that download (the read-write reconcile).
        if let Some(outbox) = syncing.outbox {
            outbox.stop().await;
            // Its counts go with it (the outbox on the bus); the next worker counts again.
            self.clear_outbox_counts();
        }
        // A read-write folder's watcher goes with its sync (`docs/design/writes.md` §3): this call took
        // the sync, so it stops the watcher that came with it, and no other call can.
        if let Some(watcher) = syncing.watcher {
            self.stop_watcher(watcher).await;
        }
        if let Some(watch) = syncing.sign_in_watch {
            watch.abort();
        }
        if let Some((task, _)) = syncing.thumbnails {
            let _ = task.await;
        }
        true
    }

    /// The second half of [`stop_sync`](Self::stop_sync): the activity
    /// log's clone of the store goes too, once no write holds it (see
    /// `store`), and so does the walker measuring the folder — a kick starts
    /// it again. Only where no sync can start meanwhile — inside a change of the folder's
    /// state, or where nothing else starts one: done outside
    /// it, a Forget's first stop let go of whatever was attached by then,
    /// the sync a reconnect had just started included.
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
        if self.syncing.lock().unwrap().is_some() {
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
        if self.syncing.lock().unwrap().is_none() {
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
        match self.syncing.lock().unwrap().as_ref() {
            Some(syncing) => {
                syncing.poller.refresh();
                // The notification socket too: closed while stopped, opened again when not,
                // tried again at once when the network came back (`live`).
                syncing.poller.wake_live();
                true
            }
            None => false,
        }
    }

    /// A cycle now with a Full reconcile, which places again what is missing
    /// here (`RestoreDeletes`, a forgotten local object). Whether a sync runs.
    pub(super) fn nudge_full(&self) -> bool {
        match self.syncing.lock().unwrap().as_ref() {
            Some(syncing) => {
                syncing.poller.refresh_full();
                true
            }
            None => false,
        }
    }
}

/// Asks a OneDrive folder's sync for a cycle each time the account becomes
/// signed in from any other state. A cycle that finds the account signed out
/// fails as blocking trouble and is retried only on the poller's schedule, so
/// without this a folder reading "signed out" would keep saying so for up to
/// a poll interval after the sign-in. Runs as long as the sync does.
async fn nudge_on_sign_in(
    mut account: watch::Receiver<crate::account::state::AccountSnapshot>,
    syncing: Arc<Mutex<Option<Syncing>>>,
) {
    let mut last = account.borrow_and_update().state;
    while account.changed().await.is_ok() {
        let now = account.borrow_and_update().state;
        if now == SignInState::SignedIn && last != SignInState::SignedIn {
            if let Some(syncing) = syncing.lock().unwrap().as_ref() {
                syncing.poller.refresh();
            }
        }
        last = now;
    }
}
