//! Queue totals: how much is left to download and to upload, how much of this
//! run is done, and about how long the rest takes — per account, each way. `Transfers`
//! publishes them as `DownloadLeftCount`, `DownloadLeftBytes`, `DownloadDoneBytes`,
//! `DownloadTimeLeft`, and the same four for uploads.
//!
//! - **Left.** Uploads, in changes: the unsent changes not uploaded yet (`PendingCount`,
//!   `PendingBytes`; a move, a delete or a new folder is a change of no bytes), less the bytes
//!   already sent of the uploads under way. Downloads, in files: the pinned files whose
//!   content is not on this computer yet, as the store had them at the last plan
//!   (`TransferStatus::pinned_download`: known from the moment a directory is pinned, and
//!   fewer only as files land), and the files being opened that are downloaded now
//!   (`Transfers`: files being read, `Hydrate`); less the bytes already received of the
//!   downloads under way. No file being opened is queued ahead: it counts only while its
//!   download runs. A download is under way from its slot of the pool to its landing
//!   (`Transfers`): one whose bytes are all here and whose file is not in place yet is
//!   still a file left, with no byte left.
//!   What is kept back — blocked, waiting for space while OneDrive is full, too big for
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

use konedrive_tree::PinnedDownloadTotal;

use super::transfers::{Transfer, Transfers};
use crate::status::snapshot::{SyncSnapshot, SyncStateHandle};

/// One direction's totals, as `Transfers` publishes them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    /// Files (downloads) or changes (uploads) left.
    pub left_count: u32,
    pub left_bytes: u64,
    /// Bytes moved in this run.
    pub(crate) done_bytes: u64,
    /// Seconds; 0 when unknown.
    pub(crate) time_left: u32,
}

/// Both directions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueTotals {
    pub down: Totals,
    pub(crate) up: Totals,
}

/// Counts the totals, and remembers where each direction's run began: the pool's count of
/// bytes moved when nothing was last left that way.
#[derive(Debug, Default)]
pub(crate) struct Counter {
    began: [u64; 2],
}

impl Counter {
    /// The totals now, from the published state `s` and the downloads under way.
    pub fn count(&mut self, s: &SyncSnapshot, transfers: &BTreeMap<u64, Transfer>) -> QueueTotals {
        let pool = s.transfers.throughput;
        let down = count_downloads_left(s.transfers.pinned_download, transfers);
        // `PendingCount` takes in the changes that wait for space and those too big for it:
        // kept back, not left.
        let sent: u64 = s.upload_queue.uploads.iter().map(|(_, sent, _)| sent).sum();
        let kept = s.upload_queue.space_waiting_count.saturating_add(s.upload_queue.too_big_count);
        let kept_bytes = s.upload_queue.space_waiting_bytes.saturating_add(s.upload_queue.too_big_bytes);
        let up = (s.upload_queue.pending_count.saturating_sub(kept), s.upload_queue.pending_bytes.saturating_sub(kept_bytes).saturating_sub(sent));
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

/// What is left to download, as (files, bytes): the pinned files with no content here,
/// less what the downloads of pinned files under way have received; and each file being
/// opened that is downloaded now, with what it has still to receive. A download of a
/// pinned file is of one of `pinned`: the plan gives a step to no other. Pure.
pub(crate) fn count_downloads_left(pinned: PinnedDownloadTotal, transfers: &BTreeMap<u64, Transfer>) -> (u32, u64) {
    let (opened, of_pins): (Vec<&Transfer>, Vec<&Transfer>) = transfers.values().partition(|t| t.open);
    // TODO: check whether a pinned file a program opens before its step ran (it is then a file being opened, and among `pinned` too) is worth telling apart, and fix if needed: it is counted twice while it downloads.
    let count = pinned.count.saturating_add(opened.len() as u64);
    let received_of_pins: u64 = of_pins.iter().map(|t| t.done.min(t.total)).sum();
    let left_of_opened: u64 = opened.iter().map(|t| t.total.saturating_sub(t.done)).sum();
    (u32::try_from(count).unwrap_or(u32::MAX), pinned.bytes.saturating_sub(received_of_pins).saturating_add(left_of_opened))
}

/// Counts the totals into `state`: at once, then whenever the state or the downloads under
/// way change — at most once a [`PUBLISH_EVERY`](konedrive_graph::pool::PUBLISH_EVERY). Never returns
/// while `state` is held here: the account's folder, which starts it, aborts it as it goes
/// (`sync::SyncService`).
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
