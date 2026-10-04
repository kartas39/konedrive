//! Where hydration gets its bytes from, and the loop that fills a
//! placeholder in place from one of those sources.

use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt};

pub mod fill;
pub mod parts;
mod target;

pub use fill::{answer_request, hydrate, hydrate_in_parts, hydrate_with, Answered, FillError, CHECKPOINT_EVERY};
pub(crate) use fill::{back_to_placeholder, download_into, Downloaded};
pub use parts::{Share, Split};
use fill::{checkpoint_every, errno_of, rehash, same_version};
#[cfg(test)]
use fill::{clear_checkpoint_every, set_checkpoint_every};

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
    /// verified or checkpointed, exactly as in part 1.
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
    /// in parts (issue #28) asks for its own range only. A source may still
    /// serve more than that; the download reads no further than `end`.
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError>;

    /// How far a download in parts has come as a whole: `done` bytes of the
    /// file's `size` are on disk. No one of its streams can tell, so the
    /// download says it here; a source that shows progress
    /// (`tracked::Tracked`) shows this one.
    fn progress(&self, _done: u64, _size: u64) {}
}

/// Why a source file must not be read into a placeholder, or
/// `None` if it may be: it is one of konedrive's own files — it carries
/// `user.konedrive.*` attributes, however it was reached (a hardlink, a bind
/// mount) — or the file it leads to lies inside the sync folder `root`.
///
/// A placeholder read through a source is read with nothing intercepting
/// it — or through the daemon's own exemption — so its zeros come back as
/// content, and a fill writes them into the file it fills and stamps it
/// `hydrated`: the outcome this whole component exists to prevent, on the
/// offline route a user actually runs. A source *directory* that overlaps
/// the folder is refused before a populate starts (m10); a source *file*
/// can still lead into it — a symlink to a placeholder there, or a hardlink
/// to one — and that is what this looks at.
///
/// `xattrs` are the file's attribute names, `resolved` where it really is.
fn refused_source(
    xattrs: impl Iterator<Item = std::ffi::OsString>,
    resolved: &Path,
    root: &Path,
) -> Option<String> {
    let ours = xattrs
        .into_iter()
        .any(|name| name.as_encoded_bytes().starts_with(b"user.konedrive."));
    if ours {
        return Some("it is one of konedrive's own files (it carries user.konedrive.* attributes)".into());
    }
    if resolved.starts_with(root) {
        return Some(format!("it is {}, inside the sync folder", resolved.display()));
    }
    None
}

/// [`refused_source`] for a source file named by path, following a
/// symlink as a fill would: the check `PopulateFromDirectory` makes before
/// it mirrors the file. Reads attributes and resolves the path; opens
/// nothing.
pub(crate) fn refused_source_path(path: &Path, root: &Path) -> io::Result<Option<String>> {
    let resolved = std::fs::canonicalize(path)?;
    let names = xattr::list_deref(path)?;
    Ok(refused_source(names, &resolved, root))
}

/// Test source: one file per item id in a directory, with fault injection.
pub struct LocalDir {
    dir: PathBuf,
    /// The sync folder this source fills, whose own files it refuses to be
    /// read from; `None` for a source that fills nothing real.
    refusing: Option<PathBuf>,
    fail_at: Option<u64>,
    /// With `fail_at`, whether the break applies to the first fetch only.
    /// A permanent break can only ever exercise the give-up path; healing
    /// after one failure is what lets a test follow a file all the way
    /// across two fetches, which is where the resume arithmetic lives.
    heal_after_first_failure: bool,
    fetches: AtomicU64,
    delay: Option<std::time::Duration>,
}

impl LocalDir {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            refusing: None,
            fail_at: None,
            heal_after_first_failure: false,
            fetches: AtomicU64::new(0),
            delay: None,
        }
    }

    /// Refuse to serve any file that leads into `root`, the sync folder
    /// this source fills, or that is one of konedrive's own files —
    /// decided on the descriptor the bytes would be read from, so
    /// a symlink swapped after the folder was populated is caught too.
    pub fn refusing_files_of(mut self, root: impl Into<PathBuf>) -> Self {
        self.refusing = Some(root.into());
        self
    }

    /// Break the stream after this many bytes, on every fetch.
    pub fn fail_at(mut self, bytes: u64) -> Self {
        self.fail_at = Some(bytes);
        self.heal_after_first_failure = false;
        self
    }

    /// Break the stream after this many bytes on the first fetch only; every
    /// later fetch serves the rest of the file.
    pub fn fail_once_at(mut self, bytes: u64) -> Self {
        self.fail_at = Some(bytes);
        self.heal_after_first_failure = true;
        self
    }

    pub fn delay(mut self, delay: std::time::Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    /// How many times `fetch` has been called. A test that means "and it did
    /// not even try again" has to be able to say so.
    pub fn fetches(&self) -> u64 {
        self.fetches.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ContentSource for LocalDir {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let fetch_number = self.fetches.fetch_add(1, Ordering::SeqCst);
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        let path = self.dir.join(item_id);
        // One open, and everything decided on it: what it is, whether it may
        // be read, and the bytes.
        let opened = std::fs::File::open(&path).map_err(|e| SourceError::NotFound(e.to_string()))?;
        if let Some(root) = &self.refusing {
            let names = xattr::FileExt::list_xattr(&opened)
                .map_err(|e| SourceError::NotFound(format!("{}: {e}", path.display())))?;
            let resolved = std::fs::read_link(format!("/proc/self/fd/{}", opened.as_raw_fd()))
                .map_err(|e| SourceError::NotFound(format!("{}: {e}", path.display())))?;
            if let Some(why) = refused_source(names, &resolved, root) {
                tracing::error!("refusing to fill a file from {}: {why}", path.display());
                return Err(SourceError::NotFound(format!("{}: {why}", path.display())));
            }
        }
        let meta = opened.metadata().map_err(|e| SourceError::NotFound(e.to_string()))?;
        let mut file = tokio::fs::File::from_std(opened);
        if from > 0 {
            use tokio::io::AsyncSeekExt;
            file.seek(io::SeekFrom::Start(from))
                .await
                .map_err(|e| SourceError::Transient(e.to_string()))?;
        }
        let mut stream: Box<dyn AsyncRead + Send + Unpin> = Box::new(file);
        let breaks_here = match self.fail_at {
            Some(limit) if !self.heal_after_first_failure || fetch_number == 0 => Some(limit),
            _ => None,
        };
        if let Some(limit) = breaks_here {
            stream = Box::new(stream.take(limit.saturating_sub(from)));
        }
        if let Some(end) = end {
            stream = Box::new(stream.take(end.saturating_sub(from)));
        }
        Ok(Fetched {
            served_from: from,
            size: meta.len(),
            mtime: meta.modified().map_err(|e| SourceError::Transient(e.to_string()))?,
            version: None,
            stream,
        })
    }
}
