use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use super::{blocking, blocking_under, Sections};

/// A section started under a lock keeps it until the section is over, though the task that
/// started it is gone (the worker stopping): nobody takes the lock while the section still
/// works on the folder.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_section_keeps_its_lock_after_its_task_is_dropped() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let (begun, has_begun) = mpsc::channel();
    let (end, may_end) = mpsc::channel::<()>();
    let task = {
        let lock = Arc::clone(&lock);
        tokio::spawn(async move {
            let tree = Arc::new(lock.lock_owned().await);
            blocking_under(Arc::clone(&tree), move || {
                begun.send(()).unwrap();
                may_end.recv().unwrap();
                Ok(())
            })
            .await
        })
    };
    tokio::task::spawn_blocking(move || has_begun.recv().unwrap()).await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(lock.try_lock().is_err(), "the section is still running: the lock is still held");
    end.send(()).unwrap();
    let free = tokio::time::timeout(Duration::from_secs(10), lock.lock()).await;
    assert!(free.is_ok(), "the lock is free once the section has ended");
}

/// The worker's stop waits for a section of a row whose task it dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_waits_for_the_section_of_a_dropped_row() {
    let sections = Sections::default();
    let (begun, has_begun) = mpsc::channel();
    let (end, may_end) = mpsc::channel::<()>();
    let row = {
        let sections = sections.clone();
        tokio::spawn(async move {
            sections
                .of(blocking(move || {
                    begun.send(()).unwrap();
                    may_end.recv().unwrap();
                    Ok(())
                }))
                .await
        })
    };
    tokio::task::spawn_blocking(move || has_begun.recv().unwrap()).await.unwrap();
    row.abort();
    assert!(row.await.unwrap_err().is_cancelled());
    let stop = tokio::spawn(async move { sections.ended().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!stop.is_finished(), "the section is still running: the stop waits");
    end.send(()).unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(10), stop).await.is_ok(), "the stop ends once the section has");
}
