//! What writes a change into the outbox, and wakes the worker that sends it.
//!
//! [`OutboxRecorder`] owns the store's writer and what wakes the outbox's worker, while one
//! runs. It is the one place of the layer that writes detections: what a change is recorded
//! as is decided ([`super::detections`]) and written in one transaction. It runs no thread
//! of its own — a write operation records on the write-operation thread
//! ([`super::write_operation_thread`]), under the folder's tree lock, and a cycle of the
//! delta listing records what it kept of the user's own inside its own transaction
//! with what its step did to the rows of `items` ([`OutboxRecorder::record_step_in`]).
//!
//! Whether the folder takes changes is not its to ask: the write operation that records has
//! asked the access mode switch already, on the thread the switch to read-only runs on. It knows
//! nothing of the mount or of the disk.

use std::sync::{Arc, Mutex};

use konedrive_tree::{LocalItemId, LocalVersionChange, Store, TreeError, TreeStore};

use super::detections::{FolderEvent, OutboxBatch};
use crate::local::record::{record, record_with_step_changes, Recorded};

/// What wakes the outbox's worker.
pub(super) type OutboxWaker = Arc<dyn Fn() + Send + Sync>;

pub(super) struct OutboxRecorder {
    /// The store's one writer.
    store: Store,
    /// Wakes the outbox's worker: set while a sync that uploads runs, which has one. It is the
    /// way to that worker and not the folder's access mode: nothing reads it as the mode.
    outbox_waker: Mutex<Option<OutboxWaker>>,
}

impl OutboxRecorder {
    /// The recorder into `store`, which wakes nothing yet.
    pub(super) fn new(store: Store) -> Self {
        OutboxRecorder { store, outbox_waker: Mutex::new(None) }
    }

    /// From now on each recorded change calls `outbox_waker`; `None`: nothing is woken.
    pub(super) fn set_outbox_waker(&self, outbox_waker: Option<OutboxWaker>) {
        *crate::panic::lock(&self.outbox_waker) = outbox_waker;
    }

    /// Wakes the outbox's worker, when a sync that uploads runs: a row was written.
    pub(super) fn wake_outbox(&self) {
        let outbox_waker = crate::panic::lock(&self.outbox_waker).clone();
        if let Some(outbox_waker) = outbox_waker {
            outbox_waker();
        }
    }

    /// Records `event` in the outbox: what it is recorded as is decided and written in one
    /// transaction, and the worker is woken. Whether the folder takes changes is the
    /// caller's to have asked: a read-only folder's write operations record nothing. Called
    /// on the write-operation thread, or wherever no async runtime runs.
    pub(super) fn record(&self, event: impl FolderEvent) -> Result<Recorded, TreeError> {
        let now = crate::clock::unix_now();
        let recorded = self.store.call_blocking(move |s| write_batch(s, event.detect(&*s, now)?, now))?;
        self.wake_outbox();
        Ok(recorded)
    }

    /// Writes `batch` into the outbox of `store`, dated `now`, inside the call of the store
    /// the caller is in already: the transaction of a cycle of the delta listing, which has
    /// the store for itself. The worker is not woken here: the cycle wakes it once it is
    /// committed ([`wake_outbox`](Self::wake_outbox)). With it, in the same transaction, what
    /// the cycle's step did to the rows of `items`: `given_up`, the items that give their
    /// bindings up, each with what the upload rules say of its name, and `changes` of local
    /// versions; they are written also where the batch holds nothing, as in a read-only
    /// folder. How many of them the rows refused. Nothing at all for a batch that holds
    /// nothing and a step that changed no row.
    pub(super) fn record_step_in(&self, store: &mut TreeStore, batch: OutboxBatch, given_up: &[(LocalItemId, bool)], changes: &[LocalVersionChange], now: i64) -> Result<usize, TreeError> {
        if given_up.is_empty() && changes.is_empty() {
            return write_batch(store, batch, now).map(|_| 0);
        }
        record_with_step_changes(store, batch.detections, batch.ops, given_up, changes, now).map(|(refused, _)| refused)
    }
}

/// The one place of the layer that writes detections into a store.
fn write_batch(store: &mut TreeStore, batch: OutboxBatch, now: i64) -> Result<Recorded, TreeError> {
    if batch.is_empty() {
        return Ok(Recorded::default());
    }
    record(store, batch.detections, batch.ops, now)
}

#[cfg(test)]
mod tests;
