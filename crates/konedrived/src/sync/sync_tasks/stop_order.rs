//! A folder's stop, its take-down, its account's sign-out and sign-in, and its
//! forgetting.
//!
//! **State owned:** none.
//!
//! **Answers for:** the map's §7.2 and §7.3. Every function that ends something takes
//! what it ends by value from the owner of the folder's state, inside a change of the
//! folder. The order of an end: the sync is stopped ([`stop_folder_sync`]), which answers
//! the parts the folder keeps; these are then started again
//! (`start_order::start_folder_sync`), taken down ([`take_folder_down`]) or forgotten
//! ([`forget_folder`]).
//!
//! **Tested without the rest:**
// test: a_stop_ends_the_listing_before_the_plans_task
// test: a_stop_ends_the_thumbnail_filler_and_the_watcher_of_the_sign_in
// test: a_step_between_its_disk_change_and_its_commit_is_waited_for_at_a_stop
// test: a_step_that_waits_for_the_lease_under_the_stoppers_write_lock_does_not_hold_the_stop
// test: a_request_cut_off_by_a_stop_is_gone_on_with_or_adopted_at_the_next_start
// test: a_sign_out_holds_all_sending_wakes_the_plan_and_stops_no_task
// test: a_sign_in_forgets_what_was_not_allowed_and_wakes_the_plan
// test: the_watcher_releases_sending_at_each_sign_in_and_ends_at_the_stop
// test: a_refresh_wakes_the_plan_and_asks_for_a_cycle
// test: the_network_back_wakes_the_live_socket_and_the_plan
// test: a_forget_with_something_unsent_is_refused_unless_forced
// test: a_folder_taken_down_has_no_mount_a_closed_store_and_its_trouble_said
// test: a_folder_forgotten_has_its_write_bits_and_a_closed_store
// test: a_store_somebody_still_holds_after_the_wait_is_answered
// test: a_closed_store_that_is_not_there_or_not_of_this_version_counts_as_nothing_unsent
#![allow(dead_code, unused_variables)]

use konedrive_tree::{Store, UnsentTotal};
use tokio_util::sync::CancellationToken;

use super::start_order::{KeptFolderParts, StartedFolder};
use super::SyncTasks;
use crate::account::state::AccountSnapshot;
use crate::fuse::TakeDownCause;
use crate::remote::listing_poller::ListingPollHandle;
use crate::status::snapshot::SyncStateHandle;
use crate::upload::sending_holds::SendingHolds;

impl SyncTasks {
    /// Stops the sync and waits for every task. The stop's token is cancelled (again: the
    /// owner cancelled it before it took the folder's write lock,
    /// [`stop_handle`](Self::stop_handle)); then, in this order: the listing's poller
    /// (`ListingPoller::stop`: a cycle is cancelled between two requests; the page being
    /// written is written whole); the plan's loop, which ends its own steps before it
    /// returns (`SyncPlanTask::run`: requests are aborted, a step between a disk change
    /// and its commit is settled, a step towards the folder that waits for the lease is
    /// not run); the watcher of the sign-in; the thumbnail filler. After it no task holds
    /// a clone of the store. The mount and the store stay.
    ///
    /// Called by the owner of the folder's state for every change of the folder that ends
    /// its sync, the daemon's end among them. The owner runs it in a task of its own
    /// (`tokio::spawn`) and awaits that task: a caller that is cut then does not cut the
    /// stop half-way, and the next change waits for the same task. A `SyncTasks` dropped
    /// without it only tells its tasks to stop (`Drop`).
    pub async fn stop(self) {
        todo!()
    }

    /// The account signed out: all sending is held (`SendingHolds::set_signed_out`, which
    /// wakes the plan) and uploads stop after their fragment; no task stops, and the
    /// mount goes on taking changes, as today.
    pub fn hold_for_sign_out(&self) {
        todo!()
    }

    /// A refresh the user asked for: the plan's task is woken with the refresh's cause —
    /// on which the task itself makes every next try due (`StepMemory::clear_next_tries`
    /// and its own tries; this function clears nothing) — and the listing is asked for a
    /// cycle (`ListingPollHandle::refresh`).
    // TODO: wake with the refresh's own variant of `WakeCause` once the plan's task has it
    // (it is being added); until then `WakeCause::UserAnswered`, on which the task clears
    // the tries today — check and fix if needed.
    pub fn refresh(&self) {
        todo!()
    }

    /// The network is back: the live socket is looked at again
    /// (`ListingPollHandle::wake_live`), a cycle is asked for
    /// (`ListingPollHandle::refresh`), and the plan's task is woken
    /// (`WakeCause::SendingHoldChanged`: "the network"). No next try is made due: a step
    /// that failed for the network is tried at its own time.
    pub fn wake_for_network(&self) {
        todo!()
    }
}

