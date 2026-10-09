//! Where a change from OneDrive meets something the user made here: what is kept, and what
//! that is recorded as.
//!
//! An item that comes to a name where a local object stands does not hide it: the object is
//! kept beside it under a copy's name
//! ([`UploadRules::copy_name`](crate::fuse::upload_rules::UploadRules::copy_name)) and is
//! uploaded from there. A folder OneDrive removed that still holds local objects stays, as a
//! local directory, and is made again in OneDrive with what it holds
//! ([`create_local_only_entries`](super::create_local_only_entries)), unless its directory is another item's
//! ([`is_directory_of`](super::is_directory_of)).
//!
//! These are decided inside a cycle of the delta listing, which records them in its own
//! transaction: they read nothing from the store.

use std::path::PathBuf;

use super::{create_or_mkdir, rebase, LocalObject, OutboxBatch, FolderPlace};

/// The local object `object` stood at `from`, where an item comes, and was renamed to `to`
/// beside it. `uploaded`: the copy's name is one that is uploaded.
#[derive(Debug)]
pub(in crate::fuse) struct EntryKeptAside {
    pub(in crate::fuse) object: LocalObject,
    pub(in crate::fuse) from: PathBuf,
    pub(in crate::fuse) to: FolderPlace,
    pub(in crate::fuse) uploaded: bool,
}

impl EntryKeptAside {
    /// The copy is created, or made, from its new place; the rows of what is below a
    /// directory follow it there.
    pub(in crate::fuse) fn detect(self, now: i64) -> OutboxBatch {
        let ops = if self.object.is_dir() { vec![rebase(&self.from, &self.to.rel())] } else { Vec::new() };
        let detections = if self.uploaded { vec![create_or_mkdir(&self.to, &self.object, false, now)] } else { Vec::new() };
        OutboxBatch { detections, ops }
    }
}

#[cfg(test)]
mod tests;
