//! Queue totals (issue #16): how much is left to download and to upload, how much of this
//! run is done, and about how long the rest takes — per account, each way. `Transfers`
//! publishes them as `DownloadLeftCount`, `DownloadLeftBytes`, `DownloadDoneBytes`,
//! `DownloadTimeLeft`, and the same four for uploads.
//!
//! - **Left.** Uploads, in changes: the outbox rows not uploaded yet (`PendingCount`,
//!   `PendingBytes`; a move, a delete or a new folder is a change of no bytes), less the bytes
//!   already sent of the uploads under way. Downloads, in files: the pinned files waiting
//!   ([`super::pin::Pins`]) and every download under way (`Transfers`: opens, `Hydrate`,
//!   replacements, pinned files), less the bytes already received of those under way. A file
//!   being opened and a replacement are not queued ahead: they count only while they run.
//!   What is kept back — blocked, held, waiting for space while OneDrive is full, too big for
//!   the space left — is not left: the Not Uploaded page counts it.
//! - **Done.** The bytes moved that way (the transfer pool's count) since nothing was last
//!   left that way, or since the daemon started; 0 while nothing is left.
//! - **Time left.** The bytes left over the pool's average speed (`pool::AVERAGE_SPAN`); none
//!   while nothing has moved that way for `pool::STILL_AFTER` (the average is 0 then), while
//!   OneDrive's `Retry-After` runs, and — for uploads — while syncing is paused.
//!
//! [`run`] counts them into the published state at most once a
//! [`PUBLISH_EVERY`](crate::pool::PUBLISH_EVERY), as the pool publishes its speeds.

use std::collections::BTreeMap;

use super::activity::{Transfer, Transfers};
use super::{SyncSnapshot, SyncStateHandle};

/// One direction's totals, as `Transfers` publishes them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    /// Files (downloads) or changes (uploads) left.
    pub left_count: u32,
    pub left_bytes: u64,
    /// Bytes moved in this run.
    pub done_bytes: u64,
    /// Seconds; 0 when unknown.
    pub time_left: u32,
}

/// Both directions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueTotals {
    pub down: Totals,
    pub up: Totals,
}

/// Counts the totals, and remembers where each direction's run began: the pool's count of
/// bytes moved when nothing was last left that way.
#[derive(Debug, Default)]
pub struct Counter {
    began: [u64; 2],
}

impl Counter {
    /// The totals now, from the published state `s` and the downloads under way.
    pub fn count(&mut self, s: &SyncSnapshot, transfers: &BTreeMap<u64, Transfer>) -> QueueTotals {
        let pool = s.throughput;
        let received: u64 = transfers.values().map(|t| t.total.saturating_sub(t.done)).sum();
        let running = u32::try_from(transfers.len()).unwrap_or(u32::MAX);
        let (pinned, pinned_bytes) = s.pinned_waiting;
        let down = (pinned.saturating_add(running), pinned_bytes.saturating_add(received));
        // `PendingCount` takes in the changes that wait for space and those too big for it:
        // kept back, not left.
        let sent: u64 = s.uploads.iter().map(|(_, sent, _)| sent).sum();
        let kept = s.space_waiting_count.saturating_add(s.too_big_count);
        let kept_bytes = s.space_waiting_bytes.saturating_add(s.too_big_bytes);
        let up = (s.pending_count.saturating_sub(kept), s.pending_bytes.saturating_sub(kept_bytes).saturating_sub(sent));
        let throttled = pool.retry_after > 0;
        QueueTotals {
            down: self.direction(0, down, pool.down_moved, pool.down_average, throttled),
            up: self.direction(1, up, pool.up_moved, pool.up_average, throttled || s.paused_until.is_some()),
        }
    }

    /// One direction: `left` (count, bytes), the bytes the pool has moved that way in all,
    /// its average speed, and whether it is held back (no time left then).
    fn direction(&mut self, at: usize, (count, bytes): (u32, u64), moved: u64, average: u64, held: bool) -> Totals {
        if count == 0 {
            self.began[at] = moved;
            return Totals::default();
        }
        let time_left = if held || average == 0 || bytes == 0 {
            0
        } else {
            u32::try_from(bytes.div_ceil(average)).unwrap_or(u32::MAX)
        };
        Totals { left_count: count, left_bytes: bytes, done_bytes: moved.saturating_sub(self.began[at]), time_left }
    }
}

