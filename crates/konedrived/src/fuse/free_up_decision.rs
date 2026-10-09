//! Whether the downloaded copy of a file is freed: the one decision of a free-up, with no
//! disk, store or mount in it. Whoever frees space asks here — the walk that frees what is
//! below a path, for each downloaded copy, and the folder's sync, for a path it is asked to
//! free ([`PathStatus`](super::files_interface::PathStatus)).

use super::directory_contents::{EntryState, FileSyncState};

/// What a free-up does with a file or directory of OneDrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeUpDecision {
    /// Its downloaded copy is removed: the file stays in the folder as a name.
    Free,
    /// A pin keeps it on this device: its own, or a directory's above it.
    LeavePinned,
    /// Its content was changed here and is not sent yet: its copy is the only one there is.
    LeaveModifiedHere,
    /// A program holds it open for writing.
    LeaveOpenForWriting,
    /// It is being downloaded whole.
    LeaveDownloading,
    /// Nothing of it is downloaded, and nothing is to free: a file that is not downloaded,
    /// and a directory, which has no copy of its own.
    NotDownloaded,
}

/// What a free-up does with what is in `state`, for whether a pin keeps it (`is_pinned`),
/// whether it is being downloaded whole and whether a program holds it open for writing.
/// The first reason that holds is the one given, in the order of [`FreeUpDecision`]'s
/// cases: a pin before a change that is not sent, that before a writer, a writer before a
/// download.
pub(super) fn decide_free_up(state: &EntryState, is_pinned: bool, is_downloading: bool, is_open_for_writing: bool) -> FreeUpDecision {
    match state {
        _ if is_pinned => FreeUpDecision::LeavePinned,
        EntryState::File(FileSyncState::ModifiedHere(..)) => FreeUpDecision::LeaveModifiedHere,
        // The sync state says this too while the outbox holds the file's row; the open
        // handles say it also once that row is dropped under a writer, by a forced switch to
        // read-only.
        _ if is_open_for_writing => FreeUpDecision::LeaveOpenForWriting,
        _ if is_downloading => FreeUpDecision::LeaveDownloading,
        EntryState::File(FileSyncState::Synced(..)) => FreeUpDecision::Free,
        EntryState::File(FileSyncState::RemoteOnly(_) | FileSyncState::LocalOnly(_)) | EntryState::Directory(_) => FreeUpDecision::NotDownloaded,
    }
}

#[cfg(test)]
mod tests;
