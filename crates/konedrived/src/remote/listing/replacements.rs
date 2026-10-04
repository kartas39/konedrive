use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::status::activity::{self, Kind};
use crate::hydration::tracked::Tracked;
use crate::folder::disk::Disk;
use crate::remote::materialize::{replace_until, Leased, ReplaceOutcome, Replacement};
use super::Listing;

/// Replacements at once (issue #39): the queue's workers. A guess; each
/// also waits for a slot of the account's transfer pool.
pub const REPLACE_WORKERS: usize = 8;

/// A replacement under way: the version it fetches, and a newer version of
/// the same file that arrived meanwhile, fetched when it ends.
pub(super) struct InFlight {
    ctag: String,
    next: Option<Replacement>,
}

impl Listing {
    /// Starts a replacement for each file not being replaced already (spec
    /// §7.3), and retries those that failed. A newer version of a file whose
    /// replacement is under way is fetched when that one ends; a failed one
    /// is retried only when no fresher replacement of the file stands for it.
    /// They wait in one queue, worked by at most [`REPLACE_WORKERS`] tasks
    /// (issue #39): a delta changing thousands of files starts a few tasks,
    /// not one each.
    pub(super) fn spawn_replacements(self: &Arc<Self>, fresh: Vec<Replacement>) {
        let fresh_ids: HashSet<&str> = fresh.iter().map(|r| r.id.as_str()).collect();
        let retries: Vec<Replacement> = self
            .failed_replacements
            .lock()
            .unwrap()
            .values()
            .filter(|(failed, _)| !fresh_ids.contains(failed.id.as_str()))
            .map(|(failed, _)| failed.clone())
            .collect();
        let mut start = Vec::new();
        {
            let mut replacing = self.replacing.lock().unwrap();
            for replacement in fresh {
                match replacing.get_mut(&replacement.id) {
                    None => {
                        replacing.insert(replacement.id.clone(), InFlight { ctag: replacement.ctag.clone(), next: None });
                        start.push(replacement);
                    }
                    // This very version is on its way; nothing newer after it.
                    Some(running) if running.ctag == replacement.ctag => running.next = None,
                    Some(running) => running.next = Some(replacement),
                }
            }
            for replacement in retries {
                if !replacing.contains_key(&replacement.id) {
                    replacing.insert(replacement.id.clone(), InFlight { ctag: replacement.ctag.clone(), next: None });
                    start.push(replacement);
                }
            }
        }
        self.queue_replacements(start);
    }

    /// Queues `replacements`, each already in `replacing`, and starts
    /// workers for them up to [`REPLACE_WORKERS`].
    fn queue_replacements(self: &Arc<Self>, replacements: Vec<Replacement>) {
        if replacements.is_empty() {
            return;
        }
        let starting = {
            let mut queued = self.queued_replacements.lock().unwrap();
            queued.0.extend(replacements);
            let starting = REPLACE_WORKERS.saturating_sub(queued.1).min(queued.0.len());
            queued.1 += starting;
            starting
        };
        if starting == 0 {
            return;
        }
        let mut tasks = self.replacements.lock().unwrap();
        // Finished ones are kept only for `join_replacements`.
        while tasks.try_join_next().is_some() {}
        for _ in 0..starting {
            let this = Arc::clone(self);
            tasks.spawn(async move { this.replace_queued().await });
        }
    }

    /// A replacement worker: takes the next file from the queue until it is
    /// empty. Cut short by `Poller::stop`: what is left in the queue goes
    /// with no outcome, and so does the replacement under way where it
    /// waits (`replace_until`). The worker itself is never dropped: file
    /// calls it has begun end, and are said, before it does, so nothing of
    /// it is at work once `join_replacements` returns.
    async fn replace_queued(self: Arc<Self>) {
        loop {
            let replacement = {
                let mut queued = self.queued_replacements.lock().unwrap();
                match queued.0.pop_front().filter(|_| !self.cancel_replacements.is_cancelled()) {
                    Some(replacement) => replacement,
                    None => {
                        let left: Vec<Replacement> = queued.0.drain(..).collect();
                        queued.1 -= 1;
                        drop(queued);
                        let mut replacing = self.replacing.lock().unwrap();
                        for replacement in left {
                            replacing.remove(&replacement.id);
                        }
                        return;
                    }
                }
            };
            let outcome = self.replace_one(&replacement).await;
            let stopped = outcome.is_none();
            if let Some((outcome, event)) = outcome {
                // A failure retried after every cycle is said once, not a
                // minute (I1).
                let news = self.record_replacement(&replacement, outcome);
                if let Some(event) = event {
                    if news {
                        self.ctx.report.activity.record(vec![event]).await;
                    }
                    self.ctx.report.space.kick();
                }
            }
            let next = {
                let mut replacing = self.replacing.lock().unwrap();
                let next = replacing.remove(&replacement.id).and_then(|running| running.next).filter(|_| !stopped);
                if let Some(next) = &next {
                    replacing.insert(next.id.clone(), InFlight { ctag: next.ctag.clone(), next: None });
                }
                next
            };
            if let Some(next) = next {
                self.queued_replacements.lock().unwrap().0.push_back(next);
            }
        }
    }

