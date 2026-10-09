//! Where the bytes of a read come from: a decision of the open file and of what is known of
//! it when it is read, with no disk, store or download in it.

use crate::fuse::open_handles::OpenFile;

/// How far behind its written-up-to mark the file of a running download is surely written:
/// the download writes each piece it has read before it reads the next, and a piece is no
/// larger than this.
pub(super) const WRITTEN_BEHIND: u64 = 1 << 20;

/// What is known, when it is read, of a file that was opened while it was not downloaded.
/// Nothing of it is asked for a file opened from the backing directory, which is given the
/// default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct ReadFacts {
    /// The backing directory holds the file's downloaded copy since it was opened.
    pub(super) downloaded_since_open: bool,
    /// The file's size in OneDrive.
    pub(super) file_size: u64,
    /// How far the file of a download of it that runs now is written in order; `None` when
    /// none runs, or it has no file yet.
    pub(super) download_written_up_to: Option<u64>,
}

/// Where the bytes of one read come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReadOrigin {
    /// The file in the backing directory: the one the handle was opened from, or the
    /// downloaded copy that is there since.
    BackingFile,
    /// The file a running download writes into, which is surely written that far.
    RunningDownload,
    /// OneDrive, as the range of bytes from `from` up to `end`.
    OneDriveRange { from: u64, end: u64 },
    /// Nowhere: the read starts at or past the end of the file, and is answered no bytes.
    Nothing,
}

/// Where the `size` bytes at `offset` of `open_file` come from. A file opened from the
/// backing directory is read there. One that was opened while it was not downloaded is read
/// from its downloaded copy once there is one; else, cut at the file's size, from what a
/// running download has surely written, or from OneDrive as a range.
pub(super) fn choose_read_origin(open_file: &OpenFile, facts: &ReadFacts, offset: u64, size: u32) -> ReadOrigin {
    match open_file {
        OpenFile::ReadingBackingFile { .. } | OpenFile::Writing { .. } => ReadOrigin::BackingFile,
        OpenFile::ReadingRemoteOnly { .. } if facts.downloaded_since_open => ReadOrigin::BackingFile,
        OpenFile::ReadingRemoteOnly { .. } => {
            let end = (offset + u64::from(size)).min(facts.file_size);
            if offset >= end {
                ReadOrigin::Nothing
            } else if facts.download_written_up_to.is_some_and(|written_up_to| end + WRITTEN_BEHIND <= written_up_to) {
                ReadOrigin::RunningDownload
            } else {
                ReadOrigin::OneDriveRange { from: offset, end }
            }
        }
    }
}

#[cfg(test)]
mod tests;
