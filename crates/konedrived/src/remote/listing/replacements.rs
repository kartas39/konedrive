//! Downloaded files that changed in OneDrive, replaced after the cycle.
//!
//! [`Replacements`] is the bookkeeping, one state behind one lock: which
//! files are being replaced, which wait for a worker, which failed and are
//! tried again. The workers are `Listing`'s, since a replacement needs the
//! folder, the drive and the report.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::Listing;
use crate::folder::disk::Disk;
use crate::hydration::tracked::Tracked;
use crate::remote::materialize::{replace_until, Failure, FailureReason, Leased, ReplaceOutcome, Replacement};
use crate::status::activity::{self, Kind};
use crate::status::snapshot::{ReplacementNote, SyncStateHandle};

/// Replacements at once (issue #39): the queue's workers. A guess; each
/// also waits for a slot of the account's transfer pool.
pub const REPLACE_WORKERS: usize = 8;

/// The replacements of one folder: under way, waiting, failed.
///
/// A file is replaced by one worker at a time. A newer version of a file
/// whose replacement is under way is fetched when that one ends. One that
/// failed is kept, said in the status ([`ReplacementNote`]) and issued again
/// after every cycle until it goes through or is no longer needed.
pub(super) struct Replacements {
    state: Mutex<State>,
    /// Cancelled when the poller stops: no replacement starts, and one under
    /// way is given up where it waits.
    stop: CancellationToken,
    /// Where the note of what failed is published.
    status: SyncStateHandle,
}

#[derive(Default)]
struct State {
    /// The replacements under way or queued, by item id.
    running: HashMap<String, InFlight>,
    /// Those that failed, by item id.
    failed: HashMap<String, Failed>,
    /// Those waiting for a worker.
    queue: VecDeque<Replacement>,
    /// How many workers run.
    workers: usize,
    /// The workers' tasks ([`REPLACE_WORKERS`] at most, issue #39).
    tasks: JoinSet<()>,
    /// Counts the failures said, so that the note quotes the newest.
    said: u64,
}

/// A replacement under way: the version it fetches, and a newer version of
/// the same file that arrived meanwhile, fetched when it ends.
struct InFlight {
    ctag: String,
    next: Option<Replacement>,
}

/// A replacement that failed, and why; `said` orders them by when each was
/// news.
struct Failed {
    replacement: Replacement,
    failure: Failure,
    said: u64,
}

/// What recording a replacement's end found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Recorded {
    /// It is news for the activity log (I1): a replacement that went through
    /// is; a failure is only when it is new for this file — a first failure,
    /// one for a newer version, or one for another reason than the last. The
    /// same failure on every retry is said once, and the status note keeps
    /// saying it.
    pub news: bool,
    /// It ended with nothing to do, and asks for a Full reconcile: a Changed
    /// scope never looks at that file again, and only a Full one works out
    /// from the disk and the tree what it needs now — the replacement again,
    /// an update of its placeholder, a rescue, or nothing.
    pub full: bool,
}

impl State {
    /// `replacement` is under way from now on, and waits for a worker.
    fn enqueue(&mut self, replacement: Replacement) {
        self.running.insert(replacement.id.clone(), InFlight { ctag: replacement.ctag.clone(), next: None });
        self.queue.push_back(replacement);
    }
}

impl Replacements {
    pub(super) fn new(status: SyncStateHandle) -> Self {
        Self { state: Mutex::new(State::default()), stop: CancellationToken::new(), status }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The poller stops: no replacement starts from now on, and one under
    /// way is given up where it waits.
    pub(super) fn stop(&self) {
        self.stop.cancel();
    }

    /// Cancelled once the poller stops.
    pub(super) fn stop_token(&self) -> &CancellationToken {
        &self.stop
    }

    /// Queues a replacement for each file of `fresh` not being replaced
    /// already, and for each that failed and has no fresher replacement
    /// standing for it; a newer version of a file whose replacement is under
    /// way waits for that one to end. Starts workers for the queue, each the
    /// future `worker` makes, up to [`REPLACE_WORKERS`] in all (issue #39): a
    /// delta changing thousands of files starts a few tasks, not one each.
    pub(super) fn admit<W: Future<Output = ()> + Send + 'static>(&self, fresh: Vec<Replacement>, worker: impl Fn() -> W) {
        let mut state = self.state();
        let fresh_ids: HashSet<String> = fresh.iter().map(|r| r.id.clone()).collect();
        let retries: Vec<Replacement> =
            state.failed.values().filter(|failed| !fresh_ids.contains(&failed.replacement.id)).map(|failed| failed.replacement.clone()).collect();
        for replacement in fresh {
            match state.running.get_mut(&replacement.id) {
                None => state.enqueue(replacement),
                // This very version is on its way; nothing newer after it.
                Some(running) if running.ctag == replacement.ctag => running.next = None,
                Some(running) => running.next = Some(replacement),
            }
        }
        for retry in retries {
            if !state.running.contains_key(&retry.id) {
                state.enqueue(retry);
            }
        }
        let starting = REPLACE_WORKERS.saturating_sub(state.workers).min(state.queue.len());
        if starting == 0 {
            return;
        }
        state.workers += starting;
        // Finished ones are kept only for `join`.
        while state.tasks.try_join_next().is_some() {}
        for _ in 0..starting {
            state.tasks.spawn(worker());
        }
    }

