//! A file or a directory a program removes from the folder: recorded first, then removed
//! from the backing directory.

use fuser::Errno;

use super::remove_plan::{plan_remove, RemovePlace, RemovePlan};
use super::WriteOperations;
use crate::fuse::backing_directory::EntryKind;
use crate::fuse::detections::EntryRemoved;
use crate::fuse::errno::to_errno;
use crate::fuse::node_table::NodeId;

impl WriteOperations {
    /// Removes `name` from the directory node `parent`, as a `kind`: a directory, which must
    /// hold nothing, or a file. What is in OneDrive is removed there by the outbox; a
    /// local-only entry is forgotten there.
    pub(in crate::fuse) fn remove(&self, parent: NodeId, name: &str, kind: EntryKind) -> Result<(), Errno> {
        self.access_mode_switch.check_read_write()?;
        let plan = plan_remove(kind, name, self.find_remove_place(parent, name))?;
        let (event, rel) = match &plan {
            RemovePlan::OneDriveFile { id, path } | RemovePlan::OneDriveDirectory { id, path } => (EntryRemoved::InOneDrive { id: id.clone(), rel: path.clone() }, path),
            RemovePlan::LocalOnlyFile { handle, path } | RemovePlan::LocalOnlyDirectory { handle, path } => (EntryRemoved::LocalOnly { handle: handle.clone(), rel: path.clone() }, path),
        };
        self.record_or_undo(event, format_args!("the removal of {}", rel.display()), || {})?;
        match &plan {
            // The directory in the backing directory is removed when nothing is left in it.
            RemovePlan::OneDriveDirectory { path, .. } => drop(self.backing_directory.remove_dir(path)),
            RemovePlan::OneDriveFile { id, path } => drop(self.backing_directory.remove_downloaded_copy(path, id)),
            RemovePlan::LocalOnlyDirectory { path, .. } => self.backing_directory.remove_dir(path).map_err(|e| to_errno(&e))?,
            RemovePlan::LocalOnlyFile { path, .. } => self.backing_directory.remove_file(path).map_err(|e| to_errno(&e))?,
        }
        if let RemovePlan::LocalOnlyFile { handle, .. } | RemovePlan::LocalOnlyDirectory { handle, .. } = &plan {
            self.node_resolver.drop_node(handle);
        }
        Ok(())
    }

    /// What stands at `name` of the directory node `parent`, for the plan of a remove.
    fn find_remove_place(&self, parent: NodeId, name: &str) -> RemovePlace {
        let parent_directory = self.node_resolver.find_directory(parent);
        let entry = parent_directory.as_ref().and_then(|directory| self.directory_contents.find_entry(directory, name));
        let holds_entries = parent_directory.as_ref().zip(entry.as_ref()).is_some_and(|(directory, entry)| self.holds_entries(entry, &directory.path.join(name)));
        RemovePlace { parent_directory, entry, holds_entries }
    }
}

#[cfg(test)]
mod tests;
