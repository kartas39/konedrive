//! A OneDrive folder's sync started ([`SyncService::start_sync`]) and asked for a cycle
//! (`Refresh()`, the network back). How it starts and stops is `crate::sync_tasks`'.
// The skeleton of the switch: bodies that drove the old mechanism are `todo!()`.
#![allow(dead_code, unused_variables, unused_imports, unused_mut)]

use std::sync::{Arc, Mutex, OnceLock};

use konedrive_graph::drive::DriveClient;
use konedrive_tree::Store;
use tokio::sync::{watch, Notify};
use tokio_util::sync::CancellationToken;

use crate::account::state::SignInState;
use crate::config::Mode;
use crate::desktop::thumbs;
use crate::account_folder::folder::{Down, Is, Record, Standing, Stopped, Sync, Why};
use crate::account_folder::{SyncError, SyncPaths, SyncService};

impl SyncService {
    /// Starts the sync of the folder that is up now; nothing for a folder that is not up. Inside a change of the
    /// folder's state, so that no Forget can come in between, and none runs: the change
    /// stopped it.
    ///
    /// The start itself is `sync_tasks::start_order::start_folder_sync`'s, stage by stage; what
    /// it answers is put into the state, from where only a change takes it out.
    pub(super) async fn start_sync(&self, stopped: &mut Stopped<'_>) {
        // TODO: `sync_tasks::start_order::start_folder_sync`, in a task of its own, its answer
        // (`StartedFolder`) kept as `Sync::Running`. Its input: `kept` is `Kept::parts`, taken whole;
        // `settings` is `PlanSettings::of(self.machine_name())`; `standing_holds` what stops the account's work now
        // (`Running::stop`: the user's pause with its end, the automatic hold as paused until resumed) and
        // `mode::decide_account_holds` of the account's snapshot read now; `take_down_report` is
        // `self.take_down_report()`; `unsent_count_report` tells the account
        // (`FolderAccount::tell_unsent_count`); `lease` is `Lease::on(&self.folder)`.
        // A `RefusedStart` goes to `cannot_start` — `StartRefusal::DriveNotKnown` as `Why::DriveNotKnown`,
        // which the watcher of the account starts again; a start that is a try after a take-down
        // (`Up::take_down_try`) stays `Why::TakenDown`, as `retry_after_take_down` says — and the parts it
        // gives back into `Kept::parts`. What a rebuild of the store lost (`StartedFolder::store_origin`,
        // `RefusedStart::rebuild_loss`) is read here, once, and said (`describe_rebuild_loss`).
        // After a start: the desktop told the mount came (`stopped.read_again()`, in either access mode),
        // and `PinnedCount` shown (`show_pins`).
        todo!()
    }

    /// What the user is told once after a start whose store was rebuilt and lost
    /// something (`RebuildLoss`: pins, unsent changes that are not on disk, open upload
    /// sessions): `None` when nothing was lost. Pure.
    // TODO: where it is said beside the journal — an event of the activity log, the folder's `LastError` — and in which words is the owner's to say; until then the journal alone.
    #[allow(unused_variables)]
    fn describe_rebuild_loss(loss: &konedrive_tree::RebuildLoss) -> Option<String> {
        todo!()
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
        // TODO: `SyncTasks::refresh` (`sync_tasks::stop_order`: `WakeCause::RefreshAsked` and a cycle) while a
        // sync runs, with `refresh_quota`; a folder that is down or whose sync does not run — it could not
        // start, or a fault took its mount down (`Why::TakenDown`: tried at once, whatever its next try, the
        // count kept) — is started through `refresh_in`, inside a change.
        todo!()
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
        // TODO: `DriveClient::quota`, then `OneDriveSpace::apply_quota(quota, now)` of the running sync (who reads the
        // quota applies it; it writes the account's figures too), then the plan's task woken (`WakeCause::QuotaRead`).
        todo!()
    }

    /// A cycle now, if a OneDrive folder is syncing (the network came back). A read-write
    /// folder's outbox waits for that cycle (`docs/design/writes.md` §9).
    pub fn refresh_now(&self) {
        // TODO: what `SyncTasks::wake_for_network` does, over the handles of the view (this is not async and holds no
        // lock of the folder): `View::poll` (`wake_live`, `refresh`) and the plan's wake (`View::parts`,
        // `WakeCause::SendingHoldChanged`).
        todo!()
    }

    /// [`refresh_now`](Self::refresh_now); whether a sync was running to
    /// nudge.
    pub(super) fn nudge(&self) -> bool {
        // TODO: as `refresh_now`, over `View::poll`; whether a sync was running (`View::poll` is some).
        todo!()
    }

}