    /// The next replacement for a worker; `None` ends the worker: the queue
    /// is empty, or the poller stopped — what was left in the queue then goes
    /// with no outcome.
    pub(super) fn next(&self) -> Option<Replacement> {
        let mut state = self.state();
        if self.stop.is_cancelled() {
            let left: Vec<Replacement> = state.queue.drain(..).collect();
            for replacement in left {
                state.running.remove(&replacement.id);
            }
        }
        let next = state.queue.pop_front();
        if next.is_none() {
            state.workers -= 1;
        }
        next
    }

    /// Records what the replacement of `replacement` came to, and publishes
    /// the note of what failed. One that failed is kept, to be issued again
    /// after every cycle ([`admit`](Self::admit)); a Full reconcile would add
    /// nothing but a scan of the whole folder.
    pub(super) fn record(&self, replacement: &Replacement, outcome: &ReplaceOutcome) -> Recorded {
        let mut state = self.state();
        let recorded = match outcome {
            ReplaceOutcome::Replaced => Recorded { news: true, full: false },
            ReplaceOutcome::Current => Recorded { news: false, full: true },
            // Open somewhere: its deferred change brings it back at the next cycle.
            ReplaceOutcome::Busy => Recorded { news: false, full: false },
            ReplaceOutcome::Failed(failure) => {
                let before = state.failed.get(&replacement.id);
                let same = before.is_some_and(|before| before.replacement.ctag == replacement.ctag && before.failure.reason == failure.reason);
                Recorded { news: !same, full: false }
            }
        };
        match outcome {
            ReplaceOutcome::Failed(failure) => {
                // The same failure again keeps its place and its words.
                if recorded.news {
                    tracing::warn!("{}: {}", replacement.rel.display(), failure.text);
                    state.said += 1;
                    let said = state.said;
                    state.failed.insert(replacement.id.clone(), Failed { replacement: replacement.clone(), failure: failure.clone(), said });
                } else {
                    tracing::debug!("{}: still {}", replacement.rel.display(), failure.text);
                }
            }
            ReplaceOutcome::Replaced | ReplaceOutcome::Current | ReplaceOutcome::Busy => {
                state.failed.remove(&replacement.id);
            }
        }
        // Under the lock, so that two workers' notes are published in the order they were made.
        let note = state
            .failed
            .values()
            .max_by_key(|failed| failed.said)
            .map(|newest| ReplacementNote { files: state.failed.len(), why: newest.failure.text.clone() });
        self.status.update_if_changed(|s| s.cycle.replacement_note = note);
        recorded
    }

    /// The replacement of `replacement` is over. A newer version of the file
    /// that arrived meanwhile is queued in its place, unless the poller
    /// `stopped` it.
    pub(super) fn done(&self, replacement: &Replacement, stopped: bool) {
        let mut state = self.state();
        let next = state.running.remove(&replacement.id).and_then(|running| running.next).filter(|_| !stopped);
        if let Some(next) = next {
            state.enqueue(next);
        }
    }

    /// Waits for the workers, and for the newer versions they hand over to.
    pub(super) async fn join(&self) {
        loop {
            let mut tasks = std::mem::take(&mut self.state().tasks);
            if tasks.is_empty() {
                return;
            }
            while tasks.join_next().await.is_some() {}
        }
    }
}

impl Listing {
    /// Issues the replacements a cycle found (spec §7.3), and again those
    /// that failed.
    pub(super) fn spawn_replacements(self: &Arc<Self>, fresh: Vec<Replacement>) {
        self.replacements.admit(fresh, || Arc::clone(self).replace_queued());
    }

