//! The decisions of a rename, with no disk in them: what moves where, and over what, or the
//! errno it is refused with ([`plan_rename`], the one writer of a [`RenamePlan`]); and what
//! the rename is recorded as ([`describe_rename`]). The rename gathers what each is decided
//! from, and performs it.

use std::path::PathBuf;

use fuser::Errno;
use konedrive_fs::handle::FileHandle;

use crate::fuse::backing_directory::EntryKind;
use crate::fuse::detections::{DisplacedEntry, EntryRenamed, MovedEntry, WriteState};
use crate::fuse::directory_contents::{DirectoryEntry, DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::entry_place::EntryPlace;
use crate::fuse::local_only_entry::LocalOnlyEntry;
use crate::fuse::upload_rules::UploadStatus;

/// What a rename does to what is at the new place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fuse) enum RenameMode {
    /// It is replaced.
    Replace,
    /// The rename is refused (`RENAME_NOREPLACE`).
    NoReplace,
}

/// What stands at the two places of a rename, looked up before the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RenamePlaces {
    /// The directories the two nodes the program named stand for; `None` for a node that
    /// stands for none.
    pub(super) from_directory: Option<DirectoryReference>,
    pub(super) to_directory: Option<DirectoryReference>,
    pub(super) name: String,
    pub(super) new_name: String,
    /// The new name is one the daemon uses for itself in the folder.
    pub(super) new_name_is_daemon_owned: bool,
    /// The entry at the old place, which moves.
    pub(super) moved_entry: Option<DirectoryEntry>,
    /// The entry at the new place.
    pub(super) target: Option<DirectoryEntry>,
    /// The entry at the new place is a directory that holds something.
    pub(super) target_holds_entries: bool,
    /// The new place is in a directory that is ignored: nothing there is uploaded.
    pub(super) to_is_in_ignored_directory: bool,
    /// What the upload rules say of a local-only entry at the new place.
    pub(super) to_upload_status: UploadStatus,
}

/// A rename the check let through: what moves where, and over what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RenamePlan {
    /// What moves.
    pub(super) moved_entry: DirectoryEntry,
    /// What is at the new place, and is replaced.
    pub(super) target: Option<DirectoryEntry>,
    /// The path it moves from.
    pub(super) from: PathBuf,
    /// The place it moves to.
    pub(super) to: EntryPlace,
    /// Whether a local-only entry is uploaded from the new place.
    pub(super) to_upload_status: UploadStatus,
}

impl RenamePlan {
    /// The local-only file that is saved, by rename, over a OneDrive file, and that file's
    /// OneDrive id.
    pub(super) fn find_saved_over_file(&self) -> Option<(&LocalOnlyEntry, &str)> {
        match (&self.moved_entry.state, self.target.as_ref().and_then(DirectoryEntry::onedrive_id)) {
            (EntryState::File(FileSyncState::LocalOnly(file)), Some(over)) => Some((file, over)),
            _ => None,
        }
    }
}

impl RenamePlan {
    /// Whether what moves is a local-only directory that is uploaded from its new place:
    /// what it holds there is walked then, to be recorded with it.
    pub(super) fn uploads_moved_directory(&self) -> bool {
        matches!((&self.moved_entry.state, self.to_upload_status), (EntryState::Directory(DirectorySyncState::LocalOnly(_)), UploadStatus::Uploaded))
    }
}

/// What the backing directory held at the old place, which was renamed with the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackingDirectoryMove {
    /// Something was there, and is at the new place now.
    Moved,
    /// Nothing: a OneDrive file that is not downloaded, a directory no download made yet.
    NothingThere,
}

