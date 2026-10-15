//! A folder's stop, its take-down, its account's sign-out and sign-in, and its
//! forgetting.
//!
//! **State owned:** none.
//!
//! **Answers for:** the order of a stop (`docs/design/sync.md` §9.3). Every function that ends something takes
//! what it ends by value from the owner of the folder's state, inside a change of the
//! folder. The order of an end: the sync is stopped (`stop_folder_sync`), which answers
//! the parts the folder keeps; these are then started again
//! (`start_order::start_folder_sync`), taken down (`take_folder_down`) or forgotten
//! (`forget_folder`).
//!
//! **Tested without the rest:**
// test: a_stop_ends_the_listing_before_the_plans_task
// test: a_stop_ends_the_thumbnail_filler
// test: a_step_between_its_disk_change_and_its_commit_is_waited_for_at_a_stop
// test: a_step_that_waits_for_the_lease_under_the_stoppers_write_lock_does_not_hold_the_stop
// test: a_request_cut_off_by_a_stop_is_gone_on_with_or_adopted_at_the_next_start
// test: a_sign_out_holds_all_sending_wakes_the_plan_and_stops_no_task
// test: a_sign_in_forgets_what_was_not_allowed_and_wakes_the_plan
// test: a_refresh_wakes_the_plan_and_asks_for_a_cycle
// test: the_network_back_wakes_the_live_socket_and_the_plan
// test: a_forget_with_something_unsent_is_not_refused
// test: a_folder_taken_down_has_no_mount_a_closed_store_and_its_trouble_said
// test: a_folder_forgotten_has_its_write_bits_and_a_closed_store
// test: a_folder_forgotten_with_no_parts_kept_has_its_write_bits
// test: a_store_somebody_still_holds_after_the_wait_is_answered
// test: a_closed_store_that_is_not_there_or_not_of_this_version_counts_as_nothing_unsent
#![allow(dead_code, unused_variables)]

use konedrive_tree::{Reason, Store, UnsentTotal};

use super::start_order::{KeptFolderParts, StartedFolder};
use super::SyncTasks;
use crate::folder::root::SyncRoot;
use crate::listing::listing_poller::ListingPollHandle;
use crate::sync_parts::sending_holds::SendingHolds;
use crate::sync_parts::sync_plan_wake::WakeCause;

impl SyncTasks {
    /// Stops the sync and waits for every task. The stop's token is cancelled (again: the
    /// owner cancelled it before it took the folder's write lock,
    /// [`stop_handle`](Self::stop_handle)); then, in this order: the listing's poller
    /// (`ListingPoller::stop`: a cycle is cancelled between two requests; the page being
    /// written is written whole); the plan's loop, which ends its own steps before it
    /// returns (`SyncPlanTask::run`: requests are aborted, a step between a disk change
    /// and its commit is settled, a step towards the folder that waits for the lease is
    /// not run); the thumbnail filler. After it no task holds
    /// a clone of the store. The mount and the store stay. The queue's status on the bus
    /// is empty by then: the plan's loop published it so before it returned
    /// (`upload_queue::publish_stopped_queue`), and nobody else writes it.
    ///
    /// Called by the owner of the folder's state for every change of the folder that ends
    /// its sync, the daemon's end among them. The owner runs it in a task of its own
    /// (`tokio::spawn`) and awaits that task: a caller that is cut then does not cut the
    /// stop half-way, and the next change waits for the same task. A `SyncTasks` dropped
    /// without it only tells its tasks to stop (`Drop`).
    pub(crate) async fn stop(self) {
        let mut tasks = self;
        tasks.stop.cancel();
        if let Some(listing_poller) = tasks.listing_poller.take() {
            listing_poller.stop().await;
        }
        if let Err(error) = (&mut tasks.plan_loop).await {
            tracing::warn!("the loop of the plan's task failed: {error}");
        }
        if let Some(thumbnail_filler) = tasks.thumbnail_filler.take() {
            if let Err(error) = thumbnail_filler.await {
                tracing::warn!("the task of the thumbnail filler failed: {error}");
            }
        }
    }