/// The account signed in: what OneDrive refused as not allowed may be allowed now
/// (`StepMemory::forget_refusals_for(&Reason::Forbidden)`), every next try is due
/// (`StepMemory::clear_next_tries`) — both through `store`, a failure of either a warning
/// —, then the hold ends (`SendingHolds::set_signed_out(false)`, which wakes the plan) and
/// the listing is asked for a cycle (`ListingPollHandle::refresh`). Called by the watcher
/// of the sign-in ([`watch_sign_in`]), which holds clones of the three and not the
/// `SyncTasks` it is a task of.
pub async fn release_after_sign_in(store: &Store, sending_holds: &SendingHolds, poll: &ListingPollHandle) {
    todo!()
}

/// The watcher of the account's sign-in, as a task of the sync
/// (`start_order::start_sync_tasks` spawns it, `SyncTasks::stop` awaits it): each time
/// the account becomes signed in from any other state it calls
/// [`release_after_sign_in`]. It ends when `stop` is cancelled and when the account's
/// sender is gone. Without it a folder that says "signed out" would say so until the
/// poller's next cycle after the sign-in.
pub async fn watch_sign_in(account_changes: tokio::sync::watch::Receiver<AccountSnapshot>, store: Store, sending_holds: std::sync::Arc<SendingHolds>, poll: ListingPollHandle, stop: CancellationToken) {
    todo!()
}

/// Stops the sync of `started` (`SyncTasks::stop`) and answers what the folder keeps: the
/// store, the mount — still served — and the leaf state the mount was made with. Called
/// by the owner of the folder's state where a sync ends, after it cancelled the stop's
/// token and took the folder's write lock; what it answers is started again, taken down
/// or forgotten.
pub async fn stop_folder_sync(started: StartedFolder) -> KeptFolderParts {
    todo!()
}

/// Whether the store of a folder that was taken down or forgotten is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeDownOutcome {
    /// The store is closed: its files may be removed, and the folder may be started
    /// again with nothing kept.
    StoreClosed,
    /// Somebody still holds the store after the wait ran out. After a take-down nothing
    /// is started: the folder stays down with its trouble said, and the next start of the
    /// daemon runs the check. After a Forget the owner removes the files all the same,
    /// with a warning, as today.
    StoreStillOpen,
}

/// Takes the folder down after its mount asked for it. Called by the owner of the
/// folder's state, inside a change of the folder, when the report it gave the start was
/// called, AFTER it stopped the sync ([`stop_folder_sync`], whose answer is `kept`).
///
/// Before the call the owner has detached the activity log from the store
/// (`Activity::detach`) and taken back what it gave the bus and the view (the mount's
/// `files_interface`, the view's clone of the store): each holds the store open.
///
/// In this order: `cause` is said in the journal and as the folder's blocking trouble
/// (through `state`); the mount stopped (`ItemMount::stop`, off the runtime's threads: it
/// aborts every download, so none holds the store any more, and takes the write bits
/// off); every `Arc` of the mount and of the leaf state dropped — `kept` is taken by
/// value; then the store closed (`Store::close`), waited for with a bound.
///
/// The owner does NOT start the folder again by itself after a take-down for a fault:
/// the same fault would take it down again, without end. The folder stays in trouble
/// until the user's refresh or the daemon's next start, whose
/// `start_order::start_folder_sync` with nothing kept runs the check at start, which
/// reads what the failed undo left as a change made while no daemon ran (design §8).
pub async fn take_folder_down(kept: KeptFolderParts, cause: TakeDownCause, state: &SyncStateHandle) -> TakeDownOutcome {
    todo!()
}

/// Ends the parts of a folder that is forgotten. Called by the owner of the folder's
/// state, inside a change of the folder, after [`decide_forget`] allowed it and the sync
/// was stopped ([`stop_folder_sync`], whose answer is `kept`); with the same things done
/// before the call as before [`take_folder_down`].
///
/// In this order: the mount stopped and the folder left a plain directory with its write
/// bits (`ItemMount::stop_and_restore_write_bits`, off the runtime's threads); every
/// `Arc` of the mount and of the leaf state dropped; the store closed (`Store::close`),
/// waited for with a bound. The store's files, `config.toml` and what the bus shows are
/// the owner's to remove and clear afterwards.
pub async fn forget_folder(kept: KeptFolderParts) -> TakeDownOutcome {
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
/// reader opened for the question: for a Forget of a folder that keeps no open store.
/// Blocking. Where there is no file, and where the file cannot be opened as a store of
/// this version (an older version, no database), nothing is unsent: a sync would rebuild
/// that store empty, so it holds nothing a sync could send. Any other failure — a
/// permission, the disk — is the error, and the Forget is refused for it.
pub fn count_unsent_of_closed_store(tree_db: &std::path::Path) -> Result<UnsentTotal, konedrive_tree::TreeError> {
    todo!()
}