    /// A replacement worker: takes the next file from the queue until it is
    /// empty. Cut short by `Poller::stop`: what is left in the queue goes
    /// with no outcome, and so does the replacement under way where it
    /// waits (`replace_until`). The worker itself is never dropped: file
    /// calls it has begun end, and are said, before it does, so nothing of
    /// it is at work once `join_replacements` returns.
    async fn replace_queued(self: Arc<Self>) {
        while let Some(replacement) = self.replacements.next() {
            let outcome = self.replace_one(&replacement).await;
            if let Some((outcome, event)) = &outcome {
                let recorded = self.replacements.record(&replacement, outcome);
                if recorded.full {
                    self.request_full();
                }
                if let Some(event) = event {
                    // A failure retried after every cycle is said once, not a
                    // minute (I1).
                    if recorded.news {
                        self.ctx.report.activity.record(vec![event.clone()]).await;
                    }
                    self.ctx.report.space.kick();
                }
            }
            self.replacements.done(&replacement, outcome.is_none());
        }
    }

    /// Replaces one file, shown in `Transfers` while it downloads (spec
    /// §16.2), and what the activity log would say of it: `updated` or
    /// `update-failed`. Whether it says it is [`Replacements::record`]'s to
    /// decide. `None` when the poller stopped it.
    async fn replace_one(&self, replacement: &Replacement) -> Option<(ReplaceOutcome, Option<activity::Event>)> {
        let shown = self.ctx.root.path.join(&replacement.rel).display().to_string();
        let tracked = Tracked::new(Arc::clone(&self.ctx.source), self.ctx.report.transfers.clone(), shown.clone());
        let outcome = self.replace_through(&tracked, replacement).await?;
        let size = tracked.fetched().unwrap_or(replacement.size);
        drop(tracked);
        let event = match &outcome {
            ReplaceOutcome::Replaced => Some(activity::event(Kind::Updated, shown, activity::human_size(size))),
            ReplaceOutcome::Failed(Failure { reason: FailureReason::NoSpace, .. }) => Some(activity::event(Kind::UpdateFailed, shown, activity::NO_DISK_SPACE)),
            ReplaceOutcome::Failed(failure) => Some(activity::event(Kind::UpdateFailed, shown, failure.text.clone())),
            ReplaceOutcome::Current | ReplaceOutcome::Busy => None,
        };
        Some((outcome, event))
    }

    async fn replace_through(&self, source: &Tracked, replacement: &Replacement) -> Option<ReplaceOutcome> {
        // A background download in the account's transfer pool; a large one also waits for
        // the large-file limit.
        let size = konedrive_graph::pool::Size::of(replacement.size);
        let stop = self.replacements.stop_token();
        let mut slot = stop.run_until_cancelled(self.ctx.drive.pool().acquire_sized(konedrive_graph::pool::Class::Download, size)).await?;
        // Opening reads the root's attribute to prove it is still this root:
        // on a blocking thread, like every open (part 1's).
        let (root, locked) = (self.ctx.root.clone(), self.ctx.locked);
        let disk = match tokio::task::spawn_blocking(move || Disk::open(&root, locked)).await {
            Ok(Ok(disk)) => disk,
            Ok(Err(e)) => return Some(ReplaceOutcome::Failed(Failure { reason: FailureReason::Folder, text: e.to_string() })),
            Err(e) => return Some(ReplaceOutcome::Failed(Failure { reason: FailureReason::Task, text: format!("the replacement task failed: {e}") })),
        };
        // Read-write mode: the swap under a write lease and the tree lock, and the new
        // version's deferred change into the base as it lands.
        let leased = self.ctx.writes.as_ref().map(|writes| Leased { tree_lock: &writes.tree_lock, store: &self.ctx.store });
        let outcome = replace_until(&disk, &self.ctx.locks, source, replacement, leased.as_ref(), stop).await?;
        if matches!(outcome, ReplaceOutcome::Replaced) {
            // A new version is a new inode: the item's recorded one now.
            crate::local::record_replaced_async(&disk, &self.ctx.store, &replacement.id, &replacement.rel).await;
            slot.succeeded();
        }
        Some(outcome)
    }

    /// Stops every replacement: none starts, and one under way is given up
    /// where it waits (`Poller::stop`, which then waits for them).
    pub(super) fn stop_replacements(&self) {
        self.replacements.stop();
    }

    /// Waits for the replacements under way, and for the newer versions they
    /// hand over to (tests; and `Poller::stop` after stopping them).
    pub async fn join_replacements(&self) {
        self.replacements.join().await;
    }
}

#[cfg(test)]
mod tests;
