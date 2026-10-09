//! The local-only entries the outbox has no row for, found again when the folder starts
//! taking changes ([`UnrecordedEntries`]): one walk of the backing directory, recorded in
//! one go.

use super::WriteOperations;
use crate::fuse::detections::UnrecordedEntries;
use crate::fuse::directory::Directory;
use crate::fuse::local_only_entries::collect_local_only_entries;

impl WriteOperations {
    /// Records every local-only entry the outbox has no row for. Called on the recorder's
    /// thread.
    pub(in crate::fuse) fn record_unrecorded_entries(&self) {
        let mut found = Vec::new();
        let mut directories = vec![self.directory_contents.find_root_directory()];
        while let Some(directory) = directories.pop() {
            for listed in self.backing_directory.list_directory(&directory.rel) {
                let Some(name) = listed.file_name().to_str().map(str::to_owned) else { continue };
                let rel = directory.rel.join(&name);
                if self.directory_contents.find_local_only(&directory, &name).is_some() {
                    // With what it holds, when it is a directory.
                    found.extend(collect_local_only_entries(&self.backing_directory, &self.upload_rules, &directory, &name));
                } else if listed.file_type().is_ok_and(|kind| kind.is_dir()) {
                    directories.push(Directory { id: self.backing_directory.read_onedrive_id(&rel), rel });
                }
            }
        }
        if found.is_empty() {
            return;
        }
        match self.outbox_recorder.record(UnrecordedEntries(found)) {
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
