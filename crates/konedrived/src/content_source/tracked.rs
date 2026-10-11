//! How far a download is, as `Transfers` shows it: a content source that reports what it fetches.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
use tokio::io::{AsyncRead, ReadBuf};

use crate::content_source::source::{ContentSource, Fetched, SourceError};
use crate::status::transfers::TransferHandle;

/// A [`ContentSource`] that moves the entry of [`Transfers`] its download is shown by.
///
/// The entry is not its own: whoever holds the download's slot of the pool holds the entry
/// for as long (`mount::item_downloader`), so a download is shown from its slot to its
/// landing or its failure, also while its source has answered nothing yet and after its
/// last byte. This only says how far the bytes are: at each answer that tells a size, as
/// its stream is read, and as a download in parts says it. A source that answers nothing
/// moves nothing.
///
/// It is the outermost source of a download: whatever decides which answers a
/// download may use is below it.
///
/// [`Transfers`]: crate::status::transfers::Transfers
pub struct Tracked<'a> {
    source: &'a dyn ContentSource,
    entry: TransferHandle,
}

impl<'a> Tracked<'a> {
    pub fn new(source: &'a dyn ContentSource, entry: TransferHandle) -> Self {
        Self { source, entry }
    }
}

#[async_trait]
impl ContentSource for Tracked<'_> {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let Fetched { served_from, size, mtime, version, stream } = self.source.fetch(item_id, from, end).await?;
        // An answer that tells no size leaves the entry as it is: nothing is counted
        // against a size nobody said.
        let Some(total) = size else { return Ok(Fetched { served_from, size, mtime, version, stream }) };
        if end.is_some() {
            // A piece of a download in parts: the file is shown once, and the
            // download says how far it has come as a whole (`progress`).
            if self.entry.total() != total {
                self.entry.progress(0, total);
            }
            return Ok(Fetched { served_from, size, mtime, version, stream });
        }
        self.entry.progress(served_from, total);
        let stream = Box::new(Counting { inner: stream, at: served_from, total, handle: self.entry.clone() });
        Ok(Fetched { served_from, size, mtime, version, stream })
    }

    fn progress(&self, done: u64, size: u64) {
        self.entry.progress(done, size);
    }

    fn reject_written(&self) {
        // What is shown as downloaded counts no more either; and the source below is
        // told, which may keep a mark of its own.
        self.entry.progress(0, self.entry.total());
        self.source.reject_written();
    }
}

/// A download's stream, moving its [`Transfers`] entry on as it is read.
struct Counting {
    inner: Box<dyn AsyncRead + Send + Unpin>,
    at: u64,
    total: u64,
    handle: TransferHandle,
}

impl AsyncRead for Counting {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &polled {
            let read = (buf.filled().len() - before) as u64;
            if read > 0 {
                self.at += read;
                let (at, total) = (self.at, self.total);
                self.handle.progress(at, total);
            }
        }
        polled
    }
}

#[cfg(test)]
mod tests;