    /// The account signed out: all sending is held (`SendingHolds::set_signed_out`, which
    /// wakes the plan) and uploads stop after their fragment; no task stops, and the
    /// mount goes on taking changes. Called by the owner's watcher of the
    /// account (`SyncService::follow_account`) when the snapshot it applies says signed
    /// out; the same watcher ends the hold ([`release_after_sign_in`]). The plan's task
    /// sets the same hold itself when a step is answered that the account is signed out
    /// (`sync_plan_task`'s settlement), whichever comes first: that hold too is ended by
    /// the owner's watcher, at the next snapshot that says signed in.
    pub(crate) fn hold_for_sign_out(&self) {
        self.sending_holds.set_signed_out(true);
    }

    /// A refresh the user asked for ("sync now"): the plan's task is woken with
    /// `WakeCause::RefreshAsked` — on which the task itself reads the quota, once, and
    /// makes every next try due (`StepMemory::clear_next_tries` and its own tries; this
    /// function reads and clears nothing) —
    /// and the listing is asked for a cycle (`ListingPollHandle::refresh`). The one way a
    /// refresh reaches a running sync: `Folder.Refresh()` calls nothing else of it.
    pub fn refresh(&self) {
        self.sync_plan_wake.wake(WakeCause::RefreshAsked);
        self.poll.refresh();
    }

    /// The network is back: the live socket is looked at again
    /// (`ListingPollHandle::wake_live`), a cycle is asked for
    /// (`ListingPollHandle::refresh`), and the plan's task is woken
    /// (`WakeCause::SendingHoldChanged`: "the network"). No next try is made due: a step
    /// that failed for the network is tried at its own time.
    pub(crate) fn wake_for_network(&self) {
        self.poll.wake_live();
        self.poll.refresh();
        self.sync_plan_wake.wake(WakeCause::SendingHoldChanged);
    }
}

/// The account signed in: what OneDrive refused as not allowed may be allowed now
/// (`StepMemory::forget_refusals_for(&Reason::Forbidden)`), every next try is due
/// (`StepMemory::clear_next_tries`) — both through `store`, a failure of either a warning
/// —, then the hold ends (`SendingHolds::set_signed_out(false)`, which wakes the plan) and
/// the listing is asked for a cycle (`ListingPollHandle::refresh`). Called by the owner's
/// watcher of the account (`SyncService::follow_account`) — the ONE that sets and ends
/// the hold of a sign-out from outside the sync — when the snapshot it applies says
/// signed in and the holds say signed out (`SendingHolds::find`), with the store, the
/// holds and the poll of the running sync (`StartedFolder::store`,
/// `SyncTasks::sending_holds`, `SyncTasks::poll`). Since one task applies the snapshots
/// in their order, no hold set from an older snapshot outlives a newer one.
pub(crate) async fn release_after_sign_in(store: &Store, sending_holds: &SendingHolds, poll: &ListingPollHandle) {
    if let Err(error) = store.call(|store| store.step_memory().forget_refusals_for(&Reason::Forbidden)).await {
        tracing::warn!("cannot forget what OneDrive refused as not allowed: {error}");
    }
    if let Err(error) = store.call(|store| store.step_memory().clear_next_tries()).await {
        tracing::warn!("cannot make the next tries due: {error}");
    }
    sending_holds.set_signed_out(false);
    poll.refresh();
}

/// Stops the sync of `started` (`SyncTasks::stop`) and answers what the folder keeps: the
/// store, the mount — still served —, the leaf state the mount was made with and the
/// folder's drive (`StartedFolder::drive_id`). Called
/// by the owner of the folder's state where a sync ends, after it cancelled the stop's
/// token and took the folder's write lock; what it answers is started again, taken down
/// or forgotten.
pub(crate) async fn stop_folder_sync(started: StartedFolder) -> KeptFolderParts {
    let StartedFolder { store, mount, tasks, sync_plan_wake, running_steps, published_plan, onedrive_space: _, drive_id, store_origin: _ } = started;
    tasks.stop().await;
    KeptFolderParts { store, mount, sync_plan_wake, running_steps, published_plan, drive_id }
}

