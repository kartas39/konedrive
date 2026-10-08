//! A file that is not downloaded, read: the bytes asked for come from OneDrive as a range, and
//! the file being read goes before the background downloads.

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use konedrive_graph::pool::{Class, Size};
use konedrive_tree::Row;

use super::shared::Shared;

/// What a program holds open: the node, its item when it has one, the file in the folder
/// under the mount once there is one, and whether the handle may write it.
pub(super) struct Handle {
    pub(super) ino: u64,
    pub(super) id: Option<String>,
    pub(super) rel: PathBuf,
    pub(super) copy: Option<Arc<File>>,
    pub(super) writes: bool,
}

/// A file being read as ranges: the pool's slot held for it, how many of its reads wait for
/// OneDrive now, and until when it counts as read after the last of them.
pub(super) struct Reader {
    pub(super) _slot: konedrive_graph::pool::Slot,
    pub(super) in_flight: usize,
    pub(super) until: tokio::time::Instant,
}

/// One read of a file, counted from [`Shared::reading`] until it is dropped.
pub(super) struct Reading(Arc<Shared>, String);

impl Drop for Reading {
    fn drop(&mut self) {
        if let Some(reader) = crate::panic::lock(&self.0.readers).get_mut(&self.1) {
            reader.in_flight = reader.in_flight.saturating_sub(1);
            reader.until = tokio::time::Instant::now() + READ_LINGERS;
        }
    }
}

/// How long after its last read a file still counts as being read: the background downloads
/// stay held back between the reads of one program.
pub(super) const READ_LINGERS: Duration = Duration::from_secs(2);

impl Shared {
    /// Item `id` is being read: the pool is held for it as for a file being opened, for as
    /// long as what this returns lives and a moment after its last read.
    pub(super) async fn reading(self: &Arc<Self>, id: &str) -> Reading {
        let join = |s: &Shared| crate::panic::lock(&s.readers).get_mut(id).map(|reader| reader.in_flight += 1).is_some();
        if !join(self) {
            let slot = self.pool.acquire_sized(Class::Open, Size::Small).await;
            // Another read of the item may have taken the place meanwhile: one slot a file.
            if !join(self) {
                let until = tokio::time::Instant::now() + READ_LINGERS;
                crate::panic::lock(&self.readers).insert(id.to_owned(), Reader { _slot: slot, in_flight: 1, until });
                let (s, id) = (Arc::clone(self), id.to_owned());
                self.runtime.spawn(async move {
                    loop {
                        let Some((in_flight, until)) = crate::panic::lock(&s.readers).get(&id).map(|r| (r.in_flight, r.until)) else { return };
                        let now = tokio::time::Instant::now();
                        if in_flight == 0 && now >= until {
                            crate::panic::lock(&s.readers).remove(&id);
                            return;
                        }
                        // While a read waits it is looked at again after a while; its end moves
                        // `until`.
                        tokio::time::sleep_until(if in_flight == 0 { until } else { now + READ_LINGERS }).await;
                    }
                });
            }
        }
        Reading(Arc::clone(self), id.to_owned())
    }

    /// The bytes of `row` from `from` up to `end`, asked of OneDrive as a range.
    pub(super) async fn range(self: &Arc<Self>, row: &Row, from: u64, end: u64) -> Result<Vec<u8>, i32> {
        use tokio::io::AsyncReadExt;
        let _reading = self.reading(&row.id).await;
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
}
