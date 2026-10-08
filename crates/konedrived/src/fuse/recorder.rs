//! The one thread that changes the folder for programs: each change a program asks for is
//! made and recorded here, one after another, under the folder's tree lock — the lock a cycle
//! of the listing holds while it makes the folder follow OneDrive — and answered only then.
//!
//! So a change is in the outbox before the program hears it was made, the mount's own
//! threads never wait for the store or for a cycle, and no two changes of names interleave.

use std::sync::{mpsc, Arc};

use konedrive_tree::outbox::{Detection, OutboxOp};
use konedrive_tree::{TreeError, TreeStore};

use super::shared::Shared;
use crate::local::record::{record, Recorded};

/// One change, made on the recorder's thread.
pub(super) type Job = Box<dyn FnOnce(&Arc<Shared>) + Send>;

/// What a change means for the outbox: its detections, and what goes with them.
pub(super) type Meaning = (Vec<Detection>, Vec<OutboxOp>);

impl Shared {
    /// Has `job` run on the recorder's thread. A mount that is going drops it, and with it
    /// the reply it holds: the program is told the request failed.
    pub(super) fn change(&self, job: impl FnOnce(&Arc<Shared>) + Send + 'static) {
        if let Some(jobs) = &*crate::panic::lock(&self.jobs) {
            let _ = jobs.send(Box::new(job));
        }
    }

    /// Writes what `meaning` finds a change to mean into the outbox, in one transaction with
    /// what it read to find it, and wakes the worker. Called on the recorder's thread.
    pub(super) fn record(&self, meaning: impl FnOnce(&mut TreeStore) -> Result<Meaning, TreeError> + Send + 'static) -> Result<Recorded, TreeError> {
        // A folder switched to read-only takes no row: what a program still held open at the
        // switch is not sent.
        if !self.writable() {
            return Ok(Recorded::default());
        }
        let now = self.now();
        let recorded = self.store.call_blocking(move |s| {
            let (detections, ops) = meaning(s)?;
            if detections.is_empty() && ops.is_empty() {
                return Ok(Recorded::default());
            }
            record(s, detections, ops, now)
        })?;
        self.wake();
        Ok(recorded)
    }
}

/// The recorder's thread: every job in the order it came, each under the tree lock.
pub(super) fn run(shared: &Arc<Shared>, jobs: mpsc::Receiver<Job>) {
    for job in jobs {
        let _tree = shared.writing.tree_lock.blocking_lock();
        job(shared);
    }
}
