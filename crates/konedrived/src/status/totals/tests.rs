use std::path::PathBuf;
use std::time::Duration;

use super::*;
use crate::status::snapshot::{OutboxStatus, PauseStatus, TransferStatus};
use konedrive_graph::pool::{Direction, TransferPool, STILL_AFTER};
use crate::hydration::pin::Pins;

const MIB: u64 = 1024 * 1024;

fn uploading(pending: u32, bytes: u64, sent: u64) -> SyncSnapshot {
    let outbox = OutboxStatus { pending_count: pending, pending_bytes: bytes, uploads: vec![("/r/big.bin".into(), sent, 8 * MIB)], ..OutboxStatus::default() };
    SyncSnapshot { outbox, ..SyncSnapshot::default() }
}

/// Uploads: the changes not uploaded yet, less what the uploads under way have sent; done
/// is this run's bytes; the time left comes from the pool's average — on a paused clock,
/// one MiB a second for four seconds — and is gone once nothing has moved for 10 s.
#[tokio::test(start_paused = true)]
async fn uploads_left_done_and_time_left_follow_the_pool() {
    let pool = TransferPool::starting_at(4, 64);
    let mut counter = Counter::default();
    let mut s = uploading(6, 20 * MIB, 0);
    s.transfers.throughput = pool.throughput();
    let up = counter.count(&s, &BTreeMap::new()).up;
    assert_eq!(up, Totals { left_count: 6, left_bytes: 20 * MIB, done_bytes: 0, time_left: 0 }, "nothing moved yet");

    for _ in 0..4 {
        pool.moved(Direction::Up, MIB);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let mut s = uploading(6, 20 * MIB, 4 * MIB);
    s.transfers.throughput = pool.throughput();
    let up = counter.count(&s, &BTreeMap::new()).up;
    assert_eq!(up, Totals { left_count: 6, left_bytes: 16 * MIB, done_bytes: 4 * MIB, time_left: 16 });

    // Paused, or during OneDrive's `Retry-After`: no time left, the rest as it was.
    let paused = SyncSnapshot { pause: PauseStatus { paused_until: Some(0), ..PauseStatus::default() }, ..s.clone() };
    assert_eq!(counter.count(&paused, &BTreeMap::new()).up, Totals { time_left: 0, ..up });
    let mut throttled = s.clone();
    throttled.transfers.throughput.retry_after = 30;
    assert_eq!(counter.count(&throttled, &BTreeMap::new()).up, Totals { time_left: 0, ..up });

    // Nothing moved for ten seconds: no time left either.
    tokio::time::sleep(STILL_AFTER).await;
    s.transfers.throughput = pool.throughput();
    assert_eq!(counter.count(&s, &BTreeMap::new()).up, Totals { time_left: 0, ..up });
}

/// What waits for space, or is too big for it, is kept back: not left.
#[test]
fn changes_waiting_for_space_are_not_left() {
    let mut counter = Counter::default();
    let mut s = uploading(5, 50 * MIB, 0);
    s.outbox.uploads.clear();
    s.outbox.space_waiting_count = 3;
    s.outbox.space_waiting_bytes = 30 * MIB;
    s.outbox.too_big_count = 1;
    s.outbox.too_big_bytes = 15 * MIB;
    let up = counter.count(&s, &BTreeMap::new()).up;
    assert_eq!((up.left_count, up.left_bytes), (1, 5 * MIB));

    s.outbox.pending_count = 4;
    s.outbox.pending_bytes = 45 * MIB;
    assert_eq!(counter.count(&s, &BTreeMap::new()).up, Totals::default(), "all of it kept back");
}

/// Done goes back to 0 when nothing is left — what is kept back does not hold it — and
/// the next run counts from there.
#[test]
fn done_starts_again_when_nothing_is_left() {
    let mut counter = Counter::default();
    let mut s = uploading(2, 10 * MIB, 0);
    s.outbox.uploads.clear();
    s.transfers.throughput.up_moved = 7 * MIB;
    assert_eq!(counter.count(&s, &BTreeMap::new()).up.done_bytes, 7 * MIB, "since the daemon started");

    let kept_back = SyncSnapshot {
        outbox: OutboxStatus { blocked_count: 3, held_count: 1, ..OutboxStatus::default() },
        transfers: TransferStatus { throughput: s.transfers.throughput, ..TransferStatus::default() },
        ..SyncSnapshot::default()
    };
    assert_eq!(counter.count(&kept_back, &BTreeMap::new()).up, Totals::default());

    s.transfers.throughput.up_moved = 9 * MIB;
    assert_eq!(counter.count(&s, &BTreeMap::new()).up.done_bytes, 2 * MIB, "this run only");
}

/// Downloads: the pinned files waiting and every download under way, less what those
/// have received; the pins' queue says what waits, and a download on open counts only
/// while it runs.
#[tokio::test]
async fn downloads_left_are_the_pinned_files_waiting_and_those_under_way() {
    let state = SyncStateHandle::new(SyncSnapshot::default());
    let pins = Pins::detached(state.clone());
    pins.add((0..3).map(|i| (PathBuf::from(format!("/r/{i}.bin")), 10 * MIB)).collect());
    assert_eq!(state.get().local.pinned_waiting, (3, 30 * MIB));

    let transfers = Transfers::default();
    let opening = transfers.start("/r/open.bin".into(), 0);
    opening.progress(MIB, 4 * MIB);
    let mut s = state.get();
    s.transfers.throughput.down_moved = MIB;
    s.transfers.throughput.down_average = MIB;
    let mut counter = Counter::default();
    let down = counter.count(&s, &transfers.subscribe().borrow());
    assert_eq!(down.down, Totals { left_count: 4, left_bytes: 33 * MIB, done_bytes: MIB, time_left: 33 });
    assert_eq!(down.up, Totals::default());

    pins.clear();
    drop(opening);
    let s = SyncSnapshot { transfers: TransferStatus { throughput: s.transfers.throughput, ..state.get().transfers }, ..state.get() };
    assert_eq!(counter.count(&s, &transfers.subscribe().borrow()).down, Totals::default());
}

/// The totals reach the published state, and follow it.
#[tokio::test(start_paused = true)]
async fn the_totals_are_published_and_kept_up_to_date() {
    let state = SyncStateHandle::new(SyncSnapshot::default());
    let transfers = Transfers::default();
    let task = tokio::spawn(run(state.clone(), transfers.clone()));
    state.update(|s| {
        s.outbox.pending_count = 2;
        s.outbox.pending_bytes = MIB;
    });
    tokio::time::sleep(konedrive_graph::pool::PUBLISH_EVERY * 2).await;
    assert_eq!((state.get().transfers.queue.up.left_count, state.get().transfers.queue.up.left_bytes), (2, MIB));

    let entry = transfers.start("/r/f.bin".into(), 3 * MIB);
    tokio::time::sleep(konedrive_graph::pool::PUBLISH_EVERY * 2).await;
    assert_eq!((state.get().transfers.queue.down.left_count, state.get().transfers.queue.down.left_bytes), (1, 3 * MIB));
    drop(entry);
    tokio::time::sleep(konedrive_graph::pool::PUBLISH_EVERY * 2).await;
    assert_eq!(state.get().transfers.queue.down, Totals::default());
    task.abort();
}
