use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use konedrive_tree::outbox::{OutboxKind, OutboxState};
use konedrive_tree::{Store, TreeStore};

use super::OutboxRecorder;
use crate::fuse::detections::tests::{detection_of, file, in_root, local_only};
use crate::fuse::detections::{EntryCreated, OutboxBatch, WriteState};

fn outbox_recorder() -> (Store, Arc<OutboxRecorder>) {
    let store = Store::new(TreeStore::in_memory().unwrap());
    let outbox_recorder = Arc::new(OutboxRecorder::new(store.clone()));
    (store, outbox_recorder)
}

fn file_created() -> EntryCreated {
    EntryCreated::Fresh { entry: local_only(&file(1, 3), &in_root("new.txt")), write_state: WriteState::NotOpen }
}

fn count_wakes(outbox_recorder: &OutboxRecorder) -> Arc<AtomicUsize> {
    let woken = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&woken);
    outbox_recorder.set_outbox_waker(Some(Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    })));
    woken
}

#[test]
fn a_recorded_change_is_in_the_outbox_and_wakes_the_worker() {
    let (store, outbox_recorder) = outbox_recorder();
    let woken = count_wakes(&outbox_recorder);
    let recorded = outbox_recorder.record(file_created()).unwrap();
    assert_eq!(recorded.applied.queued.len(), 1);
    let rows = store.call_blocking(|s| s.outbox_rows()).unwrap();
    assert_eq!(rows.iter().map(|row| (row.kind, row.rel.to_string_lossy().into_owned())).collect::<Vec<_>>(), [(OutboxKind::Create, "new.txt".to_owned())]);
    assert_eq!(woken.load(Ordering::SeqCst), 1);
}

/// A cycle records what it kept of the user's own inside its own transaction, and wakes the
/// worker itself once it is committed.
#[test]
fn a_batch_recorded_inside_a_call_of_the_store_is_in_the_outbox_and_wakes_nothing() {
    let (store, outbox_recorder) = outbox_recorder();
    let woken = count_wakes(&outbox_recorder);
    let kept = detection_of(OutboxKind::Create, &file(1, 3), &in_root("kept.txt"), OutboxState::Ready);
    let recording = Arc::clone(&outbox_recorder);
    let refused = store.call_blocking(move |s| recording.record_step_in(s, OutboxBatch { detections: vec![kept], ops: Vec::new() }, &[], &[], 1_000)).unwrap();
    assert_eq!(refused, 0);
    let rows = store.call_blocking(|s| s.outbox_rows()).unwrap();
    assert_eq!(rows.iter().map(|row| row.rel.to_string_lossy().into_owned()).collect::<Vec<_>>(), ["kept.txt"]);
    assert_eq!(woken.load(Ordering::SeqCst), 0);
}

#[test]
fn a_batch_that_holds_nothing_writes_nothing() {
    let (store, outbox_recorder) = outbox_recorder();
    store.call_blocking(move |s| outbox_recorder.record_step_in(s, OutboxBatch::default(), &[], &[], 1_000)).unwrap();
    assert!(store.call_blocking(|s| s.outbox_rows()).unwrap().is_empty());
}

/// Without a sync that uploads nothing is woken, and the change is recorded all the same.
#[test]
fn a_recorder_without_a_waker_records_and_wakes_nothing() {
    let (store, outbox_recorder) = outbox_recorder();
    outbox_recorder.record(file_created()).unwrap();
    assert_eq!(store.call_blocking(|s| s.outbox_rows()).unwrap().len(), 1);
}
