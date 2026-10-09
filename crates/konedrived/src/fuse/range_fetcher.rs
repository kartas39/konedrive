//! A range of a file fetched from OneDrive: the bytes a program reads of a file that is not
//! downloaded.
//!
//! [`RangeFetcher`] owns the source of the content, as far as ranges go: it asks OneDrive
//! for the bytes, and drops what the source remembered of a file between its range reads
//! when the file changed. While a range is fetched its file counts as being read
//! ([`RangeReaders`]), which holds the background downloads back.

use std::sync::Arc;

use konedrive_tree::Row;

use super::errno::to_raw_errno;
use super::range_readers::RangeReaders;
use crate::hydration::source::ContentSource;

pub(super) struct RangeFetcher {
    range_readers: Arc<RangeReaders>,
    source: Arc<dyn ContentSource>,
}

impl RangeFetcher {
    pub(super) fn new(range_readers: Arc<RangeReaders>, source: Arc<dyn ContentSource>) -> Self {
        RangeFetcher { range_readers, source }
    }

    /// The bytes of the OneDrive file `row` from `from` up to `end`, asked of OneDrive as a
    /// range. The errno, when they did not come.
    pub(super) async fn fetch_range(&self, row: &Row, from: u64, end: u64) -> Result<Vec<u8>, i32> {
        use tokio::io::AsyncReadExt;
        let _active_read = self.range_readers.begin_read(&row.id).await;
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
        fetched.stream.take(end - from).read_to_end(&mut bytes).await.map_err(|e| to_raw_errno(&e))?;
        tracing::info!("{} bytes at {from} of {} came from OneDrive in {} ms ({} ms to the answer)", bytes.len(), row.name, asked.elapsed().as_millis(), answered.as_millis());
        Ok(bytes)
    }

    /// The OneDrive file `id` changed or is gone: whatever the source remembered of it
    /// between its range reads is of what it was, and is dropped.
    pub(super) fn drop_cached_ranges(&self, id: &str) {
        self.source.forget(id);
    }
}

#[cfg(test)]
mod tests;
