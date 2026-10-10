//! A folder's stop, its take-down, its account's sign-out and sign-in, and its
//! forgetting.
//!
//! **State owned:** none.
//!
//! **Answers for:** the map's §7.2 and §7.3. Every function that ends something takes
//! what it ends by value from the owner of the folder's state, inside a change of the
//! folder.
//!
//! **Tested without the rest:**
// test: a_stop_ends_the_listing_before_the_plans_task
// test: a_step_between_its_disk_change_and_its_commit_is_waited_for_at_a_stop
// test: a_request_cut_off_by_a_stop_is_gone_on_with_or_adopted_at_the_next_start
// test: a_sign_out_holds_all_sending_wakes_the_plan_and_stops_no_task
// test: a_sign_in_forgets_what_was_not_allowed_and_wakes_the_plan
// test: a_forget_with_something_unsent_is_refused_unless_forced
// test: a_folder_taken_down_has_no_task_no_mount_and_a_closed_store
// test: a_store_somebody_still_holds_after_the_wait_is_answered_and_nothing_is_started
#![allow(dead_code, unused_variables)]

use konedrive_tree::UnsentTotal;

use super::start_order::{KeptFolderParts, StartedFolder};
use super::SyncTasks;
use crate::fuse::TakeDownCause;

impl SyncTasks {
    /// Stops the sync: the listing's poller first (`ListingPoller::stop`: a cycle is
    /// cancelled between two requests; the page being written is written whole), then the
    /// plan's loop, which ends its own steps before it returns (`SyncPlanTask::run`:
    /// requests are aborted, a step between a disk change and its commit is settled).
    /// The mount and the store stay. Called by the owner of the folder's state for every change of the folder
    /// that ends its sync, the daemon's end among them, as `Ending::join` today
    /// (`running_sync.rs:140-160`).
    pub async fn stop(self) {
        todo!()
    }

    /// The account signed out: all sending is held (`SendingHolds::set_signed_out`, which
    /// wakes the plan) and uploads stop after their fragment; neither task stops, and the
    /// mount goes on taking changes, as today.
    pub fn hold_for_sign_out(&self) {
        todo!()
    }

    /// The account signed in: what OneDrive refused as not allowed may be allowed now
    /// (`StepMemory::forget_refusals_for(&Reason::Forbidden)`), every next try is due
    /// (`clear_next_tries`), the hold ends, the listing is asked for a cycle
    /// (`ListingPollHandle::refresh`). Today:
    /// `nudge_on_sign_in` and the worker's `release_forbidden`.
    pub async fn release_after_sign_in(&self) {
        todo!()
    }

    /// A refresh the user asked for, and the network back: every next try is due
    /// (`StepMemory::clear_next_tries`), a cycle is asked for (`ListingPollHandle::refresh`,
    /// and `wake_live` for the network), the plan woken
    /// (`WakeCause::UserAnswered`). Today `retry_outbox` and `outbox_after_network`.
    pub async fn retry_now(&self) {
        todo!()
    }
}

/// Stops the sync of `started` and answers what the folder keeps for its next sync: the
/// store, the mount and the leaf state the mount was made with. Called by the owner of
/// the folder's state where a sync ends and the folder stays up.
pub async fn stop_folder_sync(started: StartedFolder) -> KeptFolderParts {
    todo!()
}

/// Whether a folder that was taken down can be started again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeDownOutcome {
    /// The store is closed: the owner starts the folder again.
    StoreClosed,
    /// Somebody still holds the store after the wait ran out: nothing is started, the
    /// folder stays down with its trouble said, and the next start of the daemon runs the
    /// check.
    StoreStillOpen,
}

/// Takes the folder down after its mount asked for it, in this order: the tasks stopped
/// (`SyncTasks::stop`); the mount stopped (`ItemMount::stop`, which aborts every
/// download: none holds the store any more); every `Arc` of the mount and of the parts
/// it handed out dropped — `started` is taken by value, and the owner has taken back
/// before what it gave the bus (`files_interface`); then the store closed
/// (`Store::close`), waited for with a bound. `cause` is said in the journal and as the
/// folder's trouble. Called by the owner of the folder's state, inside a change of the
/// folder, when the report it gave the start is called; on `StoreClosed` it calls
/// `start_order::start_folder_sync` with nothing kept, whose check at start reads what
/// the failed undo left as a change made while no daemon ran (design §8).
pub async fn take_folder_down(started: StartedFolder, cause: TakeDownCause) -> TakeDownOutcome {
    todo!()
}

/// What a Forget does with a folder that holds unsent changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgetDecision {
    Forget,
    /// Refused, with how many changes wait.
    RefusedForUnsent { count: u64 },
}

/// Whether the folder is forgotten: refused while something is unsent, unless forced, as
/// today; counted by `ItemReader::count_unsent` — also from a store that is closed. Pure.
pub fn decide_forget(unsent: UnsentTotal, is_forced: bool) -> ForgetDecision {
    todo!()
}

/// How many changes of the folder whose store is at `tree_db` are unsent, read through a
/// reader opened for the question: for a Forget of a folder whose sync does not run.
pub fn count_unsent_of_closed_store(tree_db: &std::path::Path) -> Result<UnsentTotal, konedrive_tree::TreeError> {
    todo!()
}
