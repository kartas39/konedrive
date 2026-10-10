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
use crate::fuse::Mount;
use crate::remote::listing::{self, PollHandle};
use crate::sync::folder::{Down, Is, Record, Standing, Stopped};
use crate::sync::running_sync::{Handles, Parts, RunningSync, Sync, Why};
use crate::sync::{SyncError, SyncPaths, SyncService};

/// What a sync is built on, once [`SyncService::prepare`] has made the folder ready.
struct Prepared {
    drive: DriveClient,
    paths: SyncPaths,
    store: Store,
    tree_lock: Arc<tokio::sync::Mutex<()>>,
    mount: Arc<Mount>,
}

impl SyncService {
    /// Starts the sync of the folder that is up now; nothing for a folder that is not up. Inside a change of the
    /// folder's state, so that no Forget can come in between, and none runs: the change
    /// stopped it.
    ///
    /// Three steps: the folder is made ready ([`prepare`](Self::prepare)), the parts are
    /// built and linked to each other ([`build`](Self::build)), and they are started and
    /// put into the state, from where only a change takes them out.
    ///
    /// A read-write folder's mount takes changes from here on, and wakes the sync's outbox
    /// worker for each one it records; a read-only folder's takes none.
    pub(super) async fn start_sync(&self, stopped: &mut Stopped<'_>) {
        debug_assert!(stopped.folder().running().is_none(), "a sync runs inside a change");
        // What the parts started here read of the folder is what it is now.
        stopped.publish();
        let Some(reg) = stopped.folder().up().map(|up| up.record.clone()) else { return };
        let wanted = stopped.folder().wanted;
        let prepared = match self.prepare(stopped, &reg).await {
            Ok(prepared) => prepared,
            Err(why) => return self.cannot_start(stopped, why),
        };
        // Before any part that asks about the pause is started.
        self.take_old_pause(&prepared.store).await;
        let mount = Arc::clone(&prepared.mount);
        let sync = self.build(prepared, &reg, wanted);
        match sync.handles().outbox().cloned() {
            // At once, and not waited for behind the tree lock: the first cycle of the sync
            // just built may hold it while it waits for the folder's state, which this change
            // holds.
            Some(outbox) => mount.switch_to_read_write(Arc::new(move || outbox.wake())),
            // The change that leads here made the switch already. A cycle of a read-only
            // folder takes the tree lock only once it has the folder's lease, which this
            // change holds: none holds the lock the write-operation thread waits for.
            None => mount.switch_to_read_only().await,
        };
        if let Some(running) = stopped.folder_mut().sync_mut() {
            *running = Sync::Running(Box::new(sync));
        }
        // The readers reach the sync from here on.
        stopped.publish();
        // `Paused` as the account has it, and a timer for a pause that ends.
        self.show_pause();
    }

    /// Makes the folder ready for a sync: the tree store open, the folder mounted, and the
    /// activity log kept in the store. Why not, when the sync cannot start. What a
    /// read-write cycle deferred is staged by the first cycle of a read-only folder, which
    /// runs once no change waits to upload.
    async fn prepare(&self, stopped: &mut Stopped<'_>, reg: &Record) -> Result<Prepared, String> {
        let Some(crate::sync::OneDrive { drive, paths }) = self.wiring.onedrive.clone() else {
            return Err(format!("{} shows OneDrive, but no drive is configured; it is not kept in step", reg.root.path.display()));
        };
        let Some(source) = reg.kept.source.clone() else {
            return Err(format!("{} shows OneDrive, but no drive is configured; it is not kept in step", reg.root.path.display()));
        };
        let tree_lock = Arc::clone(&reg.kept.tree_lock);
        // The folder's store, the one connection to it: open already, or opened now, made
        // if there is none, and kept with the folder until it is forgotten.
        let store = self.kept_store(stopped, &paths).await?;
        // The folder is mounted from its first sync on, once its store is there to read the
        // names from.
        let mount = match stopped.folder().mount().cloned() {
            Some(mount) => mount,
            None => {
                let setup = crate::fuse::MountSetup {
                    root: reg.root.clone(),
                    store_path: paths.tree_db.clone(),
                    store: store.clone(),
                    source: Arc::clone(&source),
                    report: self.report.clone(),
                    pool: Arc::clone(&self.pool),
                    split_download_share: Arc::clone(&self.parts),
                    download_after: self.wiring.transfers.download_after,
                    write_setup: crate::fuse::WriteSetup { tree_lock: Arc::clone(&tree_lock), ignore: Arc::clone(&self.ignore), machine_name: self.machine_name() },
                };
                let mount = Mount::start(setup).await.map_err(|e| format!("{} cannot be mounted: {e}", reg.root.path.display()))?;
                if let Some(record) = stopped.folder_mut().record_mut() {
                    record.kept.mount = Some(Arc::clone(&mount));
                }
                self.show_pins(&mount);
                // A file manager that showed the folder before this mount reads it again: its
                // own watch ended with the mount that went.
                stopped.read_again();
                self.report.space.measure_at(Some(mount.backing_directory().resolve_root_path()));
                mount
            }
        };
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
        Ok(Prepared { drive, paths, store, tree_lock, mount })
    }

