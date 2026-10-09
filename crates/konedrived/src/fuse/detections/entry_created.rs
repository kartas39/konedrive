//! A file or a directory a program made in the folder.

use konedrive_tree::{Row, TreeError};

use super::{create_or_mkdir, update_item, wait_for_writer, FolderEvent, LocalObject, OutboxBatch, FolderPlace, StoreFacts};

/// What a program made at a place where nothing stood.
#[derive(Debug)]
pub(in crate::fuse) enum EntryCreated {
    /// A new file or directory, which OneDrive has no item for: created or made there.
    /// `open`: a file the program holds open for writing.
    Fresh { place: FolderPlace, object: LocalObject, open: bool },
    /// A file made where the user just removed the file of the item `base`, whose removal
    /// is not sent yet: it is that item's new content. Programs replace a file that way, and
    /// the item keeps its history and its links.
    ReplacingRemovedFile { place: FolderPlace, file: LocalObject, open: bool, base: Row },
}

impl FolderEvent for EntryCreated {
    fn detect(self, _facts: &impl StoreFacts, now: i64) -> Result<OutboxBatch, TreeError> {
        Ok(OutboxBatch::of([match self {
            EntryCreated::Fresh { place, object, open } => create_or_mkdir(&place, &object, open, now),
            // Not held back a moment when it is not open: it merges into the removal's row,
            // which was ready.
            EntryCreated::ReplacingRemovedFile { place, file, open, base } => {
                let mut update = update_item(&place, &base, &file);
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
