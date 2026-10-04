use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, Notify};
use tokio_util::sync::CancellationToken;

use super::{CycleError, Listing};
use crate::status::snapshot::OutboxNote;

/// How often the poller runs a cycle, and how soon after a failure.
#[derive(Debug, Clone)]
pub struct Schedule {
    pub interval: Duration,
    /// The waits after the first, second, … failure in a row; after the last,
    /// the ordinary interval.
    pub retry: Vec<Duration>,
    /// The interval while the notification socket is up (`live`): the poll is only the safety
    /// net for an event OneDrive never sent.
    pub live_interval: Duration,
    /// The live task's waits; `None` runs none, and the poll alone brings changes (the tests
    /// that do not ask for it).
    pub live: Option<crate::remote::live::Timing>,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
            retry: vec![Duration::from_secs(5), Duration::from_secs(15), Duration::from_secs(30)],
            live_interval: Duration::from_secs(300),
            live: Some(crate::remote::live::Timing::default()),
        }
    }
}

impl Schedule {
    /// The poll alone, every `interval`, with these retries: no live task.
    pub fn polled(interval: Duration, retry: Vec<Duration>) -> Self {
        Self { interval, retry, live: None, ..Self::default() }
    }
}

/// Runs a cycle at once, then every `interval` (`live_interval` while the notification
/// socket is up), at once on `refresh()`, and on the retry schedule after a failure
/// (Poller). The live task (`live`), when the schedule has one, runs alongside and stops
/// with it.
pub struct Poller {
    handle: PollHandle,
    /// `None` once it has been waited for.
    task: Option<tokio::task::JoinHandle<()>>,
    listing: Arc<Listing>,
    live: Option<crate::remote::live::Live>,
}

/// What may be asked of a running poller by whoever does not own it: a cycle now, and to
/// stop. Only its owner can wait for it ([`Poller::join`]). Asking a stopped poller does
/// nothing.
#[derive(Clone)]
pub struct PollHandle {
    refresh: Arc<Notify>,
    cancel: CancellationToken,
    /// Weak: a handle is held by parts the listing itself reaches (the outbox worker's
    /// host), and must not keep the listing, and its tree store, alive after the poller.
    listing: std::sync::Weak<Listing>,
    live: Option<Arc<Notify>>,
}

impl PollHandle {
    /// A cycle now.
    pub fn refresh(&self) {
        self.refresh.notify_one();
    }

    /// The pause, the hold or the network may have changed: the live task looks again.
    pub fn wake_live(&self) {
        if let Some(live) = &self.live {
            live.notify_one();
        }
    }

    /// A cycle now whose reconcile is Full: it places again what is missing
    /// here though OneDrive did not change it (`RestoreDeletes`, an item whose
    /// local object was forgotten).
    pub fn refresh_full(&self) {
        if let Some(listing) = self.listing.upgrade() {
            listing.request_full();
        }
        self.refresh.notify_one();
    }

    /// Tells the poller, the live task and every replacement under way to stop, without
    /// waiting for them: a cycle gives up where it waits, and a file call it has begun
    /// ends first.
    pub fn cancel(&self) {
        self.cancel.cancel();
        if let Some(listing) = self.listing.upgrade() {
            listing.stop_replacements();
        }
    }
}

impl Poller {
    pub fn start(listing: Arc<Listing>, schedule: Schedule) -> Self {
        let refresh = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        // Whether the notification socket is up: the live task says, the poller's task reads.
        let up = Arc::new(watch::channel(false).0);
        let live = schedule.live.clone().map(|timing| {
            let ctx = crate::remote::live::LiveContext {
                drive: listing.ctx.drive.clone(),
                store: listing.ctx.store.clone(),
                running: Arc::clone(&listing.ctx.running),
                state: listing.ctx.state.clone(),
                refresh: Arc::clone(&refresh),
                up: Arc::clone(&up),
            };
            crate::remote::live::Live::start(ctx, timing, cancel.clone())
        });
        let task = tokio::spawn(run(Arc::clone(&listing), schedule, Arc::clone(&refresh), cancel.clone(), up.subscribe()));
        let handle = PollHandle { refresh, cancel, listing: Arc::downgrade(&listing), live: live.as_ref().map(crate::remote::live::Live::waker) };
        Self { handle, task: Some(task), listing, live }
    }

    /// What others may ask of this poller without owning it.
    pub fn handle(&self) -> PollHandle {
        self.handle.clone()
    }

    pub fn refresh(&self) {
        self.handle.refresh();
    }

    /// See [`PollHandle::wake_live`].
    pub fn wake_live(&self) {
        self.handle.wake_live();
    }

