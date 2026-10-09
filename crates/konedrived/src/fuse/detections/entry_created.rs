//! A file or a directory a program made in the folder.

use konedrive_tree::{Row, TreeError};

use crate::fuse::entry_place::EntryPlace;
use crate::fuse::local_only_entry::LocalOnlyEntry;
use super::{detect_local_only_entry, detect_modified, wait_for_writer, FolderEvent, OutboxBatch, StoreFacts};
use crate::fuse::backing_directory::DiskEntry;

/// What a program made at a place where nothing was.
#[derive(Debug)]
pub(in crate::fuse) enum EntryCreated {
    /// A new file or directory, which is not in OneDrive: created or made there. `open`: a
    /// file the program holds open for writing.
    Fresh { entry: LocalOnlyEntry, open: bool },
    /// A file made where the user just removed the OneDrive file `base`, whose removal is
    /// not sent yet: `file` is that file's new content. Programs replace a file that way,
    /// and it keeps its history and its links in OneDrive.
    ReplacingRemovedFile { place: EntryPlace, file: DiskEntry, open: bool, base: Row },
}

impl FolderEvent for EntryCreated {
    fn detect(self, _facts: &impl StoreFacts, now: i64) -> Result<OutboxBatch, TreeError> {
        Ok(OutboxBatch::of([match self {
            EntryCreated::Fresh { entry, open } => detect_local_only_entry(&entry.place, &entry.on_disk, open, now),
            // Not held back a moment when it is not open: it merges into the removal's row,
            // which was ready.
            EntryCreated::ReplacingRemovedFile { place, file, open, base } => {
                let mut update = detect_modified(&place, &base, &file);
                if open {
                    wait_for_writer(&mut update, now);
                }
                update
            }
        }]))
    }
}

#[cfg(test)]
mod tests;