/// Whether the store of a folder that was taken down or forgotten is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TakeDownOutcome {
    /// The store is closed: its files may be removed, and the folder may be started
    /// again with nothing kept.
    StoreClosed,
    /// Somebody still holds the store after the wait ran out. After a take-down nothing
    /// is started: the folder stays down with its trouble said, and the next start of the
    /// daemon runs the check. After a Forget the owner removes the files all the same,
    /// with a warning.
    StoreStillOpen,
}

/// How long the end of a folder's parts waits for the store's connections to close.
const STORE_CLOSES_WITHIN: std::time::Duration = std::time::Duration::from_secs(10);

/// How the mount of a folder's parts is ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MountEnd {
    /// Stopped: the folder on disk is left without its write bits (a take-down).
    Stopped,
    /// Stopped, and the folder left a plain directory with its write bits (a Forget).
    StoppedWithWriteBits,
}

/// Ends the parts a stopped sync kept: the mount stopped as `mount_end` says, off the
/// runtime's threads; every `Arc` of the mount and of the leaf state dropped; then the
/// store closed, waited for with [`STORE_CLOSES_WITHIN`].
async fn end_kept_parts(kept: KeptFolderParts, mount_end: MountEnd) -> TakeDownOutcome {
    let KeptFolderParts { store, mount, sync_plan_wake, running_steps, published_plan, drive_id: _ } = kept;
    let stopped = tokio::task::spawn_blocking(move || {
        match mount_end {
            MountEnd::Stopped => mount.stop(),
            MountEnd::StoppedWithWriteBits => mount.stop_and_restore_write_bits(),
        }
        // The last of it, where nobody else holds it: dropped off the runtime's threads.
        drop(mount);
    });
    if let Err(error) = stopped.await {
        tracing::warn!("the task stopping the folder's mount failed: {error}");
    }
    drop((sync_plan_wake, running_steps, published_plan));
    match tokio::time::timeout(STORE_CLOSES_WITHIN, store.close()).await {
        Ok(()) => TakeDownOutcome::StoreClosed,
        Err(_) => TakeDownOutcome::StoreStillOpen,
    }
}

/// Takes the folder down after its mount asked for it. Called by the owner of the
/// folder's state, inside a change of the folder, when the report it gave the start was
/// called, AFTER it stopped the sync ([`stop_folder_sync`], whose answer is `kept`).
///
/// Before the call the owner has detached the activity log from the store
/// (`Activity::detach`) and taken back what it gave the bus and the view (the mount's
/// `files_interface`, the view's clone of the store): each holds the store open.
///
/// It says nothing: the sentence the folder shows is the owner's
/// (`account_folder::take_down::describe_take_down`, kept as `Why::TakenDown::cause` and
/// published by the owner's `publish`, the one writer of it).
///
/// In this order: the mount stopped (`ItemMount::stop`, off the runtime's threads: it
/// aborts every download, so none holds the store any more, and takes the write bits
/// off); every `Arc` of the mount and of the leaf state dropped — `kept` is taken by
/// value; then the store closed (`Store::close`), waited for with a bound.
///
/// Nothing is started here. The OWNER starts the folder again by itself, after a growing
/// pause (`account_folder::take_down::decide_retry_wait`: 1 min, 5 min, 30 min, then
/// hourly), at once on the user's "sync now", and at the daemon's next start; the folder
/// shows its trouble with its cause meanwhile. Each such start is
/// `start_order::start_folder_sync` with nothing kept, which runs the check at start,
/// which settles what the failed undo left from its record (`docs/design/mount.md` §8.1).
/// `StoreStillOpen` is answered to that owner, which then does not start by itself.
pub(crate) async fn take_folder_down(kept: KeptFolderParts) -> TakeDownOutcome {
    end_kept_parts(kept, MountEnd::Stopped).await
}

