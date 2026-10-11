//! A OneDrive folder's sync started ([`SyncService::start_sync`]) and asked for a cycle
//! (`Refresh()`, the network back). How it starts and stops is `crate::sync_tasks`'.

use std::sync::Arc;

use konedrive_tree::StoreOrigin;

use crate::account_folder::folder::{Down, Is, Standing, Stopped, Sync, SyncChange, TakeDownTry, Why};
use crate::account_folder::mode::decide_account_holds;
use crate::account_folder::pause::decide_paused_until;
use crate::account_folder::take_down::decide_retry_wait;
use crate::account_folder::{SyncError, SyncService};
use crate::config::DriveId;
use crate::listing::listing_cycle::DriveRecord;
use crate::onedrive_steps::content_upload::Limits;
use crate::sync_parts::lease::Lease;
use crate::sync_parts::sync_plan_wake::WakeCause;
use crate::sync_plan_task::plan_input::PlanSettings;
use crate::sync_tasks::start_order::{start_folder_sync, FolderStartInput, StandingSendingHolds, StartRefusal};

impl SyncService {
    /// Starts the sync of the folder that is up now; nothing for a folder that is not up. Inside a change of the
    /// folder's state, so that no Forget can come in between, and none runs: the change
    /// stopped it.
    ///
    /// The start itself is `sync_tasks::start_order::start_folder_sync`'s, stage by stage; what
    /// it answers is put into the state, from where only a change takes it out.
    pub(super) async fn start_sync(&self, stopped: &mut Stopped<'_>) {
        debug_assert!(stopped.folder().running().is_none(), "a sync runs inside a change");
        // The daemon's end reached the account: its folder's mount is stopped, or about to be, and nothing mounts it again.
        if self.is_ended_at_daemon_stop.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        // What a start reads of the folder is what it is now.
        stopped.publish();
        let Some(up) = stopped.folder().up() else { return };
        let (root, source, tree_lock) = (up.record.root.clone(), up.record.kept.source.clone(), Arc::clone(&up.record.kept.tree_lock));
        let wanted = stopped.folder().wanted;
        let (Some(me), Some(crate::account_folder::OneDrive { drive, paths }), Some(source)) = (self.me.upgrade(), self.wiring.onedrive.clone(), source) else {
            let why = format!("{} shows OneDrive, but no drive is configured; it is not kept in step", root.path.display());
            return self.refuse_start(stopped, StartRefusal::Mount(why), &root);
        };
        // What stands of the account now: its sign-in and whether it may write, by the function the watcher of the
        // account sets them by afterwards; `config.toml` is read off the runtime.
        let snapshot = self.wiring.account.snapshot();
        let persist = self.wiring.persist.clone();
        let standing = tokio::task::spawn_blocking(move || persist.store.write_standing(&persist.account)).await.ok().flatten();
        let account_holds = decide_account_holds(&snapshot, standing.as_ref());
        let standing_holds = StandingSendingHolds {
            paused_until: decide_paused_until(self.running.stop()),
            is_signed_out: account_holds.is_signed_out,
            writes_refusal: account_holds.writes_refusal,
        };
        let persist = self.wiring.persist.clone();
        let recorded_drive = persist.store.account(&persist.account).and_then(|account| account.drive_id);
        let drive_record = DriveRecord { store: persist.store, account: persist.account, recorded: recorded_drive.clone().map(|drive| drive.into_string()) };
        let (clock, account) = (Arc::clone(&self.wiring.clock), Arc::clone(&self.wiring.account));
        // The parts the stopped sync kept, whole: the start has them, or gives them back.
        let kept = stopped.folder_mut().record_mut().and_then(|record| record.kept.parts.take());
        let is_mount_kept = kept.is_some();
        let input = FolderStartInput {
            root: root.clone(),
            tree_db: paths.tree_db,
            drive,
            recorded_drive,
            // The drive the account's token was last seen to reach (`AccountSnapshot::live_drive`: `GET /me/drive` at a sign-in), none before the first sign-in: what a start falls back to when neither the store nor `config.toml` names a drive (`decide_check_drive`).
            account_drive: DriveId::new(snapshot.live_drive.clone()),
            drive_record: Some(drive_record),
            neighbours: Some(self.neighbours()),
            source,
            report: self.report.clone(),
            pool: Arc::clone(&self.pool),
            split_download_share: Arc::clone(&self.parts),
            download_after: self.wiring.transfers.download_after,
            tree_lock,
            lease: Lease::on(&self.folder),
            ignore: Arc::clone(&self.ignore),
            state: self.state.clone(),
            quota: self.quota(),
            limits: Limits::default(),
            settings: PlanSettings::of(self.machine_name()),
            schedule: self.wiring.schedule.clone(),
            running: Arc::clone(&self.running),
            thumbnail_cache: paths.thumbnails,
            standing_holds,
            clock: Arc::new(move || clock.now()),
            wanted,
            take_down_report: me.take_down_report(),
            unsent_count_report: Arc::new(move |unsent| account.tell_unsent_count(unsent)),
            kept,
        };
        // In a task of its own: a start is never cut between two of its stages.
        let start = async move {
            match start_folder_sync(input).await {
                Ok(started) => SyncChange::Started(Box::new(started)),
                Err(refused) => SyncChange::Refused(refused),
            }
        };
        let started = match self.sync_change.run(start, self.adopt_left_sync_change()).await {
            Some(SyncChange::Started(started)) => started,
            Some(SyncChange::Refused(refused)) => {
                // The store was made anew before the start was refused: the next start finds it so, and says nothing.
                if let Some(loss) = &refused.rebuild_loss {
                    self.tell_rebuild_loss(&root.path, loss).await;
                }
                if let Some(record) = stopped.folder_mut().record_mut() {
                    record.kept.parts = refused.kept;
                }
                return self.refuse_start(stopped, refused.refusal, &root);
            }
            // A stop answers no start; and a start that panicked answers nothing.
            Some(SyncChange::Stopped(_)) | None => {
                let why = "the start failed inside konedrive".to_owned();
                return self.refuse_start(stopped, StartRefusal::Mount(why), &root);
            }
        };
        if let StoreOrigin::MadeEmpty { loss: Some(loss) } = &started.store_origin {
            self.tell_rebuild_loss(&root.path, loss).await;
        }
        let mount = Arc::clone(&started.mount);
        // The pause as it is now, set again: one that began or ended while the start ran reached no sync.
        started.tasks.sending_holds().set_paused(decide_paused_until(self.running.stop()));
        match stopped.folder_mut().sync_mut() {
            Some(sync) => *sync = Sync::Running(started),
            // Nothing is up to run it: the start's caller looked a moment ago, under this lock.
            None => return,
        }
        // The readers reach the sync from here on.
        stopped.publish();
        if !is_mount_kept {
            // The mount came with this start: the folder's space is measured in the directory under it, and a file
            // manager that showed the folder reads it again, in either access mode.
            self.report.space.measure_at(Some(mount.backing_directory().resolve_root_path()));
            self.report.space.kick();
            stopped.read_again();
        }
        self.show_pins(&mount).await;
        // `Paused` as the account has it, and a timer for a pause that ends.
        self.show_pause();
    }

