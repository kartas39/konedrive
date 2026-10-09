//! The one thread the write operations run on: each change a program asks for is made and
//! recorded here, one after another, under the folder's tree lock — the lock a cycle of the
//! delta listing holds while it makes the folder follow OneDrive — and answered only then.
//!
//! So a change is in the outbox before the program hears it was made, the mount's own
//! threads never wait for the store or for a cycle, and no two changes of names interleave.
//! What runs here besides the write operations is what must not run beside one: the switch
//! of the folder to read-only, and the scan for unrecorded entries.
//!
//! [`WriteOperationThread`] owns the jobs that wait for its thread, and the one fact of the
//! thread others need: whether a job runs on it now. The thread itself owns the tree lock.
//! It knows nothing of the mount, of the disk, of the store or of what a job does: a job is
//! a closure.
//!
//! A thread that is stopped runs nothing any more ([`WriteOperationThread::stop`]): the job
//! that ran when it was stopped has ended, and every job that waited was dropped. The stop
//! waits for that one job and for nothing else — not for the tree lock, which the thread
//! takes before it takes a job off the queue, so a job that waits for a cycle is still in
//! the queue, and is dropped there.

use std::collections::VecDeque;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use tokio::sync::oneshot;

/// One job, run on the thread.
type Job = Box<dyn FnOnce() + Send>;

pub(super) struct WriteOperationThread {
    /// The jobs that wait for the thread, in the order they came; `None` once it is
    /// stopped.
    queued_jobs: Arc<Mutex<Option<VecDeque<Job>>>>,
    /// Wakes the thread: a job was queued, or the thread is stopped.
    job_queued: Arc<Condvar>,
    /// Held by the thread from before it takes a job off the queue until the job has ended:
    /// whoever stops the thread waits for the job in flight here.
    job_in_flight: Arc<Mutex<()>>,
}

/// The thread: runs every queued job in the order it came, each under `tree_lock`, until
/// the queue is gone. A job that panics ends the thread, and the jobs that wait with it.
fn run_queued_jobs(tree_lock: &tokio::sync::Mutex<()>, queued_jobs: &Mutex<Option<VecDeque<Job>>>, job_queued: &Condvar, job_in_flight: &Mutex<()>) {
    loop {
        {
            let mut queued = crate::panic::lock(queued_jobs);
            loop {
                match &*queued {
                    None => return,
                    Some(jobs) if !jobs.is_empty() => break,
                    Some(_) => queued = job_queued.wait(queued).unwrap_or_else(PoisonError::into_inner),
                }
            }
        }
        let _tree = tree_lock.blocking_lock();
        let _in_flight = crate::panic::lock(job_in_flight);
        // Taken off the queue only now: a job that waited for the tree lock when the thread
        // was stopped is not run.
        let job = crate::panic::lock(queued_jobs).as_mut().and_then(VecDeque::pop_front);
        let Some(job) = job else { continue };
        if catch_unwind(AssertUnwindSafe(job)).is_err() {
            let dropped = crate::panic::lock(queued_jobs).take();
            drop(dropped);
            return;
        }
    }
}

impl WriteOperationThread {
    /// Starts the thread, which runs every job it is given in the order it came, each under
    /// `tree_lock`.
    pub(super) fn start(tree_lock: Arc<tokio::sync::Mutex<()>>) -> io::Result<Self> {
        let write_operation_thread = WriteOperationThread { queued_jobs: Arc::new(Mutex::new(Some(VecDeque::new()))), job_queued: Arc::default(), job_in_flight: Arc::default() };
        let (queued_jobs, job_queued, job_in_flight) = (Arc::clone(&write_operation_thread.queued_jobs), Arc::clone(&write_operation_thread.job_queued), Arc::clone(&write_operation_thread.job_in_flight));
        std::thread::Builder::new().name("konedrive-record".into()).spawn(move || run_queued_jobs(&tree_lock, &queued_jobs, &job_queued, &job_in_flight))?;
        Ok(write_operation_thread)
    }

    /// Has `job` run on the thread. A thread that is stopped drops it, and with it the reply
    /// it holds: the program is told the request failed.
    pub(super) fn run(&self, job: impl FnOnce() + Send + 'static) {
        let job: Job = Box::new(job);
        let refused = match &mut *crate::panic::lock(&self.queued_jobs) {
            Some(jobs) => {
                jobs.push_back(job);
                None
            }
            None => Some(job),
        };
        self.job_queued.notify_one();
        // Dropped once the queue is let go of: what a job holds may answer a program.
        drop(refused);
    }

    /// Has `job` run on the thread, after every job given before it; what it gives comes
    /// through what this returns, once it ran. A thread that is stopped drops the job, and
    /// the wait ends with an error.
    pub(super) fn run_and_wait<T: Send + 'static>(&self, job: impl FnOnce() -> T + Send + 'static) -> oneshot::Receiver<T> {
        let (done, waited) = oneshot::channel();
        self.run(move || drop(done.send(job())));
        waited
    }

    /// Stops the thread: when this returns, the job that ran has ended, every job that
    /// waited is dropped, and no job runs on the thread again. Waits for the job in flight
    /// alone, which a write operation's disk and store take — never for the tree lock. Not
    /// to be called from a job.
    pub(super) fn stop(&self) {
        let in_flight = crate::panic::lock(&self.job_in_flight);
        // With no job in flight, and none able to start: the queue goes.
        let dropped = self.take_queued_jobs();
        drop(in_flight);
        drop(dropped);
    }

    /// How many jobs wait for the thread.
    #[cfg(test)]
    pub(super) fn count_queued_jobs(&self) -> usize {
        crate::panic::lock(&self.queued_jobs).as_ref().map_or(0, VecDeque::len)
    }

    /// Takes the queue away, and wakes the thread to its end: no job is queued, or taken
    /// off the queue, from here on.
    fn take_queued_jobs(&self) -> Option<VecDeque<Job>> {
        let taken = crate::panic::lock(&self.queued_jobs).take();
        self.job_queued.notify_all();
        taken
    }
}

/// A thread nobody has the way to any more ends: with the job it runs, which is not waited
/// for here.
impl Drop for WriteOperationThread {
    fn drop(&mut self) {
        drop(self.take_queued_jobs());
    }
}

#[cfg(test)]
mod tests;
