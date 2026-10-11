//! The source one download fetches through: it tells how far the file is written as its
//! one stream is read, for the reads that come while the file is downloaded, and keeps
//! what the source last refused, typed.
//!
//! **State owned:** the refusal of the source's last answer, for one download: none once
//! an answer came. The download itself (`hydration::source::download_staged`) answers an
//! errno, which cannot say a sign-out from a throttle; whoever ran it asks here what the
//! source said.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::DownloadRefusal;
use crate::hydration::source::{ContentSource, Fetched, SourceError};

/// What `error` of the content source is, by what the plan's task does about it. Pure.
pub(super) fn decide_refusal(error: &SourceError) -> DownloadRefusal {
    match error {
        SourceError::SignedOut(_) => DownloadRefusal::SignedOut,
        SourceError::Throttled { retry_after, .. } => DownloadRefusal::Throttled { retry_after: *retry_after },
        SourceError::NotFound(_) => DownloadRefusal::ItemGone,
        SourceError::Transient(detail) => DownloadRefusal::Transient { detail: detail.clone() },
    }
}

/// The source of one download.
pub(super) struct DownloadSource<'a> {
    inner: &'a dyn ContentSource,
    /// How far the file is written in order. A download in parts (`end` given) writes out
    /// of order and tells nothing.
    written_up_to: Arc<AtomicU64>,
    last_refusal: std::sync::Mutex<Option<DownloadRefusal>>,
}

impl<'a> DownloadSource<'a> {
    pub(super) fn new(inner: &'a dyn ContentSource, written_up_to: Arc<AtomicU64>) -> Self {
        DownloadSource { inner, written_up_to, last_refusal: std::sync::Mutex::new(None) }
    }

    /// What the source refused its last fetch with; `None` when its last fetch was
    /// answered, and when nothing was fetched.
    pub(super) fn find_last_refusal(&self) -> Option<DownloadRefusal> {
        crate::panic::lock(&self.last_refusal).clone()
    }
}

/// The one stream of a download, counting what was read from it.
struct CountedStream {
    inner: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    written_up_to: Arc<AtomicU64>,
}

impl tokio::io::AsyncRead for CountedStream {
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
impl ContentSource for DownloadSource<'_> {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let fetched = self.inner.fetch(item_id, from, end).await;
        *crate::panic::lock(&self.last_refusal) = fetched.as_ref().err().map(decide_refusal);
        let mut fetched = fetched?;
        if end.is_none() {
            // Also when the download starts over: what is on disk counts from here again.
            self.written_up_to.store(fetched.served_from, Ordering::SeqCst);
            fetched.stream = Box::new(CountedStream { inner: fetched.stream, written_up_to: Arc::clone(&self.written_up_to) });
        }
        Ok(fetched)
    }

    fn progress(&self, done: u64, size: u64) {
        self.inner.progress(done, size);
    }
}
