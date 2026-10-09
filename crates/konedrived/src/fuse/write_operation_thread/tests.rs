use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use super::WriteOperationThread;

#[test]
fn the_jobs_run_on_the_thread_in_the_order_they_came_until_it_is_stopped() {
    let write_operation_thread = WriteOperationThread::start(Arc::default()).unwrap();
    let (tell, told) = mpsc::channel();
    for n in 0..3 {
        let tell = tell.clone();
        write_operation_thread.run(move || tell.send(n).unwrap());
    }
    assert_eq!((0..3).map(|_| told.recv_timeout(Duration::from_secs(5)).unwrap()).collect::<Vec<_>>(), [0, 1, 2]);
    write_operation_thread.stop();
    write_operation_thread.run(move || tell.send(9).unwrap());
    // The job was dropped, and the way to the test with it.
    assert_eq!(told.recv_timeout(Duration::from_secs(5)), Err(mpsc::RecvTimeoutError::Disconnected));
}

/// A job waits for a cycle of the delta listing that holds the folder's tree lock.
#[test]
fn a_job_runs_under_the_tree_lock() {
    let tree_lock = Arc::new(tokio::sync::Mutex::new(()));
    let write_operation_thread = WriteOperationThread::start(Arc::clone(&tree_lock)).unwrap();
    let held = tree_lock.try_lock().unwrap();
    let (tell, told) = mpsc::channel();
    write_operation_thread.run(move || tell.send(()).unwrap());
    assert_eq!(told.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout));
    drop(held);
    assert!(told.recv_timeout(Duration::from_secs(5)).is_ok());
}

#[test]
fn a_job_that_is_waited_for_gives_its_answer_after_the_jobs_before_it() {
    let write_operation_thread = WriteOperationThread::start(Arc::default()).unwrap();
    let (tell, told) = mpsc::channel();
    write_operation_thread.run(move || tell.send("first").unwrap());
    let answer = write_operation_thread.run_and_wait(|| 7);
    assert_eq!(answer.blocking_recv(), Ok(7));
    assert_eq!(told.try_recv(), Ok("first"));
}

#[test]
fn a_job_given_to_a_stopped_thread_is_dropped_and_its_wait_ends() {
    let write_operation_thread = WriteOperationThread::start(Arc::default()).unwrap();
    write_operation_thread.stop();
    assert!(write_operation_thread.run_and_wait(|| 7).blocking_recv().is_err(), "the job, and the way its answer comes, were dropped");
}

/// "Stopped" means "runs nothing any more": whoever stops the thread is held until the job
/// that runs has ended.
#[test]
fn a_job_in_flight_when_the_thread_is_stopped_has_ended_when_the_stop_returns() {
    let write_operation_thread = Arc::new(WriteOperationThread::start(Arc::default()).unwrap());
    let (begin, began) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let ended = Arc::new(AtomicBool::new(false));
    let job_ended = Arc::clone(&ended);
    write_operation_thread.run(move || {
        begin.send(()).unwrap();
        let _ = released.recv_timeout(Duration::from_secs(5));
        job_ended.store(true, Ordering::SeqCst);
    });
    began.recv_timeout(Duration::from_secs(5)).unwrap();
    let (stopped_thread, stop_ended) = (Arc::clone(&write_operation_thread), Arc::clone(&ended));
    let stop = std::thread::spawn(move || {
        stopped_thread.stop();
        stop_ended.load(Ordering::SeqCst)
    });
    std::thread::sleep(Duration::from_millis(100));
    assert!(!stop.is_finished(), "the stop waits for the job in flight");
    release.send(()).unwrap();
    assert!(stop.join().unwrap(), "the job had ended when the stop returned");
}

/// A cycle of the delta listing holds the tree lock: the job that waits for it is still in
/// the queue, and the stop drops it there without waiting for the cycle.
#[test]
fn a_job_that_waits_for_the_tree_lock_when_the_thread_is_stopped_is_dropped_and_not_waited_for() {
    let tree_lock = Arc::new(tokio::sync::Mutex::new(()));
    let write_operation_thread = WriteOperationThread::start(Arc::clone(&tree_lock)).unwrap();
    let held = tree_lock.try_lock().unwrap();
    let (tell, told) = mpsc::channel();
    write_operation_thread.run(move || tell.send(()).unwrap());
    // Returns while the lock is held: were it to wait for the lock, the test would not end.
    write_operation_thread.stop();
    assert_eq!(told.try_recv(), Err(mpsc::TryRecvError::Disconnected), "the job was dropped by the stop itself");
    drop(held);
    assert_eq!(told.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Disconnected));
}

/// What a job of a stopped thread would have answered is known to be lost as soon as the
/// thread is stopped: whoever waits for it is not kept waiting.
#[test]
fn the_wait_for_a_queued_job_ends_when_the_thread_is_stopped() {
    let tree_lock = Arc::new(tokio::sync::Mutex::new(()));
    let write_operation_thread = WriteOperationThread::start(Arc::clone(&tree_lock)).unwrap();
    let held = tree_lock.try_lock().unwrap();
    let answer = write_operation_thread.run_and_wait(|| 7);
    write_operation_thread.stop();
    assert!(answer.blocking_recv().is_err());
    drop(held);
}