    /// Tells the user, once, what a start whose store was made anew lost with the old one:
    /// in the journal, and as an event of the activity log ([`make_rebuild_loss_event`]:
    /// `ActivityLog.Added` and `Recent`, so the window's activity list and
    /// `konedrivectl sync activity`). Nothing when nothing the user misses was lost. An
    /// event recorded while the log has no store (the start was refused) is kept in
    /// memory and written when a store is attached.
    async fn tell_rebuild_loss(&self, folder: &std::path::Path, loss: &konedrive_tree::RebuildLoss) {
        let Some(event) = Self::make_rebuild_loss_event(folder, loss) else { return };
        tracing::warn!("{}: {}", folder.display(), event.detail);
        self.report.activity.record(vec![event]).await;
    }

    /// The event of the activity log for a store made anew that lost something
    /// (`ActivityKind::StoreRebuilt`): the folder's path, and what was lost as its detail
    /// ([`describe_rebuild_loss`](Self::describe_rebuild_loss)); `None` when nothing the
    /// user misses was lost.
    fn make_rebuild_loss_event(folder: &std::path::Path, loss: &konedrive_tree::RebuildLoss) -> Option<crate::status::activity::Event> {
        let said = Self::describe_rebuild_loss(loss)?;
        Some(crate::status::activity::event(konedrive_tree::ActivityKind::StoreRebuilt, folder.display().to_string(), said))
    }

    /// What the user is told once after a start whose store was rebuilt and lost
    /// something (`RebuildLoss`: pins, unsent changes that are not on disk, open upload
    /// sessions): `None` when nothing was lost. Pure.
    fn describe_rebuild_loss(loss: &konedrive_tree::RebuildLoss) -> Option<String> {
        let lost: Vec<String> = [
            (loss.pins, "pin(s)"),
            (loss.unsent_changes_not_on_disk, "change(s) that were not sent to OneDrive and are not on disk"),
            (loss.open_upload_sessions, "upload(s) that were under way"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, what)| format!("{count} {what}"))
        .collect();
        // TODO: wording
        (!lost.is_empty()).then(|| format!("konedrive's records of this folder were of an older version and were made anew; lost with them: {}", lost.join(", ")))
    }

