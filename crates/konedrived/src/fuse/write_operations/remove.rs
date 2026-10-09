//! A file or a directory a program removes from the folder: recorded first, then removed
//! from the backing directory.

use fuser::Errno;

use super::WriteOperations;
use crate::fuse::detections::EntryRemoved;
use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::directory_contents::{DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::errno::to_errno;

impl WriteOperations {
    /// Removes `name` from the directory node `parent`: a directory (`directory`), which must
    /// hold nothing, or a file. What is in OneDrive is removed there by the outbox; a
    /// local-only entry is forgotten there.
    pub(in crate::fuse) fn remove(&self, parent: u64, name: &str, directory: bool) -> Result<(), Errno> {
        self.access_mode.check_read_write()?;
        let parent_directory = self.node_resolver.find_directory(parent).ok_or(Errno::ENOENT)?;
        let entry = self.directory_contents.find_entry(&parent_directory, name).ok_or(Errno::ENOENT)?;
        match (&entry.state, directory) {
            (EntryState::Directory(_), false) => return Err(Errno::EISDIR),
            (EntryState::File(_), true) => return Err(Errno::ENOTDIR),
            _ => {}
        }
        let rel = parent_directory.path.join(name);
        if directory && !self.directory_contents.list_names(&DirectoryReference { onedrive_id: entry.onedrive_id().map(str::to_owned), path: rel.clone() }).is_empty() {
            return Err(Errno::ENOTEMPTY);
        }
        let event = match &entry.state {
            EntryState::File(FileSyncState::LocalOnly(local_only)) | EntryState::Directory(DirectorySyncState::LocalOnly(local_only)) => {
                EntryRemoved::LocalOnly { handle: local_only.on_disk.handle.clone(), rel: rel.clone() }
            }
            EntryState::File(FileSyncState::RemoteOnly(row) | FileSyncState::Synced(row, _) | FileSyncState::ModifiedHere(row, _)) | EntryState::Directory(DirectorySyncState::InOneDrive(row)) => {
                EntryRemoved::InOneDrive { id: row.id.clone(), rel: rel.clone() }
            }
        };
        self.record_or_undo(event, format_args!("the removal of {}", rel.display()), || {})?;
        match entry.row() {
            // The directory in the backing directory is removed when nothing is left in it.
            Some(_) if directory => drop(self.backing_directory.remove_dir(&rel)),
            Some(row) => drop(self.backing_directory.remove_downloaded_copy(&rel, &row.id)),
            None if directory => self.backing_directory.remove_dir(&rel).map_err(|e| to_errno(&e))?,
            None => self.backing_directory.remove_file(&rel).map_err(|e| to_errno(&e))?,
        }
        if let Some(local_only) = entry.local_only() {
            self.node_resolver.drop_node(&local_only.on_disk.handle);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
