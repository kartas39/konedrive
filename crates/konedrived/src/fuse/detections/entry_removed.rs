//! A file or a directory a program removed from the folder.

use std::path::PathBuf;

use konedrive_fs::handle::FileHandle;
use konedrive_tree::TreeError;

use super::{delete_item, forget_object, FolderEvent, OutboxBatch, StoreFacts};

/// What a program removed, and from where.
#[derive(Debug)]
pub(in crate::fuse) enum EntryRemoved {
    /// An item: removed from OneDrive by the outbox.
    InOneDrive { id: String, rel: PathBuf },
    /// A local object: what the outbox held of it goes.
    LocalOnly { handle: FileHandle, rel: PathBuf },
}

impl FolderEvent for EntryRemoved {
    fn detect(self, facts: &impl StoreFacts, _now: i64) -> Result<OutboxBatch, TreeError> {
        Ok(OutboxBatch::of(match self {
            EntryRemoved::InOneDrive { id, rel } => delete_item(facts, &id, &rel)?,
            EntryRemoved::LocalOnly { handle, rel } => forget_object(facts, &handle, &rel)?,
        }))
    }
}

#[cfg(test)]
mod tests;