    /// A start of the folder at `root` was refused: why its sync does not run, written into the state
    /// ([`decide_refused_sync`]) and published as blocking trouble. The try the start was made as is over.
    pub(super) fn refuse_start(&self, stopped: &mut Stopped<'_>, refusal: StartRefusal, root: &crate::folder::root::SyncRoot) {
        let now = self.clock.now();
        if let Some(up) = stopped.folder_mut().up_mut() {
            let why = decide_refused_sync(&up.sync, up.take_down_try.take(), &refusal, &root.path, now);
            if let Why::CannotStart(said) | Why::DriveNotKnown(said) = &why {
                tracing::error!("{said}");
            }
            up.sync = Sync::Stopped(why);
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
            // A sync that runs is asked, and nothing is stopped for it: every next try is due, the quota is read, and a cycle now.
            // The quota is read by the plan's task alone, for the refresh it is woken with (`SyncTasks::refresh`): one read.
            let is_asked = {
                let folder = self.folder.read().await;
                folder.running().map(|started| started.tasks.refresh()).is_some()
            };
            if is_asked {
                return Ok(());
            }
        }
        {
            let mut stopped = self.change().await;
            // The user's try of a folder a fault took down: a try like the owner's own, but for the count.
            let now = self.clock.now();
            if let Some(up) = stopped.folder_mut().up_mut() {
                if let Sync::Stopped(Why::TakenDown { attempt, .. }) = &up.sync {
                    up.take_down_try = Some(TakeDownTry { attempt: *attempt, started_at: now });
                }
            }
            let refreshed = self.refresh_in(&mut stopped).await;
            if refreshed.is_err() {
                // A refusal leaves the folder as it was, its sync included.
                self.start_again(&mut stopped).await;
            }
            refreshed?;
        }
        // The sync this refresh started read the quota before its first plan: nothing is read again.
        if self.running().is_some() {
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

    /// A cycle now, if a OneDrive folder is syncing (the network came back). A read-write
    /// folder's upload queue waits for that cycle (`docs/design/writes.md` §9).
    pub fn refresh_now(&self) {
        if self.nudge() {
            if let Some(parts) = self.view().parts {
                parts.sync_plan_wake.wake(WakeCause::SendingHoldChanged);
            }
        }
    }

    /// [`refresh_now`](Self::refresh_now); whether a sync was running to
    /// nudge.
    pub(super) fn nudge(&self) -> bool {
        match self.view().poll {
            Some(poll) => {
                // The notification socket too: tried again at once when the network came back.
                poll.wake_live();
                poll.refresh();
                true
            }
            None => false,
        }
    }
}

/// Why the sync of the folder at `folder` does not run after a start that was refused with
/// `refusal` at `now`, by what its sync was before the start (`before`) and the try the start
/// was made as (`take_down_try`). Pure.
///
/// - A folder a fault took down stays `Why::TakenDown`, with its cause, whatever the refusal:
///   `Why::CannotStart` is tried again by nothing. The owner's own try (its attempt is one more
///   than the starts tried before) is counted, and the next is due after the wait of that
///   count; the user's try (`Refresh()`: the attempt as it was) changes neither the count nor
///   the time of the next try.
/// - `StartRefusal::DriveNotKnown` is `Why::DriveNotKnown`, which the watcher of the account
///   starts again.
/// - Any other refusal is `Why::CannotStart`, with the sentence.
// test: a_refused_try_after_a_take_down_keeps_the_cause_and_counts_the_try
// test: a_refused_refresh_of_a_taken_down_folder_leaves_its_count_and_its_next_try
// test: a_start_refused_for_no_drive_waits_for_the_account
fn decide_refused_sync(before: &Sync, take_down_try: Option<TakeDownTry>, refusal: &StartRefusal, folder: &std::path::Path, now: i64) -> Why {
    if let Sync::Stopped(Why::TakenDown { cause, attempt, next_try_at }) = before {
        return match take_down_try {
            Some(tried) if tried.attempt > *attempt => {
                let wait = decide_retry_wait(tried.attempt).as_secs() as i64;
                Why::TakenDown { cause: cause.clone(), attempt: tried.attempt, next_try_at: Some(now.saturating_add(wait)) }
            }
            _ => Why::TakenDown { cause: cause.clone(), attempt: *attempt, next_try_at: *next_try_at },
        };
    }
    // TODO: wording
    match refusal {
        StartRefusal::DriveNotKnown => Why::DriveNotKnown(format!(
            "{} is not kept in step yet: which OneDrive drive the account has is not known until it is signed in",
            folder.display()
        )),
        StartRefusal::Store(why) => Why::CannotStart(format!("{} is not kept in step: {why}", folder.display())),
        StartRefusal::Mount(why) => Why::CannotStart(format!("{} is not kept in step: {why}", folder.display())),
    }
}

#[cfg(test)]
mod tests;
