//! A file or a directory a program removes from the folder: recorded first, then removed
//! from the backing directory. A local-only entry that cannot be removed from there is
//! recorded again: the outbox holds it as long as the folder does.

use fuser::Errno;

use super::remove_plan::{plan_remove, RemovePlaceContents, RemovePlan};
use super::WriteOperations;
use crate::fuse::backing_directory::EntryKind;
use crate::fuse::detections::{EntryRemoved, UnrecordedEntries};
use crate::fuse::errno::to_errno;
use crate::fuse::upload_rules::UploadDecision;
use crate::fuse::node_table::NodeId;

impl WriteOperations {
    /// Removes `name` from the directory node `parent`, as a `kind`: a directory, which must
    /// hold nothing, or a file. What is in OneDrive is removed there by the outbox; a
    /// local-only entry is forgotten there.
    pub(in crate::fuse) fn remove(&self, parent: NodeId, name: &str, kind: EntryKind) -> Result<(), Errno> {
        self.access_mode_switch.check_read_write()?;
        let plan = plan_remove(kind, name, self.find_remove_place_contents(parent, name))?;
        let (event, rel) = match &plan {
            RemovePlan::OneDriveFile { onedrive_id, path } | RemovePlan::OneDriveDirectory { onedrive_id, path } => (EntryRemoved::InOneDrive { onedrive_id: onedrive_id.clone(), path: path.clone() }, path),
            RemovePlan::LocalOnlyFile { handle, path } | RemovePlan::LocalOnlyDirectory { handle, path } => (EntryRemoved::LocalOnly { handle: handle.clone(), path: path.clone() }, path),
        };
        self.record_or_undo(event, format_args!("the removal of {}", rel.display()), || {})?;
        let removed = match &plan {
            // The directory in the backing directory is removed when nothing is left in it.
            RemovePlan::OneDriveDirectory { path, .. } => self.backing_directory.remove_directory(path).or(Ok(())),
            RemovePlan::OneDriveFile { onedrive_id, path } => {
                self.backing_directory.remove_downloaded_copy(path, onedrive_id);
                Ok(())
            }
            RemovePlan::LocalOnlyDirectory { path, .. } => self.backing_directory.remove_directory(path),
            RemovePlan::LocalOnlyFile { path, .. } => self.backing_directory.remove_file(path),
        };
        if let Err(e) = removed {
            self.record_entry_still_there(parent, name);
            return Err(to_errno(&e));
        }
        if let RemovePlan::LocalOnlyFile { handle, .. } | RemovePlan::LocalOnlyDirectory { handle, .. } = &plan {
            self.node_resolver.drop_node(handle);
        }
        Ok(())
    }

    /// The local-only entry `name` of the directory node `parent` was recorded as removed,
    /// and could not be removed from the backing directory — a directory that holds what the
    /// mount does not show, a disk that refuses: it is still there, so the record is taken
    /// back, and the entry is recorded again, as one the outbox has no row for. A directory
    /// is recorded alone: the mount showed nothing in it.
    fn record_entry_still_there(&self, parent: NodeId, name: &str) {
        let Some(parent_directory) = self.node_resolver.find_directory(parent) else { return };
        let Some(still_there) = self.directory_contents.find_local_only(&parent_directory, name) else { return };
        if self.decide_upload(&parent_directory, name) == UploadDecision::Ignore {
            return;
        }
        if let Err(e) = self.outbox_recorder.record(UnrecordedEntries(vec![still_there])) {
            tracing::warn!("{}, which could not be removed, could not be recorded again: {e}", parent_directory.path.join(name).display());
        }
    }

    /// What stands at `name` of the directory node `parent`, for the plan of a remove.
    fn find_remove_place_contents(&self, parent: NodeId, name: &str) -> RemovePlaceContents {
        let parent_directory = self.node_resolver.find_directory(parent);
        let entry = parent_directory.as_ref().and_then(|directory| self.directory_contents.find_entry(directory, name));
        let holds_entries = parent_directory.as_ref().zip(entry.as_ref()).is_some_and(|(directory, entry)| self.holds_entries(entry, &directory.path.join(name)));
        RemovePlaceContents { parent_directory, entry, holds_entries }
    }
}

#[cfg(test)]
mod tests;
