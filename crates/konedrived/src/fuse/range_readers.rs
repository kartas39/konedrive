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
//! It fetches the ranges too ([`RangeReaders::fetch_range`]), so the source of the content
//! is its own. It knows nothing of what is downloaded whole
//! ([`DownloadStates`](super::download_states::DownloadStates) counts what was read).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use konedrive_graph::pool::{Class, Size, Slot, TransferPool};
use konedrive_tree::Row;
use tokio::time::Instant;

use crate::hydration::source::ContentSource;

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
    source: Arc<dyn ContentSource>,
    /// Where the timers run.
    runtime: tokio::runtime::Handle,
}

/// One read of a file as a range, in flight from [`RangeReaders::begin_read`] until this is
/// dropped.
pub(super) struct ActiveRangeRead {
    range_readers: Arc<RangeReaders>,
    id: String,
}

impl Drop for ActiveRangeRead {
    fn drop(&mut self) {
        if let Some(reader) = crate::panic::lock(&self.range_readers.readers).get_mut(&self.id) {
            reader.in_flight = reader.in_flight.saturating_sub(1);
            reader.lingers_until = Instant::now() + READ_LINGERS;
        }
    }
}

impl RangeReaders {
    pub(super) fn new(pool: Arc<TransferPool>, source: Arc<dyn ContentSource>, runtime: tokio::runtime::Handle) -> Self {
        RangeReaders { readers: Mutex::default(), pool, source, runtime }
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
        ActiveRangeRead { range_readers: Arc::clone(self), id: id.to_owned() }
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

    /// The bytes of the OneDrive file `row` from `from` up to `end`, asked of OneDrive as a
    /// range. The errno, when they did not come.
    pub(super) async fn fetch_range(self: &Arc<Self>, row: &Row, from: u64, end: u64) -> Result<Vec<u8>, i32> {
        use tokio::io::AsyncReadExt;
        let _active_read = self.begin_read(&row.id).await;
        let asked = std::time::Instant::now();
        let fetched = self.source.read_range(&row.id, from, end).await.map_err(|e| {
            tracing::warn!("a range of {} was not fetched: {e}", row.name);
            libc::EIO
        })?;
        if fetched.served_from != from {
            tracing::error!("{}: asked for byte {from} and got a stream starting at {}", row.name, fetched.served_from);
            return Err(libc::EIO);
        }
        let mut bytes = Vec::with_capacity((end - from) as usize);
        let answered = asked.elapsed();
        fetched.stream.take(end - from).read_to_end(&mut bytes).await.map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
        tracing::info!("{} bytes at {from} of {} came from OneDrive in {} ms ({} ms to the answer)", bytes.len(), row.name, asked.elapsed().as_millis(), answered.as_millis());
        Ok(bytes)
    }

    /// The OneDrive file `id` changed or is gone: whatever the source remembered of it
    /// between its range reads is of what it was, and is dropped.
    pub(super) fn forget_file(&self, id: &str) {
        self.source.forget(id);
    }
}

#[cfg(test)]
mod tests;
