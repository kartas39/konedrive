//! A OneDrive folder's sync started ([`SyncService::start_sync`]: prepare, build, run) and
//! asked for a cycle (`Refresh()`, the network back). How it stops is in `running`.
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
use crate::sync::folder::{Down, Is, Record, Standing, Stopped, Sync, Why};
use crate::sync::{SyncError, SyncPaths, SyncService};

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
        // TODO: `sync_tasks::start_order::start_folder_sync`, in a task of its own, its answer
        // (`StartedFolder`) kept as `Sync::Running`; a `RefusedStart` goes to `cannot_start`.
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
        // TODO: `SyncTasks::refresh` (`sync_tasks::stop_order`) while a sync runs; a folder that is down or
        // whose sync could not start is started through `refresh_in`, inside a change.
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
        // TODO: `OneDriveSpace::apply_quota` (`sync_parts::onedrive_space`) with the quota read, after `Quota::read`.
        todo!()
    }

    /// A cycle now, if a OneDrive folder is syncing (the network came back). A read-write
    /// folder's outbox waits for that cycle (`docs/design/writes.md` §9).
    pub fn refresh_now(&self) {
        // TODO: `SyncTasks::wake_for_network` (`sync_tasks::stop_order`).
        todo!()
    }

    /// [`refresh_now`](Self::refresh_now); whether a sync was running to
    /// nudge.
    pub(super) fn nudge(&self) -> bool {
        // TODO: `ListingPollHandle::refresh` and `ListingPollHandle::wake_live` of `SyncTasks::poll`; whether a
        // sync was running.
        todo!()
    }

}

