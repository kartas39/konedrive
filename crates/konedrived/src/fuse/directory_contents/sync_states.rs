//! The sync state of a file and of a directory of the mount, and a directory entry: a name
//! with its state.
//!
//! A file and a directory have different sets of states, so there are two enums: a directory
//! is never modified here, and has nothing to download. Each state carries what exists in
//! it: the store's row only where OneDrive has the file or directory, the metadata of the
//! copy only where one lies on disk, the local-only entry only where there is nothing else.
//!
//! A file a program holds open for writing is [`FileSyncState::ModifiedHere`] from its first
//! writer on: the write operations record it in the outbox at that moment
//! ([`crate::fuse::write_operations`]), so the unsent changes already say so.

use konedrive_tree::Row;

use crate::fuse::local_only_entry::LocalOnlyEntry;
use crate::fuse::backing_directory::{DiskMetadata, EntryKind};

/// How a file of the mount stands between OneDrive and this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fuse) enum FileSyncState {
    /// In OneDrive, nothing of it on disk.
    RemoteOnly(Row),
    /// In OneDrive, and its downloaded copy matches.
    Synced(Row, DiskMetadata),
    /// In OneDrive, and its downloaded copy was changed here and is not sent yet.
    ModifiedHere(Row, DiskMetadata),
    /// Made here, not in OneDrive yet.
    LocalOnly(LocalOnlyEntry),
}

impl FileSyncState {
    /// The state of the OneDrive file `row`, for the downloaded copy the backing directory
    /// holds of it and for whether the outbox holds a change of its content
    /// (`changed_here`). A change of content without a copy says nothing: there is no
    /// content of the user's own to show.
    pub(in crate::fuse) fn of_onedrive_file(row: Row, downloaded_copy: Option<DiskMetadata>, changed_here: bool) -> Self {
        match (downloaded_copy, changed_here) {
            (None, _) => FileSyncState::RemoteOnly(row),
            (Some(copy), false) => FileSyncState::Synced(row, copy),
            (Some(copy), true) => FileSyncState::ModifiedHere(row, copy),
        }
    }

    /// The store's row; `None` for a file that is not in OneDrive yet.
    pub(in crate::fuse) fn row(&self) -> Option<&Row> {
        match self {
            FileSyncState::RemoteOnly(row) | FileSyncState::Synced(row, _) | FileSyncState::ModifiedHere(row, _) => Some(row),
            FileSyncState::LocalOnly(_) => None,
        }
    }

    /// The metadata of the downloaded copy, when the backing directory holds one.
    pub(in crate::fuse) fn downloaded_copy(&self) -> Option<&DiskMetadata> {
        match self {
            FileSyncState::Synced(_, copy) | FileSyncState::ModifiedHere(_, copy) => Some(copy),
            FileSyncState::RemoteOnly(_) | FileSyncState::LocalOnly(_) => None,
        }
    }
}

/// How a directory of the mount stands between OneDrive and this computer. Whether the
/// backing directory has a directory for one that is in OneDrive is the backing directory's
/// own business.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fuse) enum DirectorySyncState {
    /// In OneDrive.
    Synced(Row),
    /// Made here, not in OneDrive yet.
    LocalOnly(LocalOnlyEntry),
}

/// The state of an entry; the variant says whether it is a file or a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fuse) enum EntryState {
    File(FileSyncState),
    Directory(DirectorySyncState),
}

/// One name in a directory of the mount, and the state of what has it.
///
/// The row a state carries has the name the mount shows, which is not the store's while a
/// rename is unsent; the place of a local-only entry has the same name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fuse) struct DirectoryEntry {
    pub(in crate::fuse) name: String,
    pub(in crate::fuse) state: EntryState,
}

impl DirectoryEntry {
    /// The entry of a file or directory that is not in OneDrive yet: a file or a directory
    /// by what it is on disk.
    pub(in crate::fuse) fn from_local_only(entry: LocalOnlyEntry) -> Self {
        let name = entry.place.name.clone();
        let state = match entry.on_disk.metadata.kind {
            EntryKind::File => EntryState::File(FileSyncState::LocalOnly(entry)),
            EntryKind::Directory => EntryState::Directory(DirectorySyncState::LocalOnly(entry)),
        };
        DirectoryEntry { name, state }
    }

    /// The store's row; `None` for an entry that is not in OneDrive yet.
    pub(in crate::fuse) fn row(&self) -> Option<&Row> {
        match &self.state {
            EntryState::File(file) => file.row(),
            EntryState::Directory(DirectorySyncState::Synced(row)) => Some(row),
            EntryState::Directory(DirectorySyncState::LocalOnly(_)) => None,
        }
    }

    /// The OneDrive id; `None` for an entry that is not in OneDrive yet.
    pub(in crate::fuse) fn onedrive_id(&self) -> Option<&str> {
        self.row().map(|row| row.id.as_str())
    }

    /// The local-only entry; `None` for an entry that is in OneDrive.
    pub(in crate::fuse) fn local_only(&self) -> Option<&LocalOnlyEntry> {
        match &self.state {
            EntryState::File(FileSyncState::LocalOnly(entry)) | EntryState::Directory(DirectorySyncState::LocalOnly(entry)) => Some(entry),
            EntryState::File(_) | EntryState::Directory(DirectorySyncState::Synced(_)) => None,
        }
    }
}

#[cfg(test)]
mod tests;
