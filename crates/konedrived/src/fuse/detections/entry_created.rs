//! A file or a directory a program made in the folder.

use konedrive_tree::{Row, TreeError};

use crate::fuse::entry_place::EntryPlace;
use crate::fuse::local_only_entry::LocalOnlyEntry;
use super::{detect_local_only_entry, detect_modified, wait_for_writer, FolderEvent, OutboxBatch, StoreFacts, WriteState};
use crate::fuse::backing_directory::DiskEntry;

/// What a program made at a place where nothing was.
#[derive(Debug)]
pub(in crate::fuse) enum EntryCreated {
    /// A new file or directory, which is not in OneDrive: created or made there.
    Fresh { entry: LocalOnlyEntry, write_state: WriteState },
    /// A file made where the user just removed the OneDrive file `base`, whose removal is
    /// not sent yet: `file` is that file's new content. Programs replace a file that way,
    /// and it keeps its history and its links in OneDrive.
    ReplacingRemovedFile { place: EntryPlace, file: DiskEntry, write_state: WriteState, base: Row },
}

impl FolderEvent for EntryCreated {
    fn detect(self, _facts: &impl StoreFacts, now: i64) -> Result<OutboxBatch, TreeError> {
        Ok(OutboxBatch::of([match self {
            EntryCreated::Fresh { entry, write_state } => detect_local_only_entry(&entry.place, &entry.on_disk, write_state, now),
            // Not held back a moment when it is not open: it merges into the removal's row,
            // which was ready.
            EntryCreated::ReplacingRemovedFile { place, file, write_state, base } => {
                let mut update = detect_modified(&place, &base, &file);
                if write_state == WriteState::OpenForWriting {
                    wait_for_writer(&mut update, now);
                }
                update
            }
        }]))
    }
}

#[cfg(test)]
mod tests;