/// What the rename of `plan` is recorded as. `write_state` is whether a program holds what
/// moved open for writing; `handle_at_new_place` the file handle of what the backing
/// directory moved to the new place, `None` when it held nothing; `uploaded_entries` what a
/// walk of a local-only directory that is uploaded from its new place found there, itself
/// first ([`RenamePlan::uploads_moved_directory`]), and nothing for anything else.
///
/// - A local-only file saved over a OneDrive file is that file's new content.
/// - What is in OneDrive moved, with the handle of what the backing directory holds of it.
/// - A local-only file is uploaded from its new place, or ignored there.
/// - A local-only directory is ignored at its new place; or uploaded from it with what the
///   walk found. A walk that found nothing — the directory is marked with a OneDrive id
///   the store does not have yet, so the mount shows it as local-only and the walk, which
///   takes only what is unmarked, passes over it — is recorded as a directory that is
///   ignored from now on.
/// - What was at the new place is displaced with it: by its OneDrive id, or by its handle.
pub(super) fn describe_rename(plan: &RenamePlan, write_state: WriteState, handle_at_new_place: Option<FileHandle>, uploaded_entries: Vec<LocalOnlyEntry>) -> EntryRenamed {
    if let Some((file, over)) = plan.find_saved_over_file() {
        return EntryRenamed::SavedOverOneDriveFile { file: file.on_disk.clone(), write_state, over: over.to_owned(), to: plan.to.clone() };
    }
    let entry = match (&plan.moved_entry.state, plan.to_upload_status) {
        (EntryState::File(FileSyncState::RemoteOnly(row) | FileSyncState::Synced(row, _) | FileSyncState::ModifiedHere(row, _)), _) => MovedEntry::InOneDrive { onedrive_id: row.id.clone(), kind: EntryKind::File, handle: handle_at_new_place },
        (EntryState::Directory(DirectorySyncState::InOneDrive(row)), _) => MovedEntry::InOneDrive { onedrive_id: row.id.clone(), kind: EntryKind::Directory, handle: handle_at_new_place },
        (EntryState::File(FileSyncState::LocalOnly(file)), UploadStatus::Uploaded) => MovedEntry::LocalOnlyFile { file: file.on_disk.clone(), write_state },
        (EntryState::File(FileSyncState::LocalOnly(file)), UploadStatus::Ignored) => MovedEntry::IgnoredLocalOnlyFile { file: file.on_disk.clone() },
        (EntryState::Directory(DirectorySyncState::LocalOnly(directory)), UploadStatus::Ignored) => MovedEntry::IgnoredLocalOnlyDirectory { handle: directory.on_disk.handle.clone() },
        (EntryState::Directory(DirectorySyncState::LocalOnly(directory)), UploadStatus::Uploaded) if uploaded_entries.is_empty() => MovedEntry::IgnoredLocalOnlyDirectory { handle: directory.on_disk.handle.clone() },
        (EntryState::Directory(DirectorySyncState::LocalOnly(_)), UploadStatus::Uploaded) => MovedEntry::LocalOnlyDirectory { uploaded_entries },
    };
    let displaced = plan.target.as_ref().map(|target| match &target.state {
        EntryState::File(FileSyncState::RemoteOnly(row) | FileSyncState::Synced(row, _) | FileSyncState::ModifiedHere(row, _)) | EntryState::Directory(DirectorySyncState::InOneDrive(row)) => DisplacedEntry::InOneDrive(row.id.clone()),
        EntryState::File(FileSyncState::LocalOnly(local_only)) | EntryState::Directory(DirectorySyncState::LocalOnly(local_only)) => DisplacedEntry::LocalOnly(local_only.on_disk.handle.clone()),
    });
    EntryRenamed::Moved { entry, from: plan.from.clone(), to: plan.to.clone(), displaced }
}

/// What a rename that is not refused does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RenameDecision {
    Rename(RenamePlan),
    /// The new place is the old one: nothing moves, and the program is answered that it did.
    NothingToRename,
}

/// What the rename of `places` is, or the errno it is refused with, in this order:
///
/// 1. a node that stands for no directory (`ENOENT`);
/// 2. a new name that is one of the daemon's own (`EPERM`);
/// 3. nothing at the old place (`ENOENT`);
/// 4. the new place is the old one: nothing to rename;
/// 5. something at the new place: any entry when `mode` keeps it (`EEXIST`), a directory
///    under a file (`EISDIR`), a file under a directory (`ENOTDIR`), a directory that holds
///    something under a directory (`ENOTEMPTY`);
/// 6. what is in OneDrive moved to where nothing is uploaded (`EXDEV`): a program that
///    copies it there and removes it here does what that means.
pub(super) fn plan_rename(mode: RenameMode, places: RenamePlaces) -> Result<RenameDecision, Errno> {
    let from_directory = places.from_directory.ok_or(Errno::ENOENT)?;
    let to_directory = places.to_directory.ok_or(Errno::ENOENT)?;
    if places.new_name_is_daemon_owned {
        return Err(Errno::EPERM);
    }
    let moved_entry = places.moved_entry.ok_or(Errno::ENOENT)?;
    let (from, to) = (from_directory.path.join(&places.name), EntryPlace::new(&to_directory, &places.new_name));
    if from == to.path() {
        return Ok(RenameDecision::NothingToRename);
    }
    if let Some(target) = &places.target {
        match (&moved_entry.state, &target.state) {
            _ if mode == RenameMode::NoReplace => return Err(Errno::EEXIST),
            (EntryState::File(_), EntryState::Directory(_)) => return Err(Errno::EISDIR),
            (EntryState::Directory(_), EntryState::File(_)) => return Err(Errno::ENOTDIR),
            (EntryState::Directory(_), EntryState::Directory(_)) if places.target_holds_entries => return Err(Errno::ENOTEMPTY),
            (EntryState::File(_), EntryState::File(_)) | (EntryState::Directory(_), EntryState::Directory(_)) => {}
        }
    }
    if moved_entry.onedrive_id().is_some() && places.to_is_in_ignored_directory {
        return Err(Errno::EXDEV);
    }
    Ok(RenameDecision::Rename(RenamePlan { moved_entry, target: places.target, from, to, to_upload_status: places.to_upload_status }))
}

#[cfg(test)]
mod tests;
