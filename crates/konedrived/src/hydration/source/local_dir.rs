//! The content source of a folder filled with `PopulateFromDirectory`: a directory on this
//! machine, one file for each item id.

use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt};

use super::{ContentSource, Fetched, SourceError};

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
/// the folder is refused before a populate starts; a source *file*
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

/// One file per item id in a directory. It cannot say which version a file is
/// ([`Fetched::version`] is `None`), so nothing it serves is verified or checkpointed.
pub struct LocalDir {
    dir: PathBuf,
    /// The sync folder this source fills, whose own files it refuses to be
    /// read from; `None` for a source that fills nothing real.
    refusing: Option<PathBuf>,
}

impl LocalDir {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), refusing: None }
    }

    /// Refuse to serve any file that leads into `root`, the sync folder
    /// this source fills, or that is one of konedrive's own files —
    /// decided on the descriptor the bytes would be read from, so
    /// a symlink swapped after the folder was populated is caught too.
    pub fn refusing_files_of(mut self, root: impl Into<PathBuf>) -> Self {
        self.refusing = Some(root.into());
        self
    }
}

#[async_trait]
impl ContentSource for LocalDir {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
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
