//! A file or a directory a program removed from the folder.

use std::path::PathBuf;

use konedrive_fs::handle::FileHandle;
use konedrive_tree::TreeError;

use super::{delete_in_onedrive, forget_local_only, FolderEvent, OutboxBatch, StoreFacts};

/// What a program removed, and from where.
#[derive(Debug)]
pub(in crate::fuse) enum EntryRemoved {
    /// A file or directory of OneDrive, by its id: removed from OneDrive by the outbox.
    InOneDrive { id: String, rel: PathBuf },
    /// A local-only entry: what the outbox held of it is removed.
    LocalOnly { handle: FileHandle, rel: PathBuf },
}

impl FolderEvent for EntryRemoved {
    fn detect(self, facts: &impl StoreFacts, _now: i64) -> Result<OutboxBatch, TreeError> {
        Ok(OutboxBatch::of(match self {
            EntryRemoved::InOneDrive { id, rel } => delete_in_onedrive(facts, &id, &rel)?,
            EntryRemoved::LocalOnly { handle, rel } => forget_local_only(facts, &handle, &rel)?,
        }))
    }
}

#[cfg(test)]
mod tests;
