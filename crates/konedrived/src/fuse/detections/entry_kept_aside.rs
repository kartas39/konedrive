//! Where a change from OneDrive finds something the user made here in its way: what is kept,
//! and what that is recorded as.
//!
//! A file or directory of OneDrive that comes to a name where a local-only entry is does not
//! hide it: the entry is kept beside it under a conflict copy's name
//! ([`ConflictCopies`](crate::fuse::conflict_copies::ConflictCopies)) and is uploaded from
//! there. A directory OneDrive removed that still holds local-only entries stays, as a
//! local-only directory, and is made again in OneDrive with what it holds
//! ([`detect_local_only_entries`](super::detect_local_only_entries)), unless the directory on
//! disk backs another directory of OneDrive ([`is_directory_of`](super::is_directory_of)).
//!
//! These are decided inside a cycle of the delta listing, which records them in its own
//! transaction: they read nothing from the store.

use std::path::PathBuf;

use crate::fuse::entry_place::EntryPlace;
use super::{detect_local_only_entry, rebase, OutboxBatch, WriteState};
use crate::fuse::upload_rules::UploadDecision;
use crate::fuse::backing_directory::{DiskEntry, EntryKind};

/// The local-only entry `on_disk` was at `from`, where something of OneDrive comes, and was
/// renamed to `to` beside it; `upload_decision` says whether the conflict copy's name is one
/// that is uploaded, and `write_state` whether a program holds the entry open for writing: a
/// conflict copy that is still written waits for its last writer.
#[derive(Debug)]
pub(in crate::fuse) struct EntryKeptAside {
    pub(in crate::fuse) on_disk: DiskEntry,
    pub(in crate::fuse) from: PathBuf,
    pub(in crate::fuse) to: EntryPlace,
    pub(in crate::fuse) upload_decision: UploadDecision,
    pub(in crate::fuse) write_state: WriteState,
}

impl EntryKeptAside {
    /// The conflict copy is created, or made, from its new place; the rows of what is below
    /// a directory follow it there.
    pub(in crate::fuse) fn detect(self, now: i64) -> OutboxBatch {
        let ops = if self.on_disk.metadata.kind == EntryKind::Directory { vec![rebase(&self.from, &self.to.path())] } else { Vec::new() };
        let detections = match self.upload_decision {
            UploadDecision::Upload => vec![detect_local_only_entry(&self.to, &self.on_disk, self.write_state, now)],
            UploadDecision::Ignore => Vec::new(),
        };
        OutboxBatch { detections, ops }
    }
}

#[cfg(test)]
mod tests;
