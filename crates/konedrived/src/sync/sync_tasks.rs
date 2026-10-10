//! The tasks of a folder's sync by the sync plan — the listing's poller and the plan's
//! task — and the order in which a folder starts, stops and changes its access mode.
//!
//! **State owned:** the poller, the plan's task with the handle of its loop, and the
//! token that stops them. [`SyncTasks`] holds them and hands out the three things others
//! ask of a running sync (what holds sending, the poll, the wake); what each task does is
//! its own. It takes the place of `RunningSync` and its `Parts` (`running_sync.rs`), in
//! either access mode: a read-only folder has both tasks too.
//!
//! **Who owns a folder's start and stop:** the owner of the folder's state — `SyncService`,
//! inside a change of the folder (`Stopped`, today's `start_stop.rs:40`), which holds the
//! folder's write lock. It calls `start_order::start_folder_sync` with everything a start
//! needs, keeps the answer (`StartedFolder`) in the folder's state, and calls
//! `SyncTasks::stop` or `stop_order::take_folder_down` on it. The mount's request for
//! its take-down reaches that owner through the report the start was given
//! (`FolderStartInput::take_down_report`): no task of the sync restarts the folder.
//!
//! **A change of the access mode** (`access_mode_change`): neither task is stopped; the
//! mount is switched and the stamp of the switch written.
//!
//! **Tested without the rest** (the tests of each part are listed in its module):
// test: a_folder_started_and_stopped_leaves_no_task_running
// test: a_take_down_reported_to_the_owner_is_followed_by_a_start_through_the_check
#![allow(dead_code, unused_variables)]

use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::sync_plan_task::SyncPlanTask;
use crate::folder::sync_plan_wake::SyncPlanWake;
use crate::remote::listing_poller::{ListingPollHandle, ListingPoller};
use crate::upload::sending_holds::SendingHolds;

/// The switch between read-only and read-write.
pub mod access_mode_change;
/// A folder's start, stage by stage.
pub mod start_order;
/// A folder's stop, its sign-out and its forgetting.
pub mod stop_order;

/// The running tasks of one folder's sync.
pub struct SyncTasks {
    /// The listing's poller: a cycle on its schedule, on a refresh, when the live socket
    /// says so.
    listing_poller: ListingPoller,
    plan: Arc<SyncPlanTask>,
    plan_loop: JoinHandle<()>,
    sending_holds: Arc<SendingHolds>,
    sync_plan_wake: Arc<SyncPlanWake>,
    stop: CancellationToken,
}

impl SyncTasks {
    /// What holds this folder's sending: the pause, the sign-out and the write gate set
    /// their causes through it.
    pub fn sending_holds(&self) -> &Arc<SendingHolds> {
        todo!()
    }

    /// Asks the listing for a cycle.
    pub fn poll(&self) -> ListingPollHandle {
        todo!()
    }

    /// The user confirmed a mass removal at this count of confirmations
    /// (`SyncPlanTask::begin_removal_window`).
    pub fn begin_removal_window(&self, confirmation_count: u64) {
        todo!()
    }

    /// Wakes the plan: after the user's answer, a change of the ignore list, a pin.
    pub fn sync_plan_wake(&self) -> &Arc<SyncPlanWake> {
        todo!()
    }
}
