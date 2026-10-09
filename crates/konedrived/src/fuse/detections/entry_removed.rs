//! A file or a directory a program removed from the folder.

use std::path::PathBuf;

use konedrive_fs::handle::FileHandle;
use konedrive_tree::TreeError;

use super::{detect_removed_by_id, detect_forgotten, FolderEvent, OutboxBatch, StoreFacts};

/// What a program removed, and from where.
#[derive(Debug)]
pub(in crate::fuse) enum EntryRemoved {
    /// A file or directory of OneDrive, by its id: removed from OneDrive by the outbox.
    InOneDrive { onedrive_id: String, path: PathBuf },
    /// A local-only entry: what the outbox held of it is removed.
    LocalOnly { handle: FileHandle, path: PathBuf },
}

impl FolderEvent for EntryRemoved {
    fn detect(self, facts: &impl StoreFacts, _now: i64) -> Result<OutboxBatch, TreeError> {
        Ok(OutboxBatch::of(match self {
            EntryRemoved::InOneDrive { onedrive_id, path } => detect_removed_by_id(facts, &onedrive_id, &path)?,
            EntryRemoved::LocalOnly { handle, path } => detect_forgotten(facts, &handle, &path)?,
        }))
    }
}

#[cfg(test)]
mod tests;
