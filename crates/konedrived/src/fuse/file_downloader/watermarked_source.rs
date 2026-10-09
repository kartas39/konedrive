//! The source of a download, telling how far its file is written as its one stream is read:
//! for the reads that come while the file is downloaded.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::hydration::source::{ContentSource, Fetched, SourceError};

/// The download's source, telling how far the file is written as its one stream is read. A
/// download in parts (`end` given) writes out of order and tells nothing.
pub(super) struct WatermarkedSource<'a> {
    pub(super) inner: &'a dyn ContentSource,
    pub(super) written_up_to: Arc<AtomicU64>,
}

/// The one stream of a download, counting what was read from it.
struct WatermarkedStream {
    inner: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    written_up_to: Arc<AtomicU64>,
}

impl tokio::io::AsyncRead for WatermarkedStream {
    fn poll_read(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &polled {
            self.written_up_to.fetch_add((buf.filled().len() - before) as u64, Ordering::SeqCst);
        }
        polled
    }
}

#[async_trait::async_trait]
impl ContentSource for WatermarkedSource<'_> {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let mut fetched = self.inner.fetch(item_id, from, end).await?;
        if end.is_none() {
            // Also when the download starts over: what is on disk counts from here again.
            self.written_up_to.store(fetched.served_from, Ordering::SeqCst);
            fetched.stream = Box::new(WatermarkedStream { inner: fetched.stream, written_up_to: Arc::clone(&self.written_up_to) });
        }
        Ok(fetched)
    }

    fn progress(&self, done: u64, size: u64) {
        self.inner.progress(done, size);
    }
}
