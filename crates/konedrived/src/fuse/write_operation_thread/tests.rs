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
