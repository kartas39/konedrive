//! What the folder holds of the user's own that the outbox does not know of, found again when
//! the folder starts taking changes.
//!
//! A change is recorded as it is made, so nothing is looked for afterwards — but a row can be
//! lost: the daemon killed between a change in the folder and its record, the outbox's rows
//! dropped by a switch to read-only, a name taken off the ignore list. A file or directory
//! without an item and without a row would then never be sent, and a directory without one
//! holds back everything below it. One walk of the folder under the mount records them anew.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use super::local::{carried, Dir};
use super::shared::Shared;

impl Shared {
    /// Records every file and directory without an item that the outbox has no row for.
    /// Called on the recorder's thread.
    pub(super) fn settle(self: &Arc<Self>) {
        let mut found = Vec::new();
        let mut dirs = vec![Dir { id: self.read(|s| s.root_item_id()).flatten(), rel: PathBuf::new() }];
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(self.under(&dir.rel)).into_iter().flatten().flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
                let rel = dir.rel.join(&name);
                if self.local(&rel).is_some() {
                    // With what it holds, when it is a directory.
                    self.below(&dir, &name, &mut found);
                } else if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    dirs.push(Dir { id: carried(&self.under(&rel)), rel });
                }
            }
        }
        if found.is_empty() {
            return;
        }
        let recorded = self.record(move |s| {
            let mut unknown = Vec::new();
            for detection in found {
                if detection.inode.as_ref().is_some_and(|inode| s.outbox_for_inode(inode).is_ok_and(|rows| rows.is_empty())) {
                    unknown.push(detection);
                }
            }
            if !unknown.is_empty() {
                tracing::info!("{} file(s) and folder(s) made here were not waiting to be uploaded; they are now", unknown.len());
            }
            Ok((unknown, Vec::new()))
        });
        if let Err(e) = recorded {
            tracing::warn!("what the folder holds of the user's own could not be looked for: {e}");
        }
    }
}
