//! The task that runs a listing cycle into the rows on a schedule, on a refresh and when
//! the live socket says so: what `listing/poller.rs` is today over `Listing`.
//!
//! **State owned:** the task, its handle's signals, and the live task beside it. Stopped
//! by its owner alone ([`ListingPoller::stop`]); anyone with a [`ListingPollHandle`] may
//! ask for a cycle.
//!
//! **Answers for:** when a cycle runs: at once at its start, every interval, on the retry
//! schedule after a failure, on a refresh, and while the account's work runs
//! (`conditions::running`). It holds no cycle back for a read-only folder with unsent
//! changes (today's `held_back`, `listing/poller.rs:170-184`, goes: design §13 (3)).
//!
//! **Tested without the rest**, with a cycle the test holds and the paused clock:
// test: a_cycle_runs_at_once_and_then_every_interval
// test: a_refresh_runs_a_cycle_now
// test: a_failed_cycle_is_tried_again_on_the_retry_schedule
// test: a_full_refresh_asks_the_store_for_a_full_listing_before_the_cycle
// test: a_stop_cancels_the_cycle_between_two_requests_and_waits_for_the_page_being_written
// test: no_cycle_runs_while_the_accounts_work_is_stopped
#![allow(dead_code, unused_variables)]

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::listing::Schedule;
use super::listing_cycle::ListingCycle;
use crate::conditions::running::Running;

/// What may be asked of a running poller by whoever does not own it. Asking a stopped
/// poller does nothing.
#[derive(Clone)]
pub struct ListingPollHandle {
    refresh: Arc<tokio::sync::Notify>,
    cancel: CancellationToken,
    /// Weak: a handle is held by parts the cycle itself reaches, and must not keep the
    /// cycle, and its store, alive after the poller.
    cycle: std::sync::Weak<ListingCycle>,
    live: Option<Arc<tokio::sync::Notify>>,
}

impl ListingPollHandle {
    /// A handle with no poller and no cycle behind it, and the count of the cycles it was
    /// asked for: what the tests of the plan's task and of the folder's start give in
    /// place of a poller.
    #[cfg(test)]
    pub(crate) fn for_tests() -> (Self, Arc<std::sync::atomic::AtomicUsize>) {
        todo!()
    }

    /// A cycle now: the changes since the delta link. Called by the plan's task after a
    /// refusal that asks for a cycle, by `Refresh()` and by the sign-in.
    pub fn refresh(&self) {
        todo!()
    }

    /// A cycle now, after the store was asked for a full listing by the caller
    /// (`ItemStore::request_full_listing`): the cycle begins it by the store's request,
    /// the handle only wakes.
    pub fn refresh_after_full_listing_request(&self) {
        todo!()
    }

    /// The live socket is to be looked at again: the network is back.
    pub fn wake_live(&self) {
        todo!()
    }
}

/// The poller of one folder's listing.
pub struct ListingPoller {
    handle: ListingPollHandle,
    task: Option<tokio::task::JoinHandle<()>>,
    cycle: Arc<ListingCycle>,
    live: Option<crate::remote::live::Live>,
}

impl ListingPoller {
    /// Starts the task: a first cycle at once. Called by the folder's start, before the
    /// plan's task.
    pub fn start(cycle: Arc<ListingCycle>, schedule: Schedule, running: Arc<Running>) -> Self {
        todo!()
    }

    /// What others ask the poller through.
    pub fn handle(&self) -> ListingPollHandle {
        todo!()
    }

    /// Stops the poller and waits for it: a cycle is cancelled between two requests; the
    /// page being written is written whole. Called first in the stop of a folder's sync.
    pub async fn stop(self) {
        todo!()
    }
}