/// Counts the totals into `state`: at once, then whenever the state or the downloads under
/// way change — at most once a [`PUBLISH_EVERY`](crate::pool::PUBLISH_EVERY). Never returns
/// while `state` is held here; the caller aborts it when the account goes.
pub async fn run(state: SyncStateHandle, transfers: Transfers) {
    let mut changes = state.subscribe();
    let mut moving = transfers.subscribe();
    let mut counter = Counter::default();
    loop {
        // Both borrows end with the statement, before the state is written.
        let totals = counter.count(&changes.borrow_and_update(), &moving.borrow_and_update());
        state.set_queue(totals);
        tokio::time::sleep(crate::pool::PUBLISH_EVERY).await;
        tokio::select! {
            changed = changes.changed() => if changed.is_err() { return },
            changed = moving.changed() => if changed.is_err() { return },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::pool::{Direction, TransferPool, STILL_AFTER};
    use crate::sync::pin::Pins;

    const MIB: u64 = 1024 * 1024;

    fn uploading(pending: u32, bytes: u64, sent: u64) -> SyncSnapshot {
        SyncSnapshot {
            pending_count: pending,
            pending_bytes: bytes,
            uploads: vec![("/r/big.bin".into(), sent, 8 * MIB)],
            ..SyncSnapshot::default()
        }
    }

    /// Uploads: the changes not uploaded yet, less what the uploads under way have sent; done
    /// is this run's bytes; the time left comes from the pool's average — on a paused clock,
    /// one MiB a second for four seconds — and is gone once nothing has moved for 10 s.
    #[tokio::test(start_paused = true)]
    async fn uploads_left_done_and_time_left_follow_the_pool() {
        let pool = TransferPool::starting_at(4, 64);
        let mut counter = Counter::default();
        let mut s = uploading(6, 20 * MIB, 0);
        s.throughput = pool.throughput();
        let up = counter.count(&s, &BTreeMap::new()).up;
        assert_eq!(up, Totals { left_count: 6, left_bytes: 20 * MIB, done_bytes: 0, time_left: 0 }, "nothing moved yet");

        for _ in 0..4 {
            pool.moved(Direction::Up, MIB);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let mut s = uploading(6, 20 * MIB, 4 * MIB);
        s.throughput = pool.throughput();
        let up = counter.count(&s, &BTreeMap::new()).up;
        assert_eq!(up, Totals { left_count: 6, left_bytes: 16 * MIB, done_bytes: 4 * MIB, time_left: 16 });

        // Paused, or during OneDrive's `Retry-After`: no time left, the rest as it was.
        let paused = SyncSnapshot { paused_until: Some(0), ..s.clone() };
        assert_eq!(counter.count(&paused, &BTreeMap::new()).up, Totals { time_left: 0, ..up });
        let mut throttled = s.clone();
        throttled.throughput.retry_after = 30;
        assert_eq!(counter.count(&throttled, &BTreeMap::new()).up, Totals { time_left: 0, ..up });

        // Nothing moved for ten seconds: no time left either.
        tokio::time::sleep(STILL_AFTER).await;
        s.throughput = pool.throughput();
        assert_eq!(counter.count(&s, &BTreeMap::new()).up, Totals { time_left: 0, ..up });
    }

    /// What waits for space, or is too big for it, is kept back: not left.
    #[test]
    fn changes_waiting_for_space_are_not_left() {
        let mut counter = Counter::default();
        let mut s = uploading(5, 50 * MIB, 0);
        s.uploads.clear();
        s.space_waiting_count = 3;
        s.space_waiting_bytes = 30 * MIB;
        s.too_big_count = 1;
        s.too_big_bytes = 15 * MIB;
        let up = counter.count(&s, &BTreeMap::new()).up;
        assert_eq!((up.left_count, up.left_bytes), (1, 5 * MIB));

        s.pending_count = 4;
        s.pending_bytes = 45 * MIB;
        assert_eq!(counter.count(&s, &BTreeMap::new()).up, Totals::default(), "all of it kept back");
    }

    /// Done goes back to 0 when nothing is left — what is kept back does not hold it — and
    /// the next run counts from there.
    #[test]
    fn done_starts_again_when_nothing_is_left() {
        let mut counter = Counter::default();
        let mut s = uploading(2, 10 * MIB, 0);
        s.uploads.clear();
        s.throughput.up_moved = 7 * MIB;
        assert_eq!(counter.count(&s, &BTreeMap::new()).up.done_bytes, 7 * MIB, "since the daemon started");

        let kept_back = SyncSnapshot { blocked_count: 3, held_count: 1, throughput: s.throughput, ..SyncSnapshot::default() };
        assert_eq!(counter.count(&kept_back, &BTreeMap::new()).up, Totals::default());

        s.throughput.up_moved = 9 * MIB;
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
        assert_eq!(state.get().pinned_waiting, (3, 30 * MIB));

        let transfers = Transfers::default();
        let opening = transfers.start("/r/open.bin".into(), 0);
        opening.progress(MIB, 4 * MIB);
        let mut s = state.get();
        s.throughput.down_moved = MIB;
        s.throughput.down_average = MIB;
        let mut counter = Counter::default();
        let down = counter.count(&s, &transfers.subscribe().borrow());
        assert_eq!(down.down, Totals { left_count: 4, left_bytes: 33 * MIB, done_bytes: MIB, time_left: 33 });
        assert_eq!(down.up, Totals::default());

        pins.clear();
        drop(opening);
        let s = SyncSnapshot { throughput: s.throughput, ..state.get() };
        assert_eq!(counter.count(&s, &transfers.subscribe().borrow()).down, Totals::default());
    }

    /// The totals reach the published state, and follow it.
    #[tokio::test(start_paused = true)]
    async fn the_totals_are_published_and_kept_up_to_date() {
        let state = SyncStateHandle::new(SyncSnapshot::default());
        let transfers = Transfers::default();
        let task = tokio::spawn(run(state.clone(), transfers.clone()));
        state.update(|s| {
            s.pending_count = 2;
            s.pending_bytes = MIB;
        });
        tokio::time::sleep(crate::pool::PUBLISH_EVERY * 2).await;
        assert_eq!((state.get().queue.up.left_count, state.get().queue.up.left_bytes), (2, MIB));

        let entry = transfers.start("/r/f.bin".into(), 3 * MIB);
        tokio::time::sleep(crate::pool::PUBLISH_EVERY * 2).await;
        assert_eq!((state.get().queue.down.left_count, state.get().queue.down.left_bytes), (1, 3 * MIB));
        drop(entry);
        tokio::time::sleep(crate::pool::PUBLISH_EVERY * 2).await;
        assert_eq!(state.get().queue.down, Totals::default());
        task.abort();
    }
}
