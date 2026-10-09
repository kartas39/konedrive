//! The entry at a path of the backing directory: looked at once, with the file handle that
//! identifies it, its metadata, and the OneDrive id it is marked with.
//!
//! A file handle goes with its file or directory through every rename and is never given to
//! another, as an inode number is once its file is removed.

use std::fs::{self, File};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use konedrive_fs::handle::FileHandle;
use konedrive_fs::placeholder::XATTR_ITEM_ID;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;

use super::BackingDirectory;

/// Whether an entry is a file or a directory. The mount holds nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fuse) enum EntryKind {
    File,
    Directory,
}

/// What the filesystem says of a file or directory of the backing directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fuse) struct DiskMetadata {
    pub(in crate::fuse) kind: EntryKind,
    pub(in crate::fuse) size: u64,
    /// In units of 512 bytes, as `stat` counts them.
    pub(in crate::fuse) blocks: u64,
    pub(in crate::fuse) modified: SystemTime,
}

impl DiskMetadata {
    /// What `metadata` says, for a regular file or a directory. `None` for anything else: a
    /// symbolic link, a socket, a device.
    pub(in crate::fuse) fn from_filesystem(metadata: &fs::Metadata) -> Option<Self> {
        let kind = match (metadata.is_file(), metadata.is_dir()) {
            (true, _) => EntryKind::File,
            (_, true) => EntryKind::Directory,
            _ => return None,
        };
        let modified = UNIX_EPOCH + Duration::new(metadata.mtime().max(0) as u64, metadata.mtime_nsec() as u32);
        Some(DiskMetadata { kind, size: metadata.size(), blocks: metadata.blocks(), modified })
    }

    /// The size of a file; `None` for a directory, whose size says nothing.
    pub(in crate::fuse) fn file_size(&self) -> Option<u64> {
        (self.kind == EntryKind::File).then_some(self.size)
    }
}

/// A file or directory of the backing directory: the file handle that identifies it, and its
/// metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fuse) struct DiskEntry {
    pub(in crate::fuse) handle: FileHandle,
    pub(in crate::fuse) metadata: DiskMetadata,
}

impl BackingDirectory {
    /// Whatever is at `rel`, opened once through the backing directory's descriptor, without
    /// following a symbolic link at its name. `None` when nothing is there.
    fn open_path(&self, rel: &Path) -> Option<File> {
        let found = nix::fcntl::openat(self.descriptor.as_fd(), rel, OFlag::O_PATH | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC, Mode::empty()).ok()?;
        Some(File::from(found))
    }

    /// The file handle of whatever is at `rel`: a file, a directory, or a symbolic link
    /// itself. `None` when nothing is there.
    pub(in crate::fuse) fn find_file_handle(&self, rel: &Path) -> Option<FileHandle> {
        FileHandle::of(&self.open_path(rel)?).ok()
    }

    /// The file or directory at `rel`, looked at once: the handle and the metadata are of
    /// one inode, whatever takes the name meanwhile. `None` when nothing is there, and for
    /// what is neither a regular file nor a directory.
    pub(in crate::fuse) fn find_disk_entry(&self, rel: &Path) -> Option<DiskEntry> {
        let found = self.open_path(rel)?;
        let handle = FileHandle::of(&found).ok()?;
        Some(DiskEntry { handle, metadata: DiskMetadata::from_filesystem(&found.metadata().ok()?)? })
    }

    /// The OneDrive id the entry at `rel` is marked with.
    pub(in crate::fuse) fn read_onedrive_id(&self, rel: &Path) -> Option<String> {
        self.attribute(rel, XATTR_ITEM_ID)
    }

    /// Marks the entry at `rel` with the OneDrive id `id`: it belongs to that file or
    /// directory of OneDrive from here on.
    pub(in crate::fuse) fn mark_with_onedrive_id(&self, rel: &Path, id: &str) -> io::Result<()> {
        xattr::set(self.path(rel), XATTR_ITEM_ID, id.as_bytes())
    }

    /// Takes the OneDrive id off the entry at `rel`: it is the user's own from here on.
    pub(in crate::fuse) fn clear_onedrive_id(&self, rel: &Path) {
        let _ = xattr::remove(self.path(rel), XATTR_ITEM_ID);
    }
}

#[cfg(test)]
mod tests;
