//! The task that runs a listing cycle into the rows on a schedule, on a refresh and when
//! the live socket says so: what `listing/poller.rs` is today over `Listing`.
//!
//! **State owned:** the task, its handle's signals, and the live task beside it. Stopped
//! by its owner alone ([`ListingPoller::stop`]); anyone with a [`ListingPollHandle`] may
//! ask for a cycle.
//!
//! **Answers for:** when a cycle runs: at once at its start, every interval, on the retry
//! schedule after a failure, on a refresh, and while the account's work runs
//! (`conditions::running`). How long it waits is decided in `poll_wait`, from typed cases.
//! While the account's work is stopped the poller wakes by the clock (a timed pause), at
//! its interval and on a refresh, and by nothing else: whoever ends a pause or a hold asks
//! for a cycle (`account_folder/pause.rs`, `show_pause`), or the first cycle after it comes
//! up to an interval late.
//! A refresh given while a cycle runs is kept, and several are one: one more cycle runs
//! after the one under way. It holds no cycle back for a read-only folder with unsent
//! changes (today's `held_back`, `listing/poller.rs:170-184`, goes: design §13 (3)).
//!
//! **Tested without the rest**, over a real cycle, the fixture's store and the fake
//! OneDrive, on the real clock with short waits (a request to the fake is real I/O):
// test: a_cycle_runs_at_once_and_then_every_interval
// test: a_refresh_runs_a_cycle_now
// test: a_refresh_given_while_a_cycle_runs_runs_another_after_it
// test: a_failed_cycle_is_tried_again_on_the_retry_schedule
// test: a_full_refresh_asks_the_store_for_a_full_listing_before_the_cycle
// test: a_stop_cancels_the_cycle_between_two_requests_and_waits_for_the_page_being_written
// test: no_cycle_runs_while_the_accounts_work_is_stopped
// test: a_handle_with_no_poller_counts_the_cycles_it_is_asked_for

use std::time::Duration;
use std::sync::Arc;

use tokio::sync::{watch, Notify};
use tokio_util::sync::CancellationToken;

use super::listing_cycle::CycleError;
use super::listing_cycle::ListingCycle;
use super::live::{Live, LiveContext};
use crate::conditions::running::Running;
use crate::status::snapshot::LiveChanges;

/// How long the poller waits after a cycle and through a pause.
mod poll_wait;

use poll_wait::{decide_cycle_wait, decide_pause_wait, LastCycle, NotificationSocket, PauseWait};

/// What may be asked of a running poller by whoever does not own it. Asking a stopped
/// poller does nothing.
#[derive(Clone)]
pub(crate) struct ListingPollHandle {
    refresh: Arc<tokio::sync::Notify>,
    cancel: CancellationToken,
    live: Option<Arc<tokio::sync::Notify>>,
    /// How many cycles this handle was asked for, in a handle made by
    /// [`for_tests`](Self::for_tests): raised by [`refresh`](Self::refresh) and
    /// [`refresh_after_full_listing_request`](Self::refresh_after_full_listing_request)
    /// before they wake the poller. `None` in every handle a poller gave.
    #[cfg(test)]
    asked_cycle_count: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

impl ListingPollHandle {
    /// A handle with no poller and no cycle behind it, and the count of the cycles it was
    /// asked for: what the tests of the plan's task and of the folder's start give in
    /// place of a poller.
    #[cfg(test)]
    pub(crate) fn for_tests() -> (Self, Arc<std::sync::atomic::AtomicUsize>) {
        let asked_cycle_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handle = ListingPollHandle { refresh: Arc::new(tokio::sync::Notify::new()), cancel: CancellationToken::new(), live: None, asked_cycle_count: Some(Arc::clone(&asked_cycle_count)) };
        (handle, asked_cycle_count)
    }

    /// A cycle now: the changes since the delta link. Called by the plan's task after a
    /// refusal that asks for a cycle, by `Refresh()` and by the sign-in. In a test build
    /// it first raises `asked_cycle_count` where the handle has one: the count of a
    /// handle made by `for_tests` is of these calls.
    pub fn refresh(&self) {
        #[cfg(test)]
        if let Some(asked_cycle_count) = &self.asked_cycle_count {
            asked_cycle_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.refresh.notify_one();
    }

    /// A cycle now, after the store was asked for a full listing by the caller
    /// (`ItemStore::request_full_listing`): the cycle begins it by the store's request,
    /// the handle only wakes. Counted in `asked_cycle_count` as [`refresh`](Self::refresh)
    /// is.
    pub(crate) fn refresh_after_full_listing_request(&self) {
        self.refresh();
    }

    /// The live socket is to be looked at again: the network is back.
    pub(crate) fn wake_live(&self) {
        if let Some(live) = &self.live {
            live.notify_one();
        }
    }
}

/// The poller of one folder's listing.
pub(crate) struct ListingPoller {
    handle: ListingPollHandle,
    /// `None` once it has been waited for.
    task: Option<tokio::task::JoinHandle<()>>,
    cycle: Arc<ListingCycle>,
    live: Option<Live>,
}

impl ListingPoller {
    /// Starts the task: a first cycle at once. Called by the folder's start, before the
    /// plan's task.
    pub fn start(cycle: Arc<ListingCycle>, schedule: Schedule, running: Arc<Running>) -> Self {
        let refresh = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        // Whether the notification socket is up: the live task says, the poller's task reads.
        let up = Arc::new(watch::channel(false).0);
        let live = schedule.live.clone().map(|timing| {
            let context = LiveContext { drive: cycle.drive().clone(), running: Arc::clone(&running), state: cycle.state().clone(), refresh: Arc::clone(&refresh), up: Arc::clone(&up) };
            Live::start(context, timing, cancel.clone())
        });
        let task = tokio::spawn(run_cycles(Arc::clone(&cycle), schedule, running, Arc::clone(&refresh), cancel.clone(), up.subscribe()));
        let handle = ListingPollHandle {
            refresh,
            cancel,
            live: live.as_ref().map(Live::waker),
            #[cfg(test)]
            asked_cycle_count: None,
        };
        ListingPoller { handle, task: Some(task), cycle, live }
    }

