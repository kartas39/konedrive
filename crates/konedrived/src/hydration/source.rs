//! Where hydration gets its bytes from (`ContentSource`), and the fill of a
//! placeholder in place from one of those sources (`fill`).

use std::time::SystemTime;

use async_trait::async_trait;
use tokio::io::AsyncRead;

mod download;
pub mod fill;
mod guards;
mod local_dir;
pub mod parts;
mod target;

pub(crate) use download::{download_into, Downloaded};
pub use fill::{answer_request, hydrate_in_parts, hydrate_with, Answered, FillError};
pub(crate) use local_dir::refused_source_path;
pub use local_dir::LocalDir;
pub use parts::{Share, Split};

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Transient(String),
}

pub struct Fetched {
    /// The offset this stream actually starts at.
    ///
    /// `fill` asks for `from` and writes what comes back at `from`; a source
    /// that silently serves something else corrupts the file and there is no
    /// way to notice after the fact. This is not a theoretical
    /// implementor: the Graph source issues HTTP `Range` requests, and a
    /// server that answers `200` instead of `206` — which is always allowed —
    /// restarts the body at 0. Reporting the served offset is the only thing
    /// that makes that case detectable, so every implementation must set it
    /// to the offset of the first byte of `stream`, not to the offset it was
    /// asked for.
    pub served_from: u64,
    pub size: u64,
    pub mtime: SystemTime,
    /// `None` from a source that cannot say (`LocalDir`): nothing is then
    /// verified or checkpointed.
    pub version: Option<Version>,
    pub stream: Box<dyn AsyncRead + Send + Unpin>,
}

/// Which version of a file a source's bytes belong to, when it can say: the
/// cTag, and the quickXorHash the whole file must match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub ctag: String,
    pub quick_xor: Option<[u8; konedrive_graph::quickxor::LEN]>,
}

#[async_trait]
pub trait ContentSource: Send + Sync {
    /// Bytes of `item_id` starting at `from`, plus the item's current size and mtime.
    ///
    /// `end` is where the bytes wanted stop — the offset of the first byte not
    /// wanted — or `None` for the rest of the file: one piece of a download
    /// in parts asks for its own range only. A source may still
    /// serve more than that; the download reads no further than `end`.
    ///
    /// A `from` at the end of the item or past it is answered, not refused: an empty stream
    /// with the item's size and version. A download that continues from a checkpoint learns
    /// the item's size by now only from this answer, and drops a checkpoint the item has
    /// become too short for (`guards::resume`); a source that failed such a fetch would
    /// have it broken off three times with the checkpoint kept. `DriveClient::download`
    /// answers a `416` this way, and a file read past its end is empty.
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError>;

    /// How far a download in parts has come as a whole: `done` bytes of the
    /// file's `size` are on disk. No one of its streams can tell, so the
    /// download says it here; a source that shows progress
    /// (`tracked::Tracked`) shows this one.
    fn progress(&self, _done: u64, _size: u64) {}
}
