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

    /// The size of a file; `None` for a directory, whose size says nothing.
    pub(in crate::mount) fn file_size(&self) -> Option<u64> {
        (self.kind == EntryKind::File).then_some(self.size)
    }
}

/// A file or directory of the backing directory: the file handle that identifies it, and its
/// metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::mount) struct DiskEntry {
    pub(in crate::mount) handle: FileHandle,
    pub(in crate::mount) metadata: DiskMetadata,
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
    pub(in crate::mount) fn find_file_handle(&self, rel: &Path) -> Option<FileHandle> {
        FileHandle::of(&self.open_path(rel)?).ok()
    }

    /// The file or directory at `rel`, looked at once: the handle and the metadata are of
    /// one inode, whatever takes the name meanwhile. `None` when nothing is there, and for
    /// what is neither a regular file nor a directory.
    pub(in crate::mount) fn find_disk_entry(&self, rel: &Path) -> Option<DiskEntry> {
        let found = self.open_path(rel)?;
        let handle = FileHandle::of(&found).ok()?;
        Some(DiskEntry { handle, metadata: DiskMetadata::from_filesystem(&found.metadata().ok()?)? })
    }

    /// The file or directory at `rel`, when it is marked with no OneDrive id: nothing of it
    /// came from OneDrive, and nothing of it was sent there. This is the one rule of an
    /// unmarked entry: what a change from OneDrive moves aside, what it finds in its way,
    /// and what a walk collects to be uploaded, is found by this — and not by what the
    /// mount shows as local-only
    /// (the directory contents of the mount that went),
    /// which also holds an entry marked with an id the store does not have yet: a file just
    /// uploaded is such a one, and is neither moved aside nor uploaded again.
    pub(in crate::mount) fn find_unmarked_disk_entry(&self, rel: &Path) -> Option<DiskEntry> {
        self.find_disk_entry(rel).filter(|_| !self.is_marked_with_onedrive_id(rel))
    }

    /// Whether the entry at `rel` is marked with a OneDrive id, whichever: it is OneDrive's,
    /// whatever its name.
    pub(in crate::mount) fn is_marked_with_onedrive_id(&self, rel: &Path) -> bool {
        self.read_onedrive_id(rel).is_some()
    }

    /// The OneDrive id the entry at `rel` is marked with.
    pub(in crate::mount) fn read_onedrive_id(&self, rel: &Path) -> Option<String> {
        self.read_attribute(rel, XATTR_ITEM_ID)
    }

    /// Marks the entry at `rel` with the OneDrive id `id`: it belongs to that file or
    /// directory of OneDrive from here on.
    pub(in crate::mount) fn mark_with_onedrive_id(&self, rel: &Path, id: &str) -> io::Result<()> {
        xattr::set(self.resolve_path(rel), XATTR_ITEM_ID, id.as_bytes())
    }

    /// Takes the OneDrive id off the entry at `rel`: it is the user's own from here on.
    pub(in crate::mount) fn clear_onedrive_id(&self, rel: &Path) {
        let _ = xattr::remove(self.resolve_path(rel), XATTR_ITEM_ID);
    }
}

#[cfg(test)]
mod tests;
