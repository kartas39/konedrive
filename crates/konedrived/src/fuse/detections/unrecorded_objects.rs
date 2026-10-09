//! The local objects a walk of the folder found, recorded where the outbox has no row for
//! them.
//!
//! A change is recorded as it is made, so nothing is looked for afterwards — but a row can
//! be lost: the daemon killed between a change in the folder and its record, the outbox's
//! rows dropped by a switch to read-only, a name taken off the ignore list. A local object
//! without a row would never be uploaded, and a directory without one holds back everything
//! below it.

use konedrive_tree::TreeError;

use super::{create_entries, FolderEvent, LocalEntry, OutboxBatch, StoreFacts};

/// The local objects the folder holds, already without what stays on this computer.
#[derive(Debug)]
pub(in crate::fuse) struct UnrecordedObjects(pub(in crate::fuse) Vec<LocalEntry>);

impl FolderEvent for UnrecordedObjects {
    fn detect(self, facts: &impl StoreFacts, now: i64) -> Result<OutboxBatch, TreeError> {
        // An object whose rows cannot be read is left for the next walk.
        let unknown = |entry: &LocalEntry| facts.outbox_rows_of(&entry.object.handle).is_ok_and(|rows| rows.is_empty());
        let unknown: Vec<LocalEntry> = self.0.into_iter().filter(unknown).collect();
        Ok(OutboxBatch::of(create_entries(&unknown, now)))
    }
}

#[cfg(test)]
mod tests;
