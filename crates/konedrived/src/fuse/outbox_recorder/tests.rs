use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use konedrive_tree::outbox::OutboxKind;
use konedrive_tree::{Store, TreeStore};

use super::OutboxRecorder;
use crate::fuse::detections::tests::{file, in_root};
use crate::fuse::detections::EntryCreated;

fn outbox_recorder(writable: bool) -> (Store, OutboxRecorder) {
    let store = Store::new(TreeStore::in_memory().unwrap());
    let outbox_recorder = OutboxRecorder::start(store.clone(), Arc::default(), Arc::new(AtomicBool::new(writable))).unwrap();
    (store, outbox_recorder)
}

fn made() -> EntryCreated {
    EntryCreated::Fresh { place: in_root("new.txt"), object: file(1, 3), open: false }
}

#[test]
fn a_recorded_change_is_in_the_outbox_and_wakes_the_worker() {
    let (store, outbox_recorder) = outbox_recorder(true);
    let woken = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&woken);
    outbox_recorder.set_wake(Some(Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    })));
    let recorded = outbox_recorder.record(made()).unwrap();
    assert_eq!(recorded.applied.queued.len(), 1);
    let rows = store.call_blocking(|s| s.outbox_rows()).unwrap();
    assert_eq!(rows.iter().map(|row| (row.kind, row.rel.to_string_lossy().into_owned())).collect::<Vec<_>>(), [(OutboxKind::Create, "new.txt".to_owned())]);
    assert_eq!(woken.load(Ordering::SeqCst), 1);
}

/// What a program still held open when the folder was switched to read-only is not sent.
#[test]
fn a_folder_that_takes_no_changes_records_nothing() {
    let (store, outbox_recorder) = outbox_recorder(false);
    outbox_recorder.record(made()).unwrap();
    assert!(store.call_blocking(|s| s.outbox_rows()).unwrap().is_empty());
}

#[test]
fn the_jobs_run_on_the_thread_in_the_order_they_came_until_it_is_stopped() {
    let (_store, outbox_recorder) = outbox_recorder(true);
    let (tell, told) = mpsc::channel();
    for n in 0..3 {
        let tell = tell.clone();
        outbox_recorder.run(move || tell.send(n).unwrap());
    }
    assert_eq!((0..3).map(|_| told.recv_timeout(Duration::from_secs(5)).unwrap()).collect::<Vec<_>>(), [0, 1, 2]);
    outbox_recorder.stop();
    outbox_recorder.run(move || tell.send(9).unwrap());
    assert!(told.recv_timeout(Duration::from_millis(100)).is_err());
}

/// A job waits for a cycle of the delta listing that holds the folder's tree lock.
#[test]
fn a_job_runs_under_the_tree_lock() {
    let store = Store::new(TreeStore::in_memory().unwrap());
    let tree_lock = Arc::new(tokio::sync::Mutex::new(()));
    let outbox_recorder = OutboxRecorder::start(store, Arc::clone(&tree_lock), Arc::new(AtomicBool::new(true))).unwrap();
    let held = tree_lock.try_lock().unwrap();
    let (tell, told) = mpsc::channel();
    outbox_recorder.run(move || tell.send(()).unwrap());
    assert!(told.recv_timeout(Duration::from_millis(100)).is_err());
    drop(held);
    assert!(told.recv_timeout(Duration::from_secs(5)).is_ok());
}
