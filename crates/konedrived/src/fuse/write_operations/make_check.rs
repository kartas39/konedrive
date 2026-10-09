//! Whether a file or a directory may be made at a place, and what one that was made is
//! recorded as: two decisions, of what stands at the place, with no disk in them.

use fuser::Errno;
use konedrive_tree::Row;

use crate::fuse::backing_directory::EntryKind;
use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::upload_rules::UploadStatus;

/// What a program asks to have made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fuse) enum EntryToMake {
    /// A directory (`mkdir`).
    Directory,
    /// A file that is not opened (`mknod`).
    File,
    /// A file the program holds open for writing from the start (`create`).
    FileOpenedForWriting,
}

impl EntryToMake {
    pub(in crate::fuse) fn kind(self) -> EntryKind {
        match self {
            EntryToMake::Directory => EntryKind::Directory,
            EntryToMake::File | EntryToMake::FileOpenedForWriting => EntryKind::File,
        }
    }
}

/// What stands at the place an entry is to be made at, looked up before the check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MakePlace {
    /// The directory the node the program named stands for; `None` when it stands for none.
    pub(super) parent_directory: Option<DirectoryReference>,
    /// That directory is the folder itself.
    pub(super) is_in_root: bool,
    /// The name is one the daemon uses for itself in the folder.
    pub(super) name_is_daemon_owned: bool,
    /// The name is a file manager's trash directory's.
    pub(super) name_is_trash: bool,
    /// The directory holds an entry of that name already.
    pub(super) name_is_taken: bool,
}

/// The directory `entry_to_make` is made in, or the errno it is refused with, in this
/// order: no such directory (`ENOENT`); one of the daemon's own names, or a file manager's
/// trash directory in the folder itself (`EPERM`); a name that is taken (`EEXIST`).
pub(super) fn check_make(entry_to_make: EntryToMake, place: MakePlace) -> Result<DirectoryReference, Errno> {
    let parent_directory = place.parent_directory.ok_or(Errno::ENOENT)?;
    let is_trash_in_root = entry_to_make == EntryToMake::Directory && place.is_in_root && place.name_is_trash;
    if place.name_is_daemon_owned || is_trash_in_root {
        return Err(Errno::EPERM);
    }
    if place.name_is_taken {
        return Err(Errno::EEXIST);
    }
    Ok(parent_directory)
}

/// What an entry that was made is recorded as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MadeEntryRecord {
    /// A new local-only entry, to be created or made in OneDrive.
    Fresh,
    /// The new content of the OneDrive file with this row, which the user just removed from
    /// the place and whose removal is not sent yet.
    ReplacingRemovedFile(Row),
    /// Nothing: the entry is ignored at its place.
    NotRecorded,
}

/// What `entry_to_make`, made at a place, is recorded as: `removed_file` is the OneDrive file
/// the user removed from the place and whose removal is not sent yet, and `upload_status`
/// what the upload rules say of the place. A file made where one was removed is that file's
/// new content, whatever the rules say; a directory never is.
pub(super) fn choose_made_entry_record(entry_to_make: EntryToMake, removed_file: Option<Row>, upload_status: UploadStatus) -> MadeEntryRecord {
    match (entry_to_make.kind(), removed_file, upload_status) {
        (EntryKind::File, Some(base), _) => MadeEntryRecord::ReplacingRemovedFile(base),
        (_, _, UploadStatus::Uploaded) => MadeEntryRecord::Fresh,
        (_, _, UploadStatus::Ignored) => MadeEntryRecord::NotRecorded,
    }
}

#[cfg(test)]
mod tests;
