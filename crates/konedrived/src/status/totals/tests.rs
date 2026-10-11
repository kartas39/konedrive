use std::time::Duration;

use super::*;
use crate::status::snapshot::{UploadQueueStatus, PauseStatus, TransferStatus};
use konedrive_graph::pool::{Direction, TransferPool, STILL_AFTER};
use konedrive_tree::PinnedDownloadTotal;

const MIB: u64 = 1024 * 1024;

fn uploading(pending: u32, bytes: u64, sent: u64) -> SyncSnapshot {
    let upload_queue = UploadQueueStatus { pending_count: pending, pending_bytes: bytes, uploads: vec![("/r/big.bin".into(), sent, 8 * MIB)], ..UploadQueueStatus::default() };
    SyncSnapshot { upload_queue, ..SyncSnapshot::default() }
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
    s.upload_queue.uploads.clear();
    s.upload_queue.space_waiting_count = 3;
    s.upload_queue.space_waiting_bytes = 30 * MIB;
    s.upload_queue.too_big_count = 1;
    s.upload_queue.too_big_bytes = 15 * MIB;
    let up = counter.count(&s, &BTreeMap::new()).up;
    assert_eq!((up.left_count, up.left_bytes), (1, 5 * MIB));

    s.upload_queue.pending_count = 4;
    s.upload_queue.pending_bytes = 45 * MIB;
    assert_eq!(counter.count(&s, &BTreeMap::new()).up, Totals::default(), "all of it kept back");
}

/// Done goes back to 0 when nothing is left — what is kept back does not hold it — and
/// the next run counts from there.
#[test]
fn done_starts_again_when_nothing_is_left() {
    let mut counter = Counter::default();
    let mut s = uploading(2, 10 * MIB, 0);
    s.upload_queue.uploads.clear();
    s.transfers.throughput.up_moved = 7 * MIB;
    assert_eq!(counter.count(&s, &BTreeMap::new()).up.done_bytes, 7 * MIB, "since the daemon started");

    let kept_back = SyncSnapshot {
        upload_queue: UploadQueueStatus { blocked_count: 3, ..UploadQueueStatus::default() },
        transfers: TransferStatus { throughput: s.transfers.throughput, ..TransferStatus::default() },
        ..SyncSnapshot::default()
    };
    assert_eq!(counter.count(&kept_back, &BTreeMap::new()).up, Totals::default());

    s.transfers.throughput.up_moved = 9 * MIB;
    assert_eq!(counter.count(&s, &BTreeMap::new()).up.done_bytes, 2 * MIB, "this run only");
}

/// The totals reach the published state, and follow it.
#[tokio::test(start_paused = true)]
async fn the_totals_are_published_and_kept_up_to_date() {
    let state = SyncStateHandle::new(SyncSnapshot::default());
    let transfers = Transfers::default();
    let task = tokio::spawn(run(state.clone(), transfers.clone()));
    state.update(|s| {
        s.upload_queue.pending_count = 2;
        s.upload_queue.pending_bytes = MIB;
    });
    tokio::time::sleep(konedrive_graph::pool::PUBLISH_EVERY * 2).await;
    assert_eq!((state.get().transfers.queue.up.left_count, state.get().transfers.queue.up.left_bytes), (2, MIB));

    // A directory was pinned: all of it is left before any download runs.
    state.update(|s| s.transfers.pinned_download = PinnedDownloadTotal { count: 2500, bytes: 5000 * MIB });
    tokio::time::sleep(konedrive_graph::pool::PUBLISH_EVERY * 2).await;
    assert_eq!((state.get().transfers.queue.down.left_count, state.get().transfers.queue.down.left_bytes), (2500, 5000 * MIB));
    let entry = transfers.start("/r/f.bin".into(), 3 * MIB);
    entry.progress(MIB, 3 * MIB);
    tokio::time::sleep(konedrive_graph::pool::PUBLISH_EVERY * 2).await;
    assert_eq!((state.get().transfers.queue.down.left_count, state.get().transfers.queue.down.left_bytes), (2500, 4999 * MIB), "a download under way is still left, less what it received");
    drop(entry);
    state.update(|s| s.transfers.pinned_download = PinnedDownloadTotal::default());
    tokio::time::sleep(konedrive_graph::pool::PUBLISH_EVERY * 2).await;
    assert_eq!(state.get().transfers.queue.down, Totals::default());
    task.abort();
}

fn transfer(done: u64, total: u64, open: bool) -> Transfer {
    Transfer { path: "/r/f.bin".into(), done, total, open }
}

/// What is left to download is the pinned files with no content here, whether or not
/// their downloads run, and the files being opened that are downloaded now.
#[test]
fn downloads_left_are_the_pinned_files_not_here_and_the_files_being_opened() {
    let pinned = PinnedDownloadTotal { count: 2500, bytes: 5000 * MIB };
    let none = PinnedDownloadTotal::default();
    let of = |transfers: &[Transfer]| transfers.iter().cloned().zip(0u64..).map(|(t, id)| (id, t)).collect::<BTreeMap<u64, Transfer>>();

    assert_eq!(count_downloads_left(none, &of(&[])), (0, 0), "nothing pinned is missing and nothing is opened");
    assert_eq!(count_downloads_left(pinned, &of(&[])), (2500, 5000 * MIB), "pinned a moment ago: no download runs yet");
    // Eleven of the pool's slots hold downloads of pinned files: the count is not theirs.
    let running: Vec<Transfer> = (0..11).map(|_| transfer(MIB, 2 * MIB, false)).collect();
    assert_eq!(count_downloads_left(pinned, &of(&running)), (2500, 4989 * MIB), "less what they received");
    assert_eq!(count_downloads_left(none, &of(&[transfer(MIB, 4 * MIB, true)])), (1, 3 * MIB), "a file being opened counts while it downloads");
    assert_eq!(count_downloads_left(pinned, &of(&[transfer(MIB, 2 * MIB, false), transfer(MIB, 4 * MIB, true)])), (2501, 5002 * MIB), "both");
    // A download is shown for as long as it holds its slot. Asked for and not answered
    // yet: all of it is left. All bytes here, being checked and put in place: the file is
    // still one of the pinned ones the store counts, and none of its bytes is left.
    assert_eq!(count_downloads_left(pinned, &of(&[transfer(0, 2 * MIB, false)])), (2500, 5000 * MIB), "a pinned file's download that starts");
    assert_eq!(count_downloads_left(pinned, &of(&[transfer(2 * MIB, 2 * MIB, false)])), (2500, 4998 * MIB), "a pinned file's download that finishes");
    assert_eq!(count_downloads_left(none, &of(&[transfer(0, 4 * MIB, true)])), (1, 4 * MIB), "a file being opened whose download starts");
    assert_eq!(count_downloads_left(none, &of(&[transfer(4 * MIB, 4 * MIB, true)])), (1, 0), "a file being opened whose download finishes");
    // A size not known yet, and a count the store has not caught up with: nothing below 0.
    assert_eq!(count_downloads_left(none, &of(&[transfer(MIB, 2 * MIB, false), transfer(0, 0, true)])), (1, 0));
}
