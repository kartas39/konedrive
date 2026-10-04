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
//!   OneDrive's `Retry-After` runs, and — for uploads — while syncing is paused or held back.
//!
//! [`run`] counts them into the published state at most once a
//! [`PUBLISH_EVERY`](konedrive_graph::pool::PUBLISH_EVERY), as the pool publishes its speeds.

use std::collections::BTreeMap;

use super::transfers::{Transfer, Transfers};
use crate::status::snapshot::{SyncSnapshot, SyncStateHandle};

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
            up: self.direction(1, up, pool.up_moved, pool.up_average, throttled || s.stopped()),
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
/// way change — at most once a [`PUBLISH_EVERY`](konedrive_graph::pool::PUBLISH_EVERY). Never returns
/// while `state` is held here; the caller aborts it when the account goes.
pub async fn run(state: SyncStateHandle, transfers: Transfers) {
    let mut changes = state.subscribe();
    let mut moving = transfers.subscribe();
    let mut counter = Counter::default();
    loop {
        // Both borrows end with the statement, before the state is written.
        let totals = counter.count(&changes.borrow_and_update(), &moving.borrow_and_update());
        state.set_queue(totals);
        tokio::time::sleep(konedrive_graph::pool::PUBLISH_EVERY).await;
        tokio::select! {
            changed = changes.changed() => if changed.is_err() { return },
            changed = moving.changed() => if changed.is_err() { return },
        }
    }
}

#[cfg(test)]
mod tests;