/// The daemon's end, for one folder: the mount of the parts its stopped sync kept is
/// stopped (`ItemMount::stop`, off the runtime's threads), which leaves the folder on
/// disk without its write bits; the parts are dropped, nothing is closed, the process
/// exits. The ONE function of this layer for the daemon's end: the owner calls it inside
/// a change of the folder — which stopped the sync, if one ran, and put what it kept into
/// the folder's record —, in a task of its own and under the daemon's stop bound. A
/// caller that is cut at the bound leaves the task running until the process exits, and
/// the unit's tidy step (`konedrived --tidy`) does for the folder what was not reached.
pub(crate) async fn end_kept_folder_at_daemon_stop(kept: KeptFolderParts) {
    let stopped = tokio::task::spawn_blocking(move || {
        kept.mount.stop();
        // Dropped here, off the runtime's threads: nothing is closed or waited for.
        drop(kept);
    });
    if let Err(error) = stopped.await {
        tracing::warn!("the task stopping the folder's mount failed: {error}");
    }
}

/// Ends the parts of a folder that is forgotten. Called by the owner of the folder's
/// state, inside a change of the folder, after the sync
/// was stopped ([`stop_folder_sync`], whose answer is `kept`); with the same things done
/// before the call as before [`take_folder_down`].
///
/// In this order: the mount stopped and the folder left a plain directory with its write
/// bits (`ItemMount::stop_and_restore_write_bits`, off the runtime's threads); every
/// `Arc` of the mount and of the leaf state dropped; the store closed (`Store::close`),
/// waited for with a bound. The store's files, `config.toml` and what the bus shows are
/// the owner's to remove and clear afterwards.
///
/// A folder that keeps no parts — its start was refused, or it was taken down — has no
/// mount to stop: for it the owner calls [`forget_folder_without_mount`] instead.
pub(crate) async fn forget_folder(kept: KeptFolderParts) -> TakeDownOutcome {
    end_kept_parts(kept, MountEnd::StoppedWithWriteBits).await
}

/// Leaves a folder that is forgotten while it keeps no parts a plain directory with its
/// write bits: the daemon's last end, a build that failed or a take-down took them off,
/// and no mount is there whose stop would give them back. Off the runtime's threads: a
/// mount a daemon that died left over the folder is taken off first
/// (`mount::unmount_stale_mount`), then the write bits are given back to everything in the
/// folder on disk (`mount::restore_write_bits`, which must be given the folder itself and
/// not a mount over it). No store is open then, so there is none to close. Called by the
/// owner of the folder's state, inside a change of the folder.
pub(crate) async fn forget_folder_without_mount(root: &SyncRoot) {
    let folder = root.path.clone();
    let restored = tokio::task::spawn_blocking(move || {
        crate::mount::unmount_stale_mount(&folder);
        crate::mount::restore_write_bits(&folder);
    });
    if let Err(error) = restored.await {
        tracing::warn!("the task giving {} its write bits back failed: {error}", root.path.display());
    }
}

/// How many changes of the folder whose store is at `tree_db` are unsent, read through a
/// reader opened for the question: for the warning before a Forget of a folder that keeps
/// no open store (`SyncService::count_unsent`). Blocking. Where there is no file, and
/// where the file cannot be opened as a store of this version (an older version, no
/// database), nothing is unsent: a sync would rebuild that store empty, so it holds
/// nothing a sync could send. Any other failure — a permission, the disk — is the error.
/// It refuses nothing: a Forget is never refused for what is unsent.
pub(crate) fn count_unsent_of_closed_store(tree_db: &std::path::Path) -> Result<UnsentTotal, konedrive_tree::TreeError> {
    match konedrive_tree::decide_old_store(&konedrive_tree::find_stored_version(tree_db)?) {
        // A store of this version, or of the one before, which a start migrates in place and
        // whose rows this count reads as they are: what it cannot be asked is an error, not
        // nothing.
        konedrive_tree::OldStoreDecision::Use | konedrive_tree::OldStoreDecision::Migrate => konedrive_tree::TreeStore::open_read_only(tree_db)?.read_items(|items| items.count_unsent()),
        // No store, or one a sync would rebuild empty.
        konedrive_tree::OldStoreDecision::Make | konedrive_tree::OldStoreDecision::Rebuild => Ok(UnsentTotal::default()),
    }
}

#[cfg(test)]
mod tests;