    /// Replaces one file, shown in `Transfers` while it downloads (spec
    /// §16.2), and what the activity log would say of it: `updated` or
    /// `update-failed`. Whether it says it is [`record_replacement`]'s to
    /// decide. `None` when the poller stopped it.
    async fn replace_one(&self, replacement: &Replacement) -> Option<(ReplaceOutcome, Option<activity::Event>)> {
        let shown = self.ctx.root.path.join(&replacement.rel).display().to_string();
        let tracked = Tracked::new(Arc::clone(&self.ctx.source), self.ctx.report.transfers.clone(), shown.clone());
        let outcome = self.replace_through(&tracked, replacement).await?;
        let size = tracked.fetched().unwrap_or(replacement.size);
        drop(tracked);
        let event = match &outcome {
            ReplaceOutcome::Replaced => Some(activity::event(Kind::Updated, shown, activity::human_size(size))),
            ReplaceOutcome::Failed(why) => Some(activity::event(Kind::UpdateFailed, shown, why.clone())),
            ReplaceOutcome::NoSpace(_) => Some(activity::event(Kind::UpdateFailed, shown, activity::NO_DISK_SPACE)),
            ReplaceOutcome::Current | ReplaceOutcome::Busy => None,
        };
        Some((outcome, event))
    }

    async fn replace_through(&self, source: &Tracked, replacement: &Replacement) -> Option<ReplaceOutcome> {
        // A background download in the account's transfer pool; a large one also waits for
        // the large-file limit.
        let size = konedrive_graph::pool::Size::of(replacement.size);
        let stop = &self.cancel_replacements;
        let mut slot = stop.run_until_cancelled(self.ctx.drive.pool().acquire_sized(konedrive_graph::pool::Class::Download, size)).await?;
        // Opening reads the root's attribute to prove it is still this root:
        // on a blocking thread, like every open (part 1's).
        let (root, locked) = (self.ctx.root.clone(), self.ctx.locked);
        let disk = match tokio::task::spawn_blocking(move || Disk::open(&root, locked)).await {
            Ok(Ok(disk)) => disk,
            Ok(Err(e)) => return Some(ReplaceOutcome::Failed(e.to_string())),
            Err(e) => return Some(ReplaceOutcome::Failed(format!("the replacement task failed: {e}"))),
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

    /// What a replacement came to. One that ended with nothing to do asks for
    /// a Full reconcile: a Changed scope never looks at that file again, and
    /// only a Full one works out from the disk and the tree what it needs now
    /// — the replacement again, an update of its placeholder, a rescue, or
    /// nothing. One that failed is kept, said, and retried as it is after
    /// every cycle (`spawn_replacements`); a Full reconcile would add nothing
    /// but a scan of the whole folder.
    ///
    /// Whether it is news for the activity log (I1): a
    /// replacement that went through is; a failure is only when it is new
    /// for this file — a first failure, one for a newer version, or one for
    /// another reason than the last. The same failure on every retry is said
    /// once, and the status note keeps saying it.
    fn record_replacement(&self, replacement: &Replacement, outcome: ReplaceOutcome) -> bool {
        let mut failed = self.failed_replacements.lock().unwrap();
        let news = match &outcome {
            ReplaceOutcome::Replaced => true,
            ReplaceOutcome::Current | ReplaceOutcome::Busy => false,
            ReplaceOutcome::Failed(why) | ReplaceOutcome::NoSpace(why) => !matches!(
                failed.get(&replacement.id),
                Some((before, said)) if before.ctag == replacement.ctag && said == why
            ),
        };
        match outcome {
            ReplaceOutcome::Replaced => {
                failed.remove(&replacement.id);
            }
            ReplaceOutcome::Current => {
                failed.remove(&replacement.id);
                self.needs_full.store(true, Ordering::SeqCst);
            }
            // Open somewhere: its deferred change brings it back at the next cycle.
            ReplaceOutcome::Busy => {
                failed.remove(&replacement.id);
            }
            ReplaceOutcome::Failed(why) | ReplaceOutcome::NoSpace(why) => {
                if news {
                    tracing::warn!("{}: {why}", replacement.rel.display());
                } else {
                    tracing::debug!("{}: still {why}", replacement.rel.display());
                }
                failed.insert(replacement.id.clone(), (replacement.clone(), why));
            }
        }
        let note = match failed.values().next() {
            None => String::new(),
            Some((_, why)) => format!("{} file(s) changed in OneDrive could not be updated here yet: {why}", failed.len()),
        };
        self.ctx.state.update(|s| s.replacement_note = note);
        news
    }

    /// Waits for the replacements under way, and for the newer versions they
    /// hand over to (tests; and `Poller::stop` after cancelling them).
    pub async fn join_replacements(&self) {
        loop {
            let mut set = std::mem::take(&mut *self.replacements.lock().unwrap());
            if set.is_empty() {
                return;
            }
            while set.join_next().await.is_some() {}
        }
    }
}

#[cfg(test)]
mod tests;
