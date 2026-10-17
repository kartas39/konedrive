//! What the filesystem says of a file or directory of the backing directory: its kind, size,
//! blocks and time.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Whether an entry is a file or a directory. The mount holds nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File,
    Directory,
}

/// What the filesystem says of a file or directory of the backing directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiskMetadata {
    pub(crate) kind: EntryKind,
    pub(in crate::mount) size: u64,
    /// In units of 512 bytes, as `stat` counts them.
    pub(in crate::mount) blocks: u64,
    pub(in crate::mount) modified: SystemTime,
}

impl DiskMetadata {
    /// What `metadata` says, for a regular file or a directory. `None` for anything else: a
    /// symbolic link, a socket, a device.
    pub(in crate::mount) fn from_filesystem(metadata: &fs::Metadata) -> Option<Self> {
        let kind = match (metadata.is_file(), metadata.is_dir()) {
            (true, _) => EntryKind::File,
            (_, true) => EntryKind::Directory,
            _ => return None,
        };
        let modified = UNIX_EPOCH + Duration::new(metadata.mtime().max(0) as u64, metadata.mtime_nsec() as u32);
        Some(DiskMetadata { kind, size: metadata.size(), blocks: metadata.blocks(), modified })
    }

}

