//! The one thread that changes the folder for programs: each change a program asks for is
//! made and recorded here, one after another, under the folder's tree lock — the lock a cycle
//! of the delta listing holds while it makes the folder follow OneDrive — and answered only
//! then.
//!
//! So a change is in the outbox before the program hears it was made, the mount's own
//! threads never wait for the store or for a cycle, and no two changes of names interleave.
//!
//! [`Recorder`] owns the way to its thread (the thread itself owns the jobs' queue and the
//! tree lock), the store's writer, what wakes the outbox's worker, and the flag that says
//! whether the folder takes changes, which it is handed and shares with the mount. It knows
//! nothing of the mount, of the disk or of what a job does: a job is a closure, and what a
//! change is recorded as is decided in [`super::detections`].

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use konedrive_tree::{Store, TreeError};

use super::detections::FolderEvent;
use crate::local::record::{record, Recorded};

/// One change, made on the recorder's thread.
type Job = Box<dyn FnOnce() + Send>;

/// What wakes the outbox's worker.
pub(super) type Wake = Arc<dyn Fn() + Send + Sync>;

pub(super) struct Recorder {
    /// The way to the thread; `None` once the recorder is stopped.
    jobs: Mutex<Option<mpsc::Sender<Job>>>,
    /// The store's one writer.
    store: Store,
    /// Wakes the outbox's worker: set while a sync that uploads runs.
    wake: Mutex<Option<Wake>>,
    /// Whether programs may change the folder: the mount's flag, shared.
    writable: Arc<AtomicBool>,
}

impl Recorder {
    /// Starts the thread, which runs every job it is given in the order it came, each under
    /// `tree_lock`, and records into `store` while `writable` says the folder takes changes.
    pub(super) fn start(store: Store, tree_lock: Arc<tokio::sync::Mutex<()>>, writable: Arc<AtomicBool>) -> io::Result<Self> {
        let (jobs, queued) = mpsc::channel::<Job>();
        std::thread::Builder::new().name("konedrive-record".into()).spawn(move || {
            for job in queued {
                let _tree = tree_lock.blocking_lock();
                job();
            }
        })?;
        Ok(Recorder { jobs: Mutex::new(Some(jobs)), store, wake: Mutex::new(None), writable })
    }

    /// Has `job` run on the recorder's thread. A recorder that is stopped drops it, and with
    /// it the reply it holds: the program is told the request failed.
    pub(super) fn run(&self, job: impl FnOnce() + Send + 'static) {
        if let Some(jobs) = &*crate::panic::lock(&self.jobs) {
            let _ = jobs.send(Box::new(job));
        }
    }

    /// The thread ends with the last job it was given; what comes later is dropped.
    pub(super) fn stop(&self) {
        *crate::panic::lock(&self.jobs) = None;
    }

    /// Whether the folder takes changes now.
    pub(super) fn is_writable(&self) -> bool {
        self.writable.load(Ordering::SeqCst)
    }

    /// From now on each recorded change calls `wake`; `None`: nothing is woken.
    pub(super) fn set_wake(&self, wake: Option<Wake>) {
        *crate::panic::lock(&self.wake) = wake;
    }

    /// Wakes the outbox's worker, when a sync that uploads runs: a row was written.
    pub(super) fn wake(&self) {
        let wake = crate::panic::lock(&self.wake).clone();
        if let Some(wake) = wake {
            wake();
        }
    }

    /// Records `event` in the outbox: what it is recorded as is decided and written in one
    /// transaction, and the worker is woken. A folder that does not take changes records
    /// nothing: what a program still held open when it was switched to read-only is not
    /// sent. Called on the recorder's thread, or wherever no async runtime runs.
    pub(super) fn record(&self, event: impl FolderEvent) -> Result<Recorded, TreeError> {
        if !self.is_writable() {
            return Ok(Recorded::default());
        }
        let now = crate::clock::unix_now();
        let recorded = self.store.call_blocking(move |s| {
            let batch = event.detect(&*s, now)?;
            if batch.is_empty() {
                return Ok(Recorded::default());
            }
            record(s, batch.detections, batch.ops, now)
        })?;
        self.wake();
        Ok(recorded)
    }
}

#[cfg(test)]
mod tests;
