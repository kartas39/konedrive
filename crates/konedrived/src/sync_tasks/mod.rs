//! The tasks of a folder's sync by the sync plan — the listing's poller and the plan's
//! task — and the order in which a folder starts, stops and changes its access mode.
//!
//! **State owned:** the poller, the plan's task with the handle of its loop, the
//! thumbnail filler, and the token that stops them all. No task of the sync watches the
//! account: the owner's one watcher does (`account_folder::mode::watch_account`), and sets
//! and releases what the account holds through [`SyncTasks`].
//! [`SyncTasks`] holds them and hands out the things others ask of a running sync (what
//! holds sending, the poll, the wake, the stop's token); what each task does is its own.
//! A folder has every task in either access mode: a read-only folder too.
//!
//! **Who owns a folder's start and stop:** the owner of the folder's state — `SyncService`,
//! inside a change of the folder, which holds the folder's write lock. It calls
//! `start_order::start_folder_sync` with everything a start needs, keeps the answer
//! (`StartedFolder`) in the folder's state, and ends it with `stop_order::stop_folder_sync`
//! — whose answer, `start_order::KeptFolderParts`, it keeps as it is, the one type of what
//! a stopped sync keeps — and then, where the folder goes down too,
//! `stop_order::take_folder_down` or `stop_order::forget_folder`. The mount's request for
//! its take-down reaches that owner through the report the start was given
//! (`FolderStartInput::take_down_report`): no task of the sync takes the folder down or
//! starts it. After a take-down for a fault the OWNER starts the folder again by itself,
//! after a growing pause (`account_folder::take_down`: 1 min, 5 min, 30 min, then hourly;
//! the user's refresh tries at once; the count starts over after a start that stayed
//! up): the timer and the count are the owner's state, and nothing here keeps either.
//!
//! **What the owner's change must do:**
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
//! - it runs the start in a task of its own too (`tokio::spawn`) and awaits that task: a
//!   start cut between two of its stages would drop a store the activity log is still
//!   attached to, and a mount on one of the runtime's threads, where its drop joins a
//!   thread and unmounts. `start_order::start_folder_sync` relies on never being cut;
//! - at the daemon's end it calls ONE function, `stop_order::end_kept_folder_at_daemon_stop`,
//!   inside a change of the folder (which stopped the sync), in a task of its own, under
//!   the daemon's stop bound: the mount stopped, which leaves the folder on disk without its write bits; it closes
//!   nothing more, the process exits. Neither `take_folder_down` (it says a trouble) nor
//!   `forget_folder` (it gives the write bits back) is called for it;
//! - a pause an older daemon left is not taken over (`take_old_pause` has no place in the
//!   start's order): a store of an older version is rebuilt.
//!
//! **A change of the access mode** (`access_mode_change`): no task is stopped; the mount
//! is switched and the hold of a read-only folder set or taken off.
//!
//! **Tested without the rest** (the tests of each part are listed in its module):
// test: a_folder_started_and_stopped_leaves_no_task_running
// test: a_sync_dropped_unstopped_has_its_stop_token_cancelled
// test: a_sync_started_over_the_fixture_runs_its_tasks_until_it_is_stopped
// test: the_holder_hands_out_the_holds_the_poll_and_the_stops_token_of_its_tasks

use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::sync_parts::sync_plan_wake::SyncPlanWake;
use crate::listing::listing_poller::{ListingPollHandle, ListingPoller};
use crate::sync_parts::sending_holds::SendingHolds;

/// The switch between read-only and read-write.
pub(crate) mod access_mode_change;
/// A folder's start, stage by stage.
pub(crate) mod start_order;
/// A folder's stop, its sign-out and its forgetting.
pub(crate) mod stop_order;

/// The running tasks of one folder's sync.
pub(crate) struct SyncTasks {
    /// The listing's poller: a cycle on its schedule, on a refresh, when the live socket
    /// says so. `None` once [`stop`](Self::stop) took it to stop it.
    listing_poller: Option<ListingPoller>,
    /// The poller's handle, cloned from it at the start: what [`poll`](Self::poll)
    /// answers, also once the poller was taken. The plan's
    /// task were given clones of the same.
    poll: ListingPollHandle,
    plan_loop: JoinHandle<()>,
    /// The thumbnail filler (`desktop::thumbs::ThumbnailFiller`); none without a cache to
    /// fill. It holds a clone of the store until it ends.
    thumbnail_filler: Option<JoinHandle<()>>,
    sending_holds: Arc<SendingHolds>,
    sync_plan_wake: Arc<SyncPlanWake>,
    /// Cancelled, every task ends: the plan's loop, the thumbnail filler, and a step towards the folder that waits for the folder's lease.
    stop: CancellationToken,
}

impl SyncTasks {
    /// What holds this folder's sending: the pause, the sign-out and why the account may not
    /// write are set through it.
    pub(crate) fn sending_holds(&self) -> &Arc<SendingHolds> {
        &self.sending_holds
    }

    /// Asks the listing for a cycle.
    pub(crate) fn poll(&self) -> ListingPollHandle {
        self.poll.clone()
    }

    /// The token that tells the sync to stop: a clone of the one every task and the step
    /// runners were given (`start_order::start_sync_tasks`). The owner of the folder's
    /// state cancels it BEFORE it takes the folder's write lock, then takes the lock, then
    /// awaits [`stop`](Self::stop): a step towards the folder that waits for the folder's
    /// lease when the token is cancelled ends as `StepOutcome::NotRun(SendingStopped)`
    /// (`StepRunners::of_runners`), so the plan's loop has no step left that waits for
    /// the lock its stopper holds.
    pub(crate) fn stop_handle(&self) -> CancellationToken {
        self.stop.clone()
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

// Seen by the tests of the layers above: `tests::start_sync_over_fixture` gives them a sync that runs.
#[cfg(test)]
pub(crate) mod tests;