    /// Builds the parts of a sync, linked to each other and not through the service, and
    /// starts them: the outbox worker of a read-write folder, the listing and its poller,
    /// the sign-in nudge, the thumbnail filler.
    ///
    /// Local changes need no looking for: the folder's mount records each one as a program
    /// makes it (`fuse::outbox_recorder`), and the worker sends what is recorded.
    fn build(&self, prepared: Prepared, reg: &Record, wanted: Mode) -> RunningSync {
        let Prepared { drive, paths, store, tree_lock, mount } = prepared;
        let stop = CancellationToken::new();
        let kept_back = Arc::new(Mutex::new(None));
        // The worker asks the poller for a cycle, and the cycle wakes the worker: the
        // worker is built first, and is told its poller once that runs.
        let poll: Arc<OnceLock<PollHandle>> = Arc::new(OnceLock::new());
        let outbox = (wanted == Mode::ReadWrite).then(|| self.outbox_worker(&reg.root, &store, &drive, &tree_lock, Arc::clone(&kept_back), Arc::clone(&poll)));
        // The account's drive, as `config.toml` keeps it (`docs/design/accounts.md` §6.1):
        // the same-account check then survives a tree store rebuilt empty.
        let drive_record = {
            let persist = self.wiring.persist.clone();
            let recorded = persist.store.account(&persist.account).and_then(|a| a.drive_id).map(|drive| drive.into_string());
            Some(listing::DriveRecord { store: persist.store, account: persist.account, recorded })
        };
        // Nudges the thumbnail filler right after a cycle, rather than making
        // it wait out its own idle timer.
        let kick = Arc::new(Notify::new());
        // A read-write folder's cycle shares the tree lock with its outbox worker and with
        // the mount's write-operation thread (`docs/design/writes.md` §9); a read-only
        // folder's cycle shares it with that thread, for its reconcile.
        let writes = outbox.as_ref().map(|outbox| self.cycle_writes(&tree_lock, outbox.handle()));
        let listing = listing::Listing::new(listing::ListingContext {
            root: reg.root.clone(),
            store: store.clone(),
            drive: drive.clone(),
            drive_record,
            mount,
            state: self.state.clone(),
            lease: listing::Lease::on(&self.folder),
            after_cycle: Some(Arc::clone(&kick)),
            report: self.report.clone(),
            mode: match writes {
                Some(writes) => crate::remote::mode::Mode::ReadWrite(writes),
                None => crate::remote::mode::Mode::ReadOnly,
            },
            tree_lock: Arc::clone(&tree_lock),
            // The account hears of a drive that is not the folder's (m2).
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
        if let Some(outbox) = &outbox {
            // The folder's first delta cycle runs before the outbox
            // (`docs/design/writes.md` §9). Rows a previous run left `running` are
            // replayed first; the rest go as the mount records them.
            outbox.wait_for_cycle(false);
            outbox.start();
        }
        let running = Handles { poll: poller.handle(), outbox: outbox.as_ref().map(|outbox| outbox.handle()), kept_back, stop };
        RunningSync::new(running, Parts { poller, sign_in, thumbnails, outbox }, self.ended.clone())
    }

    /// Why a OneDrive folder is not kept in step, said as blocking trouble:
    /// nothing retries it on its own; a `Refresh()` does, as
    /// does bringing the folder up again.
    fn cannot_start(&self, stopped: &mut Stopped<'_>, why: String) {
        tracing::error!("{why}");
        if let Some(sync) = stopped.folder_mut().sync_mut() {
            *sync = Sync::Stopped(Why::CannotStart(why));
        }
        stopped.publish();
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

    /// `Refresh()`: a cycle now.
    ///
    /// A folder whose sync is not running — it could not start: its tree
    /// store could not be opened, or it could not be mounted — has it started again here,
    /// the way every start is made, inside a change of the folder's state. `Ok` means a
    /// sync runs; when it still cannot, the refusal says why.
    ///
    /// A folder that is down because a bring-up failed is brought up here: this is the way
    /// back that does not wait for the next start. When that fails too, and for a folder
    /// that only waits (for a `source` that can be read), the refusal is `NotUp`, with why.
    pub async fn refresh(&self) -> Result<(), SyncError> {
        if self.view().down.is_none() {
            self.require_record()?;
            // The outbox too (`docs/design/writes.md` §6.4): rows in backoff go now,
            // and, while a sync runs, the quota is read again, which may end a
            // full OneDrive.
            self.retry_outbox();
            if self.nudge() {
                self.refresh_quota().await;
                return Ok(());
            }
        }
        {
            let mut stopped = self.change().await;
            let refreshed = self.refresh_in(&mut stopped).await;
            if refreshed.is_err() {
                // A refusal leaves the folder as it was, its sync included.
                self.start_again(&mut stopped).await;
            }
            refreshed?;
        }
        if self.running().is_some() {
            self.refresh_quota().await;
            return Ok(());
        }
        Err(self.sync_not_running())
    }

    /// [`refresh`](Self::refresh), inside its change: the folder looked at again (a Forget
    /// may have come first), brought up if it is down for a failure, and its sync started.
    async fn refresh_in(&self, stopped: &mut Stopped<'_>) -> Result<(), SyncError> {
        if matches!(stopped.folder().standing, Standing::HeldBack(_)) {
            return Err(SyncError::NoRoot);
        }
        let retry = match &stopped.folder().is {
            Is::Absent => return Err(SyncError::NoRoot),
            Is::Down(_, Down::Failed { .. }) => true,
            Is::Down(..) | Is::Up(_) => false,
        };
        if retry {
            self.bring_up(stopped).await;
        }
        match &stopped.folder().is {
            Is::Absent => return Err(SyncError::NoRoot),
            Is::Down(record, down) => {
                let why = down.waits_for().map_or_else(|| down.why(&record.root), str::to_owned);
                return Err(if retry {
                    SyncError::not_up(&format!("bringing it up was tried just now and failed: {why}"))
                } else {
                    SyncError::not_up(&why)
                });
            }
            Is::Up(_) => {}
        }
        // A bring-up has started it already.
        if stopped.folder().running().is_none() {
            self.start_sync(stopped).await;
        }
        Ok(())
    }

    /// `Refresh()`'s part for the quota: read now, one request, into the account's
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
