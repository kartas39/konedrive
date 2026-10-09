//! A file a program writes: opened for writing, closed by its last writer, or cut.
//!
//! From its first writer on the outbox holds the file, so that nothing from OneDrive
//! replaces what a program is about to write; it is sent once the last writer closed it.

use konedrive_tree::TreeError;

use crate::fuse::entry_place::EntryPlace;
use crate::fuse::local_only_entry::LocalOnlyEntry;
use super::{detect_local_only_entry, detect_moved, detect_modified, wait_for_quiet, wait_for_writer, FolderEvent, OutboxBatch, StoreFacts, WriteState};
use crate::fuse::backing_directory::DiskEntry;

/// Which file was written, where it is, and what was done to it.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::fuse) enum FileWritten {
    /// A file of OneDrive, by its id, in its downloaded copy.
    InOneDrive { onedrive_id: String, place: EntryPlace, write: OneDriveFileWrite },
    /// A file that is not in OneDrive yet.
    LocalOnly { file: LocalOnlyEntry, write: LocalOnlyFileWrite },
}

/// What was done to a file of OneDrive; with its copy as it is now, where its content counts.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::fuse) enum OneDriveFileWrite {
    /// Its first writer opened it: its content is the user's own from here on, and waits.
    Opened(DiskEntry),
    /// Its last writer closed it, and its content was changed: due a moment later.
    ClosedChanged(DiskEntry),
    /// It was cut or extended by a program that does not hold it open: as closed.
    Cut(DiskEntry),
    /// Its last writer closed it, and nothing was written: the file is where it is with the
    /// content the base has, and the row that held it is removed, or is the move it was.
    ClosedUntouched,
}

/// What was done to a local-only file. One closed with nothing written is sent all the same:
/// it was never sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fuse) enum LocalOnlyFileWrite {
    /// Its first writer opened it: it waits for the last.
    Opened,
    /// Its last writer closed it: due a moment later.
    Closed,
    /// It was cut or extended by a program that does not hold it open: as closed.
    Cut,
}

impl FolderEvent for FileWritten {
    fn detect(self, facts: &impl StoreFacts, now: i64) -> Result<OutboxBatch, TreeError> {
        match self {
            FileWritten::LocalOnly { file, write } => {
                let write_state = match write {
                    LocalOnlyFileWrite::Opened => WriteState::OpenForWriting,
                    LocalOnlyFileWrite::Closed | LocalOnlyFileWrite::Cut => WriteState::NotOpen,
                };
                Ok(OutboxBatch::of([detect_local_only_entry(&file.place, &file.on_disk, write_state, now)]))
            }
            FileWritten::InOneDrive { onedrive_id, place, write } => {
                let Some(base) = facts.base_row(&onedrive_id)? else { return Ok(OutboxBatch::default()) };
                Ok(OutboxBatch::of([match write {
                    OneDriveFileWrite::Opened(copy) => {
                        let mut update = detect_modified(&place, &base, &copy);
                        wait_for_writer(&mut update, now);
                        update
                    }
                    OneDriveFileWrite::ClosedChanged(copy) | OneDriveFileWrite::Cut(copy) => {
                        let mut update = detect_modified(&place, &base, &copy);
                        wait_for_quiet(&mut update, now);
                        update
                    }
                    OneDriveFileWrite::ClosedUntouched => {
                        let mut here = detect_moved(&place, &base, None);
                        here.same_content = true;
                        here
                    }
                }]))
            }
        }
    }
}

#[cfg(test)]
mod tests;
