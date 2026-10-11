//! The tasks of a folder's sync by the sync plan — the listing's poller and the plan's
//! task — and the order in which a folder starts, stops and changes its access mode.
//!
//! **State owned:** the poller, the plan's task with the handle of its loop, the
//! thumbnail filler, the watcher of the sign-in, and the token that stops them all.
//! [`SyncTasks`] holds them and hands out the things others ask of a running sync (what
//! holds sending, the poll, the wake, the stop's token); what each task does is its own.
//! It takes the place of `RunningSync` and its `Parts` (`running_sync.rs`), in either
//! access mode: a read-only folder has every task too.
//!
//! **Who owns a folder's start and stop:** the owner of the folder's state — `SyncService`,
//! inside a change of the folder, which holds the folder's write lock. It calls
//! `start_order::start_folder_sync` with everything a start needs, keeps the answer
//! (`StartedFolder`) in the folder's state, and ends it with `stop_order::stop_folder_sync`
//! and then, where the folder goes down too, `stop_order::take_folder_down` or
//! `stop_order::forget_folder`. The mount's request for its take-down reaches that owner
//! through the report the start was given (`FolderStartInput::take_down_report`): no task
//! of the sync takes the folder down or starts it.
//!
//! **What the owner's change must do** — the owner's change of a folder is rewritten at
//! the switch, and this layer does not bend to the one in the tree:
//!
//! - it stops the sync only for what needs it: leaving the account, forgetting the
//!   folder, a take-down, the daemon's end. The change of the access mode and the change
//!   of the ignore list (`access_mode_change`) run while the tasks run;
//! - where it stops the sync it cancels [`SyncTasks::stop_handle`] BEFORE it takes the
//!   folder's write lock, then takes the lock, then awaits the stop: a step towards the
//!   folder waits for the folder's lease, a read of that lock, and a stop awaited under
//!   the write lock without the token cancelled first would wait for that step for ever;
//! - it runs the stop in a task of its own, so that a caller that is cut does not cut
//!   the stop;
//! - a pause an older daemon left is not taken over (`take_old_pause` has no place in the
//!   start's order): a store of an older version is rebuilt.
//!
//! **A change of the access mode** (`access_mode_change`): no task is stopped; the mount
//! is switched and the stamp of the switch written.
//!
//! **Tested without the rest** (the tests of each part are listed in its module):
// test: a_folder_started_and_stopped_leaves_no_task_running
// test: a_folder_taken_down_for_a_fault_is_not_started_again_by_itself
// test: a_sync_dropped_unstopped_has_its_stop_token_cancelled
#![allow(dead_code, unused_variables)]

use std::sync::Arc;

use konedrive_tree::Store;
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
    /// says so. `None` once [`stop`](Self::stop) took it to stop it.
    listing_poller: Option<ListingPoller>,
    plan: Arc<SyncPlanTask>,
    plan_loop: JoinHandle<()>,
    /// The thumbnail filler (`desktop::thumbs::ThumbnailFiller`); none without a cache to
    /// fill. It holds a clone of the store until it ends.
    thumbnail_filler: Option<JoinHandle<()>>,
    /// The watcher of the account's sign-in (`stop_order::watch_sign_in`).
    sign_in_watcher: JoinHandle<()>,
    sending_holds: Arc<SendingHolds>,
    sync_plan_wake: Arc<SyncPlanWake>,
    /// The folder's store, as the tasks hold it: the watcher of the sign-in was given a
    /// clone of this (`stop_order::release_after_sign_in` forgets through it what the
    /// steps remember). Dropped with the tasks, so that a stopped sync holds no clone.
    store: Store,
    /// Cancelled, every task ends: the plan's loop, the thumbnail filler, the watcher of
    /// the sign-in, and a step towards the folder that waits for the folder's lease.
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

    /// Wakes the plan: after the user's answer, a change of the ignore list, a pin.
    pub fn sync_plan_wake(&self) -> &Arc<SyncPlanWake> {
        todo!()
    }

    /// The token that tells the sync to stop: a clone of the one every task and the step
    /// runners were given (`start_order::start_sync_tasks`). The owner of the folder's
    /// state cancels it BEFORE it takes the folder's write lock, then takes the lock, then
    /// awaits [`stop`](Self::stop): a step towards the folder that waits for the folder's
    /// lease when the token is cancelled ends as `StepOutcome::NotRun(SendingStopped)`
    /// (`StepRunners::of_runners`), so the plan's loop has no step left that waits for
    /// the lock its stopper holds.
    pub fn stop_handle(&self) -> CancellationToken {
        todo!()
    }
}

/// A sync that is dropped unstopped is told to stop: its tasks end by themselves and let
/// go of the store, and no plan's loop runs beside the one of a later start. Nothing is
/// waited for here; whoever needs the tasks ended awaits [`SyncTasks::stop`].
impl Drop for SyncTasks {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
