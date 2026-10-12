//! The files being read as ranges: a file that is not downloaded is read by fetching the
//! bytes asked for from OneDrive, and while it is read it goes before the background
//! downloads.
//!
//! [`RangeReaders`] holds one reader per file that is read this way: the slot of the
//! account's pool it holds, as a file being opened does, how many of its reads wait for
//! OneDrive now, and until when it still counts as read after the last of them
//! ([`READ_LINGERS`]). While a slot is held no pinned or other background download starts, so
//! what the user reads goes first. A timer of its own lets each reader go: never while a read
//! is in flight, and only once it lingered out.
//!
//! It holds the readers and nothing else: the ranges are fetched by
//! [`ItemRangeFetcher`](super::item_range_fetcher::ItemRangeFetcher), which tells it of each read. It
//! knows nothing of the source of the content, nor of what is downloaded whole
//! ([`ItemDownloadStates`](super::item_download_states::ItemDownloadStates) counts what was read).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use konedrive_graph::pool::{Class, Size, Slot, TransferPool};
use tokio::time::Instant;

/// How long after its last read a file still counts as being read: the background downloads
/// stay held back between the reads of one program.
pub(super) const READ_LINGERS: Duration = Duration::from_secs(2);

/// A file being read as ranges: the pool's slot held for it, how many of its reads wait for
/// OneDrive now, and until when it counts as read after the last of them.
struct RangeReader {
    _slot: Slot,
    in_flight: usize,
    lingers_until: Instant,
}

/// The files being read as ranges, by OneDrive id.
pub(super) struct RangeReaders {
    readers: Mutex<HashMap<String, RangeReader>>,
    pool: Arc<TransferPool>,
    /// Where the timers run.
    runtime: tokio::runtime::Handle,
}

/// One read of a file as a range, in flight from [`RangeReaders::begin_read`] until this is
/// dropped.
pub(super) struct ActiveRangeRead {
    range_readers: Arc<RangeReaders>,
    onedrive_id: String,
}

impl Drop for ActiveRangeRead {
    fn drop(&mut self) {
        if let Some(reader) = crate::panic::lock(&self.range_readers.readers).get_mut(&self.onedrive_id) {
            reader.in_flight = reader.in_flight.saturating_sub(1);
            reader.lingers_until = Instant::now() + READ_LINGERS;
        }
    }
}

impl RangeReaders {
    pub(super) fn new(pool: Arc<TransferPool>, runtime: tokio::runtime::Handle) -> Self {
        RangeReaders { readers: Mutex::default(), pool, runtime }
    }

    /// One more read of the file `id` is in flight, if the file has a reader already.
    fn join_reader(&self, id: &str) -> bool {
        crate::panic::lock(&self.readers).get_mut(id).map(|reader| reader.in_flight += 1).is_some()
    }

    /// The OneDrive file `id` is being read: the pool is held for it as for a file being
    /// opened, for as long as what this returns lives and [`READ_LINGERS`] after its last
    /// read.
    pub(super) async fn begin_read(self: &Arc<Self>, id: &str) -> ActiveRangeRead {
        if !self.join_reader(id) {
            let slot = self.pool.acquire_sized(Class::Open, Size::Small).await;
            // Another read of the file may have become its reader meanwhile: one slot a file.
            if !self.join_reader(id) {
                let lingers_until = Instant::now() + READ_LINGERS;
                crate::panic::lock(&self.readers).insert(id.to_owned(), RangeReader { _slot: slot, in_flight: 1, lingers_until });
                self.runtime.spawn(Arc::clone(self).expire_reader(id.to_owned()));
            }
        }
        ActiveRangeRead { range_readers: Arc::clone(self), onedrive_id: id.to_owned() }
    }

    /// The timer of the reader of `id`: lets the reader, and its slot, go once no read of it
    /// is in flight and it lingered out.
    async fn expire_reader(self: Arc<Self>, id: String) {
        loop {
            let Some((in_flight, lingers_until)) = crate::panic::lock(&self.readers).get(&id).map(|reader| (reader.in_flight, reader.lingers_until)) else { return };
            let now = Instant::now();
            if in_flight == 0 && now >= lingers_until {
                crate::panic::lock(&self.readers).remove(&id);
                return;
            }
            // While a read is in flight it is looked at again after a while; the read's end
            // moves `lingers_until`.
            tokio::time::sleep_until(if in_flight == 0 { lingers_until } else { now + READ_LINGERS }).await;
        }
    }
}

#[cfg(test)]
pub(super) mod tests;
