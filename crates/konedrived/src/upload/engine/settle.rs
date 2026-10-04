//! What a row's outcome does to the row and to the worker: one function for each
//! [`Outcome`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use konedrive_tree::outbox::{OutboxState, Reason};
use konedrive_tree::TreeError;

use super::outcome::{without_urls, Outcome};
use super::{backoff_after, now, Engine, AGAIN_LIMIT};
use crate::upload::kind;

/// A row that came back from its run.
struct Landed {
    seq: i64,
    rel: PathBuf,
    /// Its reason when it was taken: the journal and the activity say a reason once, not
    /// once per retry.
    before: Option<Reason>,
}

impl Engine {
    /// [`settle`](Self::settle) off the async runtime.
    pub(super) async fn settle_blocking(self: &Arc<Self>, seq: i64, outcome: Outcome) {
        let engine = Arc::clone(self);
        if let Err(e) = tokio::task::spawn_blocking(move || engine.settle(seq, outcome)).await {
            tracing::warn!("settling outbox row {seq} failed: {e}");
        }
    }

    /// What `outcome` does to row `seq` and to the worker.
    fn settle(&self, seq: i64, outcome: Outcome) {
        let flight = self.shared().flights.landed(seq);
        let (rel, before) = flight.map(|f| (f.rel, f.reason)).unwrap_or_default();
        let row = Landed { seq, rel, before };
        if !matches!(outcome, Outcome::Done) {
            // Its state changes: the mark is written again.
            self.shared().marks.forget(seq);
        }
        let result = match outcome {
            Outcome::Done => {
                self.shared().throttle.passed();
                Ok(())
            }
            Outcome::Again => self.settle_again(&row),
            Outcome::Wait { reason, at } => self.set(&row, OutboxState::Waiting, Some(&reason), Some(at)),
            Outcome::Later { reason, at } => self.set(&row, OutboxState::Retry, Some(&reason), Some(at)),
            Outcome::Backoff { reason, detail } => self.settle_backoff(&row, reason, detail),
            Outcome::Blocked(reason) => self.settle_blocked(&row, reason),
            Outcome::Throttled(asked) => self.settle_throttled(&row, asked),
            Outcome::SignedOut => self.settle_signed_out(&row),
            Outcome::Crashed => {
                self.shared().trouble.crash();
                Ok(())
            }
            // In its place, with no timer: a quota read lets it go. No event
            // per file: the account's `QuotaFull` says it once.
            Outcome::Space(why) => self.set(&row, OutboxState::Ready, Some(&why), None),
        };
        if let Err(e) = result {
            tracing::warn!("cannot settle outbox row {seq}: {e}");
        }
        self.publish();
    }

    fn set(&self, row: &Landed, state: OutboxState, reason: Option<&Reason>, next_try: Option<i64>) -> Result<(), TreeError> {
        let (seq, reason) = (row.seq, reason.cloned());
        self.store().call_blocking(move |s| s.outbox_set_state(seq, state, reason.as_ref(), next_try))
    }

    fn count_attempt(&self, row: &Landed) -> Result<u32, TreeError> {
        let seq = row.seq;
        self.store().call_blocking(move |s| s.outbox_count_attempt(seq))
    }

    /// Rewritten and ready at once (a temporary name, a copy, a fresh guard): never more
    /// than a few times in a row, unless OneDrive keeps changing under it — then it backs
    /// off like a failure.
    fn settle_again(&self, row: &Landed) -> Result<(), TreeError> {
        let attempts = self.count_attempt(row)?;
        if attempts > AGAIN_LIMIT {
            return self.set(row, OutboxState::Retry, Some(&Reason::ChangingAgain), Some(now() + backoff_after(attempts)));
        }
        self.set(row, OutboxState::Ready, None, None)
    }

    /// One more attempt counted, and the row waits 1 s, doubling with each to an hour.
    fn settle_backoff(&self, row: &Landed, reason: Reason, detail: Option<String>) -> Result<(), TreeError> {
        let next_try = now() + backoff_after(self.count_attempt(row)?);
        self.set(row, OutboxState::Retry, Some(&reason), Some(next_try))?;
        // Once per row and key, as the event: a long network drop
        // writes one line, not one per retry.
        if let Some(detail) = detail.filter(|_| row.before.as_ref() != Some(&reason)) {
            tracing::warn!("{} is tried again later ({reason}): {}", row.rel.display(), without_urls(&detail));
        }
        Ok(())
    }

    /// Blocked until the user does something, and said as an event once per reason.
    fn settle_blocked(&self, row: &Landed, reason: Reason) -> Result<(), TreeError> {
        self.set(row, OutboxState::Blocked, Some(&reason), None)?;
        if row.before.as_ref() != Some(&reason) {
            self.activity(self.event(kind::UPLOAD_FAILED, &row.rel, reason.to_string()));
        }
        Ok(())
    }

    /// The whole worker waits ([`Throttle::refused`](super::state::Throttle::refused));
    /// the row is ready in its place, and no attempt is counted: a throttle is no failure
    /// of the row.
    fn settle_throttled(&self, row: &Landed, asked: Option<Duration>) -> Result<(), TreeError> {
        self.shared().throttle.refused(asked, now());
        self.set(row, OutboxState::Ready, None, None)
    }

    /// The worker takes nothing more; the row is ready in its place for the one a sign-in
    /// starts.
    fn settle_signed_out(&self, row: &Landed) -> Result<(), TreeError> {
        self.shared().trouble.sign_out();
        self.set(row, OutboxState::Ready, None, None)
    }
}