    /// See [`PollHandle::refresh_full`].
    pub fn refresh_full(&self) {
        self.handle.refresh_full();
    }

    /// Stops the poller, the live task and every replacement under way, and waits for them.
    pub async fn stop(mut self) {
        self.join().await;
    }

    /// [`stop`](Self::stop) for an owner that may be cut while it waits: what was waited
    /// for is not waited for again, and the rest is by the next call.
    pub async fn join(&mut self) {
        self.handle.cancel();
        if let Some(task) = self.task.as_mut() {
            let _ = task.await;
            self.task = None;
        }
        if let Some(live) = self.live.as_mut() {
            live.join().await;
            self.live = None;
        }
        self.listing.ctx.state.set_live_changes(crate::status::snapshot::LiveChanges::Off);
        self.listing.join_replacements().await;
    }
}

impl Drop for Poller {
    /// A poller dropped without [`stop`](Poller::stop) still ends: it is told to stop,
    /// and nobody waits for it.
    fn drop(&mut self) {
        self.handle.cancel();
    }
}

/// Whether a read-only folder holds changes waiting to upload: its cycles wait meanwhile,
/// and its `LastError` says why ([`run`]). A store that cannot be read holds them back too.
/// What it said goes once none wait.
async fn held_back(listing: &Listing) -> bool {
    if !listing.ctx.mode.is_read_only() {
        return false;
    }
    let store = listing.ctx.store.clone();
    let waiting = tokio::task::spawn_blocking(move || store.call_blocking(move |s| s.outbox_len())).await.ok().and_then(Result::ok);
    let note = match waiting {
        Some(0) => None,
        Some(n) => Some(OutboxNote::HeldBack(n)),
        None => Some(OutboxNote::Unreadable),
    };
    if listing.ctx.state.get().outbox.note != note {
        listing.ctx.state.update(|s| s.outbox.note = note);
    }
    waiting != Some(0)
}

async fn run(
    listing: Arc<Listing>,
    schedule: Schedule,
    refresh: Arc<Notify>,
    cancel: CancellationToken,
    mut up: watch::Receiver<bool>,
) {
    let mut failures = 0usize;
    // False once the sender is gone: `changed` would then answer at once, for good.
    let mut up_open = true;
    loop {
        // Paused (`docs/design/writes.md` §11): OneDrive is not asked, so nothing is
        // replaced either, until the pause ends or `Resume()` nudges.
        // The same for anything else that stops the account's background work.
        if let Some(stop) = listing.ctx.running.stop(&listing.ctx.store) {
            // A timed pause ends by the account's clock, as it is kept: whoever asks whether
            // it is over agrees. Looked at again after the poll's interval at the latest,
            // and no sooner than in a second.
            let clock = listing.ctx.running.clock();
            let timed = match stop {
                crate::conditions::running::Stop::Paused(until) if until > 0 => Some(until.max(clock.now().saturating_add(1))),
                _ => None,
            };
            let ended = async {
                match timed {
                    Some(at) => clock.sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = ended => {}
                () = tokio::time::sleep(schedule.interval) => {}
                () = refresh.notified() => {}
                () = cancel.cancelled() => return,
            }
            continue;
        }
        // A read-only folder that holds changes waiting to upload — a switch to read-only
        // nobody forced: a sign-out, the gate, `config.toml` — runs no
        // cycle: the read phase's reconcile would put back the moves and deletes they
        // describe. It waits for read-write again, which sends them, or for the forced switch
        // that drops them; `LastError` says so meanwhile.
        if held_back(&listing).await {
            tokio::select! {
                () = tokio::time::sleep(schedule.interval) => {}
                () = refresh.notified() => {}
                () = cancel.cancelled() => return,
            }
            continue;
        }
        let result = listing.cycle(&cancel).await;
        let wait = match &result {
            Ok(_) => {
                failures = 0;
                None
            }
            Err(CycleError::Cancelled) => return,
            Err(e) => {
                tracing::warn!("the sync with OneDrive failed: {e}");
                let wait = schedule.retry.get(failures).copied().unwrap_or(schedule.interval);
                failures += 1;
                Some(wait)
            }
        };
        // Counted from the end of the cycle: a socket that goes down makes the next cycle
        // due at most `interval` after it, and one that comes up puts it off.
        let since = tokio::time::Instant::now();
        loop {
            let wait = wait.unwrap_or(if *up.borrow_and_update() { schedule.live_interval } else { schedule.interval });
            tokio::select! {
                () = tokio::time::sleep_until(since + wait) => break,
                () = refresh.notified() => break,
                () = cancel.cancelled() => return,
                changed = up.changed(), if up_open => up_open = changed.is_ok(),
            }
        }
    }
}

#[cfg(test)]
mod tests;
