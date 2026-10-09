//! What a remove takes out of the folder, or the errno it is refused with: a decision of
//! what stands at the place, with no disk in it.

use std::path::PathBuf;

use fuser::Errno;
use konedrive_fs::handle::FileHandle;

use crate::fuse::backing_directory::EntryKind;
use crate::fuse::directory_contents::{DirectoryEntry, DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::directory_reference::DirectoryReference;

/// What stands at the place an entry is to be removed from, looked up before the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RemovePlaceContents {
    /// The directory the node the program named stands for; `None` when it stands for none.
    pub(super) parent_directory: Option<DirectoryReference>,
    /// The entry that directory holds under the name; `None` when it holds none.
    pub(super) entry: Option<DirectoryEntry>,
    /// The entry is a directory that holds something.
    pub(super) holds_entries: bool,
}

/// What a remove the check let through takes out of the folder, at `path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RemovePlan {
    /// A file of OneDrive: removed there by the outbox, and its downloaded copy here.
    OneDriveFile { onedrive_id: String, path: PathBuf },
    /// A directory of OneDrive: removed there by the outbox, and its directory here.
    OneDriveDirectory { onedrive_id: String, path: PathBuf },
    /// A local-only file: removed here, and forgotten by the outbox.
    LocalOnlyFile { handle: FileHandle, path: PathBuf },
    /// A local-only directory: removed here, and forgotten by the outbox.
    LocalOnlyDirectory { handle: FileHandle, path: PathBuf },
}

/// What removing `name` as a `kind` (a file for `unlink`, a directory for `rmdir`) takes out
/// of the folder, or the errno it is refused with, in this order: no such directory, or no
/// such entry (`ENOENT`); a directory where a file was asked for (`EISDIR`), a file where a
/// directory was (`ENOTDIR`); a directory that holds something (`ENOTEMPTY`).
pub(super) fn plan_remove(kind: EntryKind, name: &str, place: RemovePlaceContents) -> Result<RemovePlan, Errno> {
    let parent_directory = place.parent_directory.ok_or(Errno::ENOENT)?;
    let entry = place.entry.ok_or(Errno::ENOENT)?;
    let path = parent_directory.path.join(name);
    match (entry.state, kind) {
        (EntryState::Directory(_), EntryKind::File) => Err(Errno::EISDIR),
        (EntryState::File(_), EntryKind::Directory) => Err(Errno::ENOTDIR),
        (EntryState::Directory(_), EntryKind::Directory) if place.holds_entries => Err(Errno::ENOTEMPTY),
        (EntryState::Directory(DirectorySyncState::InOneDrive(row)), EntryKind::Directory) => Ok(RemovePlan::OneDriveDirectory { onedrive_id: row.id, path }),
        (EntryState::Directory(DirectorySyncState::LocalOnly(local_only)), EntryKind::Directory) => Ok(RemovePlan::LocalOnlyDirectory { handle: local_only.on_disk.handle, path }),
        (EntryState::File(FileSyncState::LocalOnly(local_only)), EntryKind::File) => Ok(RemovePlan::LocalOnlyFile { handle: local_only.on_disk.handle, path }),
        (EntryState::File(FileSyncState::RemoteOnly(row) | FileSyncState::Synced(row, _) | FileSyncState::ModifiedHere(row, _)), EntryKind::File) => Ok(RemovePlan::OneDriveFile { onedrive_id: row.id, path }),
    }
}

#[cfg(test)]
mod tests;
