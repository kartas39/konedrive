//! The examiner thread: hands what the reader, the cycle and the folder hand
//! over to the [`Sink`], one examination at a time, merging whatever queued
//! meanwhile. What is examined when is the [`Schedule`]'s; this thread waits,
//! calls the sink, and says what came of it (the status, a flush's answer).

use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use super::schedule::{Outcome, Schedule, Take};
use super::{Shared, Sink, Timing, ToExaminer};
use crate::local::Batch;

/// However the examiner thread ends, the reader ends with it, since it would
/// hand over to nobody; and an end nobody asked for (a panic in the
/// examination) is said, as the reader's is (`WatchStatus::stopped`).
struct Ending(Arc<Shared>);

impl Drop for Ending {
    fn drop(&mut self) {
        let shared = &self.0;
        // This runs while a panic unwinds. The status is read and written
        // through a poisoned lock too; and a panic of the status hook is kept
        // in here, since one that left this drop would abort the daemon.
        let said = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if !shared.stopping() && !shared.status().root_gone {
                tracing::error!("the examiner of local changes stopped unexpectedly");
                shared.update(|s| {
                    s.stopped = true;
                    s.failing = None;
                });
            }
        }));
        if said.is_err() {
            tracing::error!("the watcher's status hook panicked while the examiner was ending");
        }
        shared.stop();
    }
}

/// Runs until the watcher stops, every sender is gone, or the folder went.
pub(super) fn run(rx: mpsc::Receiver<ToExaminer>, mut sink: Box<dyn Sink>, timing: Timing, shared: Arc<Shared>) {
    let _ending = Ending(Arc::clone(&shared));
    let mut schedule = Schedule::new(timing);
    // Flushes waiting for what is pending to be examined.
    let mut acks: Vec<mpsc::Sender<bool>> = Vec::new();
    let absorb = |message: ToExaminer, schedule: &mut Schedule, acks: &mut Vec<mpsc::Sender<bool>>| match message {
        ToExaminer::Batch(batch) => schedule.add(batch),
        ToExaminer::Full(reason) => schedule.add(Batch::scan(reason)),
        ToExaminer::Flush(ack) => acks.push(ack),
        ToExaminer::Wake => {}
    };
    loop {
        if shared.stopping() {
            return;
        }
        // Everything that queued, as one batch.
        loop {
            match rx.try_recv() {
                Ok(message) => absorb(message, &mut schedule, &mut acks),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        let now = Instant::now();
        if shared.degraded() {
            schedule.degrade(now);
        }
        let take = if acks.is_empty() { Take::WhenDue } else { Take::Now };
        if let Some(batch) = schedule.take(now, take) {
            let handled = sink.handle(batch);
            let examined = match schedule.done(handled, Instant::now()) {
                Outcome::Examined => {
                    shared.update(|s| {
                        s.examined += 1;
                        s.failing = None;
                    });
                    true
                }
                Outcome::NotYet => {
                    shared.update(|s| s.failing = None);
                    tracing::debug!("the folder has no completed listing yet; its local changes wait");
                    false
                }
                Outcome::Failed { why, wait, said } => {
                    tracing::warn!("local changes could not be examined: {why}; trying again in {} s", wait.as_secs());
                    if said {
                        shared.update(|s| s.failing = Some(why));
                    }
                    false
                }
                Outcome::RootGone => {
                    tracing::warn!("the OneDrive folder was moved or deleted; nothing more is examined");
                    shared.root_gone();
                    for ack in acks.drain(..) {
                        let _ = ack.send(false);
                    }
                    return;
                }
            };
            for ack in acks.drain(..) {
                let _ = ack.send(examined);
            }
            continue;
        }
        // Nothing is pending: a flush has all it asked for.
        for ack in acks.drain(..) {
            let _ = ack.send(true);
        }
        let timeout = schedule.wake_at().map_or(Duration::from_secs(3600), |at| at.saturating_duration_since(Instant::now()));
        match rx.recv_timeout(timeout) {
            Ok(message) => absorb(message, &mut schedule, &mut acks),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}
