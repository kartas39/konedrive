//! A file a program writes: opened for writing, closed by its last writer, or cut.
//!
//! From its first writer on the outbox holds the file, so that nothing from OneDrive
//! replaces what a program is about to write; it is sent once the last writer closed it.

use konedrive_tree::TreeError;

use super::{create_or_mkdir, move_in_onedrive, update_content, wait_for_quiet, wait_for_writer, FolderEvent, LocalObject, OutboxBatch, FolderPlace, StoreFacts};

/// Whose file was written, where it stands, and what was done to it.
#[derive(Debug)]
pub(in crate::fuse) enum FileWritten {
    /// An item's file, in its downloaded copy.
    InOneDrive { id: String, place: FolderPlace, write: OneDriveFileWrite },
    /// A file OneDrive has no item for yet.
    LocalOnly { place: FolderPlace, file: LocalObject, write: LocalOnlyFileWrite },
}

/// What was done to an item's file; with the copy as it is now, where its content counts.
#[derive(Debug)]
pub(in crate::fuse) enum OneDriveFileWrite {
    /// Its first writer opened it: its content is the user's own from here on, and waits.
    Opened(LocalObject),
    /// Its last writer closed it, and its content was changed: due a moment later.
    ClosedChanged(LocalObject),
    /// It was cut or extended by a program that does not hold it open: as closed.
    Cut(LocalObject),
    /// Its last writer closed it, and nothing was written: the item is where it stands with
    /// the content the base has, and the row that held it goes, or is the move it was.
    ClosedUntouched,
}

/// What was done to a local file. One closed with nothing written is sent all the same: it
/// was never sent.
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
            FileWritten::LocalOnly { place, file, write } => Ok(OutboxBatch::of([create_or_mkdir(&place, &file, write == LocalOnlyFileWrite::Opened, now)])),
            FileWritten::InOneDrive { id, place, write } => {
                let Some(base) = facts.base_row(&id)? else { return Ok(OutboxBatch::default()) };
                Ok(OutboxBatch::of([match write {
                    OneDriveFileWrite::Opened(copy) => {
                        let mut update = update_content(&place, &base, &copy);
                        wait_for_writer(&mut update, now);
                        update
                    }
                    OneDriveFileWrite::ClosedChanged(copy) | OneDriveFileWrite::Cut(copy) => {
                        let mut update = update_content(&place, &base, &copy);
                        wait_for_quiet(&mut update, now);
                        update
                    }
                    OneDriveFileWrite::ClosedUntouched => {
                        let mut here = move_in_onedrive(&place, &base, None);
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
