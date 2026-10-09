//! What the folder holds of the user's own that the outbox has no row for, found again when
//! the folder starts taking changes ([`UnrecordedObjects`]): one walk of the backing
//! directory, recorded in one go.

use super::WriteOperations;
use crate::fuse::detections::UnrecordedObjects;
use crate::fuse::local_only_entries::collect_local_entries;
use crate::fuse::directory_contents::Directory;

impl WriteOperations {
    /// Records every local object the outbox has no row for. Called on the recorder's
    /// thread.
    pub(in crate::fuse) fn record_unrecorded_objects(&self) {
        let mut found = Vec::new();
        let mut dirs = vec![self.view.root_dir()];
        while let Some(dir) = dirs.pop() {
            for entry in self.under.list(&dir.rel) {
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
                let rel = dir.rel.join(&name);
                if self.view.local(&rel).is_some() {
                    // With what it holds, when it is a directory.
                    found.extend(collect_local_entries(&self.under, &self.rules, &dir, &name));
                } else if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    dirs.push(Directory { id: self.under.carried(&rel), rel });
                }
            }
        }
        if found.is_empty() {
            return;
        }
        match self.recorder.record(UnrecordedObjects(found)) {
            Ok(recorded) if !recorded.applied.queued.is_empty() => {
                tracing::info!("{} file(s) and folder(s) made here were not waiting to be uploaded; they are now", recorded.applied.queued.len());
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("what the folder holds of the user's own could not be looked for: {e}"),
        }
    }
}

#[cfg(test)]
mod tests;