    /// What others ask the poller through.
    pub(crate) fn handle(&self) -> ListingPollHandle {
        self.handle.clone()
    }

    /// Stops the poller and waits for it: a cycle is cancelled between two requests; the
    /// page being written is written whole. Called first in the stop of a folder's sync.
    pub(crate) async fn stop(mut self) {
        self.handle.cancel.cancel();
        if let Some(task) = self.task.take() {
            if let Err(error) = task.await {
                tracing::warn!("the task of the listing's poller failed: {error}");
            }
        }
        if let Some(mut live) = self.live.take() {
            live.join().await;
        }
        self.cycle.state().set_live_changes(LiveChanges::Off);
    }
}

impl Drop for ListingPoller {
    /// A poller dropped without [`stop`](ListingPoller::stop) still ends: it is told to
    /// stop, and nobody waits for it.
    fn drop(&mut self) {
        self.handle.cancel.cancel();
    }
}

/// The poller's task: a cycle at once, then when the next is due
/// ([`decide_cycle_wait`]) or at once on a refresh; none while the account's work is
/// stopped, which is looked at again when a timed pause ends ([`decide_pause_wait`]),
/// after the interval and on a refresh. As today's `run` (`listing/poller.rs:186`)
/// without what held a read-only folder's cycles back.
async fn run_cycles(cycle: Arc<ListingCycle>, schedule: Schedule, running: Arc<Running>, refresh: Arc<Notify>, cancel: CancellationToken, mut up: watch::Receiver<bool>) {
    let mut failure_count = 0usize;
    // False once the sender is gone: `changed` would then answer at once, for good.
    let mut is_up_open = true;
    loop {
        // Paused, or held: OneDrive is not asked until that ends or a refresh nudges.
        if let Some(stop) = running.stop() {
            let clock = running.clock();
            let pause_wait = decide_pause_wait(stop, clock.now());
            let pause_ended = async {
                match pause_wait {
                    PauseWait::UntilClock(at) => clock.sleep_until(at).await,
                    PauseWait::NotByClock => std::future::pending().await,
                }
            };
            tokio::select! {
                () = pause_ended => {}
                () = tokio::time::sleep(schedule.interval) => {}
                () = refresh.notified() => {}
                () = cancel.cancelled() => return,
            }
            continue;
        }
        let last_cycle = match cycle.run(&cancel).await {
            Ok(_) => {
                failure_count = 0;
                LastCycle::Ended
            }
            Err(CycleError::Cancelled) => return,
            Err(error) => {
                tracing::warn!("the listing of OneDrive failed: {error}");
                failure_count += 1;
                LastCycle::Failed { failures_before: failure_count - 1 }
            }
        };
        // Counted from the end of the cycle: a socket that goes down makes the next cycle
        // due at most `interval` after it, and one that comes up puts it off.
        let cycle_ended_at = tokio::time::Instant::now();
        loop {
            let socket = if *up.borrow_and_update() { NotificationSocket::Up } else { NotificationSocket::Down };
            let wait = decide_cycle_wait(&schedule, last_cycle, socket);
            tokio::select! {
                () = tokio::time::sleep_until(cycle_ended_at + wait) => break,
                () = refresh.notified() => break,
                () = cancel.cancelled() => return,
                changed = up.changed(), if is_up_open => is_up_open = changed.is_ok(),
            }
        }
    }
}

/// How often the poller runs a cycle, and how soon after a failure.
#[derive(Debug, Clone)]
pub(crate) struct Schedule {
    pub(crate) interval: Duration,
    /// The waits after the first, second, … failure in a row; after the last,
    /// the ordinary interval.
    pub(crate) retry: Vec<Duration>,
    /// The interval while the notification socket is up (`live`): the poll is only the safety
    /// net for an event OneDrive never sent.
    pub(crate) live_interval: Duration,
    /// The live task's waits; `None` runs none, and the poll alone brings changes (the tests
    /// that do not ask for it).
    pub(crate) live: Option<super::live::Timing>,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
            retry: vec![Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(30)],
            live_interval: Duration::from_secs(300),
            live: Some(super::live::Timing::default()),
        }
    }
}

impl Schedule {
    /// The poll alone, every `interval`, with these retries: no live task.
    pub(crate) fn polled(interval: Duration, retry: Vec<Duration>) -> Self {
        Self { interval, retry, live: None, ..Self::default() }
    }
}

#[cfg(test)]
mod tests;
