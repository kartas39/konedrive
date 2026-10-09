//! Where the bytes of a read come from: what each origin reads from, and the one decision
//! among them, with no disk, store or download in it.
//!
//! A file opened from the backing directory is read there, and so is one that was
//! downloaded since it was opened: the open file says both, and the read requests make
//! that origin of it. What is decided here ([`choose_read_origin`]) is the read of a file
//! that is not downloaded: from what a running download has surely written, from OneDrive
//! as a range, or from nowhere.

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use konedrive_tree::Row;

use crate::fuse::download_states::RunningDownloadFile;

/// How far behind its written-up-to mark the file of a running download is surely written:
/// the download writes each piece it has read before it reads the next, and a piece is no
/// larger than this.
pub(super) const WRITTEN_BEHIND: u64 = 1 << 20;

/// Where the bytes of one read come from: each case carries what it is read from.
#[derive(Debug, Clone)]
pub(super) enum ReadOrigin {
    /// The file in the backing directory: the one the handle was opened from, or the
    /// downloaded copy that is there since.
    BackingFile(Arc<File>),
    /// The file a running download writes into, which is surely written that far.
    RunningDownload(Arc<File>),
    /// OneDrive, as the range of bytes from `from` up to `end` of the file `row`, which was
    /// opened at `path`.
    OneDriveRange { row: Row, path: PathBuf, from: u64, end: u64 },
    /// Nowhere: the read starts at or past the end of the file, and is answered no bytes.
    Nothing,
}

/// Where the `size` bytes at `offset` of the OneDrive file `row` come from, which was opened
/// at `path` and is not downloaded; `running_download` is the file a download of it that
/// runs now writes into, and how far. The read ends at the file's size: nothing at or
/// past its end; what a running download has surely written from its file; anything else
/// from OneDrive, as a range.
pub(super) fn choose_read_origin(row: Row, path: PathBuf, running_download: Option<RunningDownloadFile>, offset: u64, size: u32) -> ReadOrigin {
    let end = (offset + u64::from(size)).min(row.size);
    if offset >= end {
        return ReadOrigin::Nothing;
    }
    match running_download {
        Some(running_download) if end + WRITTEN_BEHIND <= running_download.written_up_to => ReadOrigin::RunningDownload(running_download.file),
        Some(_) | None => ReadOrigin::OneDriveRange { row, path, from: offset, end },
    }
}

#[cfg(test)]
mod tests;
