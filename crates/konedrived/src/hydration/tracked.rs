//! A download as `Transfers` shows it: a content source that reports what it fetches.

use std::io;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};

use async_trait::async_trait;
use tokio::io::{AsyncRead, ReadBuf};

use crate::hydration::source::{ContentSource, Fetched, SourceError};
use crate::status::transfers::{TransferEntry, TransferHandle, Transfers};

/// A [`ContentSource`] whose downloads show in [`Transfers`] as `path`.
///
/// The entry is added at the first answer of the source it fetches through, not at
/// the asking: a download is a transfer once its source gave it something to
/// transfer. One whose source answers nothing — it is offline, signed out, held
/// back, or lets no answer through (`fuse::item_downloader::download_source`, for
/// an answer of another content than the asked one) — never has an entry, not even
/// for the time of the asking. The entry moves on as the bytes are read, and goes
/// when this is dropped: the caller keeps it for exactly as long as the download it
/// is for.
///
/// It is the outermost source of a download: whatever decides which answers a
/// download may use is below it.
pub struct Tracked<'a> {
    source: &'a dyn ContentSource,
    transfers: Transfers,
    path: String,
    /// A file being opened, or `Hydrate` (the pool's `Class::Open`).
    open: bool,
    entry: OnceLock<TransferEntry>,
}

impl<'a> Tracked<'a> {
    pub fn new(source: &'a dyn ContentSource, transfers: Transfers, path: impl Into<String>) -> Self {
        Self { source, transfers, path: path.into(), open: false, entry: OnceLock::new() }
    }

    /// As [`new`](Self::new), for a file being opened or `Hydrate`: its entry says so, and
    /// `LargeFiles` leaves it out.
    pub fn opening(source: &'a dyn ContentSource, transfers: Transfers, path: impl Into<String>) -> Self {
        Self { open: true, ..Self::new(source, transfers, path) }
    }

    /// The size of what was downloaded, if anything was asked for at all.
    pub fn fetched(&self) -> Option<u64> {
        self.entry.get().map(TransferEntry::total)
    }
}

#[async_trait]
impl ContentSource for Tracked<'_> {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let Fetched { served_from, size, mtime, version, stream } = self.source.fetch(item_id, from, end).await?;
        let entry = self.entry.get_or_init(|| self.transfers.start_as(self.path.clone(), 0, self.open));
        // An answer that tells no size leaves the entry's total as it is: 0, "not known
        // yet", for a first answer. Nothing is counted against a size nobody said.
        let Some(total) = size else { return Ok(Fetched { served_from, size, mtime, version, stream }) };
        if end.is_some() {
            // A piece of a download in parts: the file is shown once, and the
            // download says how far it has come as a whole (`progress`).
            if entry.total() != total {
                entry.progress(0, total);
            }
            return Ok(Fetched { served_from, size, mtime, version, stream });
        }
        entry.progress(served_from, total);
        let stream = Box::new(Counting { inner: stream, at: served_from, total, handle: entry.handle() });
        Ok(Fetched { served_from, size, mtime, version, stream })
    }

    fn progress(&self, done: u64, size: u64) {
        if let Some(entry) = self.entry.get() {
            entry.progress(done, size);
        }
    }

    fn reject_written(&self) {
        // What is shown as downloaded counts no more either; and the source below is
        // told, which may keep a mark of its own.
        if let Some(entry) = self.entry.get() {
            entry.progress(0, entry.total());
        }
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
