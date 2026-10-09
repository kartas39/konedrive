//! The one thread the write operations run on: each change a program asks for is made and
//! recorded here, one after another, under the folder's tree lock — the lock a cycle of the
//! delta listing holds while it makes the folder follow OneDrive — and answered only then.
//!
//! So a change is in the outbox before the program hears it was made, the mount's own
//! threads never wait for the store or for a cycle, and no two changes of names interleave.
//! What runs here besides the write operations is what must not run beside one: the switch
//! of the folder's access mode, and the scan for unrecorded entries.
//!
//! [`WriteOperationThread`] owns the way to its thread; the thread itself owns the jobs'
//! queue and the tree lock. It knows nothing of the mount, of the disk, of the store or of
//! what a job does: a job is a closure.

use std::io;
use std::sync::{mpsc, Arc, Mutex};

use tokio::sync::oneshot;

/// One job, run on the thread.
type Job = Box<dyn FnOnce() + Send>;

pub(super) struct WriteOperationThread {
    /// The way to the thread; `None` once it is stopped.
    jobs: Mutex<Option<mpsc::Sender<Job>>>,
}

impl WriteOperationThread {
    /// Starts the thread, which runs every job it is given in the order it came, each under
    /// `tree_lock`.
    pub(super) fn start(tree_lock: Arc<tokio::sync::Mutex<()>>) -> io::Result<Self> {
        let (jobs, queued) = mpsc::channel::<Job>();
        std::thread::Builder::new().name("konedrive-record".into()).spawn(move || {
            for job in queued {
                let _tree = tree_lock.blocking_lock();
                job();
            }
        })?;
        Ok(WriteOperationThread { jobs: Mutex::new(Some(jobs)) })
    }

    /// Has `job` run on the thread. A thread that is stopped drops it, and with it the reply
    /// it holds: the program is told the request failed.
    pub(super) fn run(&self, job: impl FnOnce() + Send + 'static) {
        if let Some(jobs) = &*crate::panic::lock(&self.jobs) {
            let _ = jobs.send(Box::new(job));
        }
    }

    /// Has `job` run on the thread, after every job given before it; what it gives comes
    /// through what this returns, once it ran. A thread that is stopped drops the job, and
    /// the wait ends with an error.
    pub(super) fn run_and_wait<T: Send + 'static>(&self, job: impl FnOnce() -> T + Send + 'static) -> oneshot::Receiver<T> {
        let (done, waited) = oneshot::channel();
        self.run(move || drop(done.send(job())));
        waited
    }

    /// The thread ends with the last job it was given; what comes later is dropped.
    pub(super) fn stop(&self) {
        *crate::panic::lock(&self.jobs) = None;
    }
}

#[cfg(test)]
mod tests;
