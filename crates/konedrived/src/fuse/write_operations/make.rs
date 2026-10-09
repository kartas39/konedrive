//! A file or a directory a program makes in the folder: made in the backing directory,
//! where it is local only, and recorded for the outbox to send.

use std::fs::File;
use std::sync::Arc;

use fuser::Errno;
use konedrive_tree::Row;

use super::WriteOperations;
use crate::fuse::detections::{EntryCreated, LocalOnlyEntry};
use crate::fuse::directory::Directory;
use crate::fuse::directory_contents::DirectoryEntry;
use crate::fuse::local_only_entries::folder_place;
use crate::fuse::read::OpenFile;
use crate::fuse::shared::errno;

impl WriteOperations {
    /// Makes `name` in the directory node `parent`: a directory, or a file, which `open`
    /// leaves open for writing. The entry that was made, and the file when it is open.
    pub(in crate::fuse) fn make(&self, parent: u64, name: &str, directory: bool, open: bool) -> Result<(DirectoryEntry, Option<OpenFile>), Errno> {
        let parent_directory = self.check_make(parent, name, directory)?;
        let rel = parent_directory.rel.join(name);
        self.directory_contents.make_marked_directories(&parent_directory.rel).map_err(|e| errno(&e))?;
        let file: Option<File> = if directory {
            self.backing_directory.make_dir(&rel).map_err(|e| errno(&e))?;
            None
        } else {
            Some(self.backing_directory.make_file(&rel).map_err(|e| errno(&e))?)
        };
        let unmake = || drop(if directory { self.backing_directory.remove_dir(&rel) } else { self.backing_directory.remove_file(&rel) });
        // What cannot be looked at is not recorded, and is not left in the folder either.
        let Some(on_disk) = self.backing_directory.find_disk_entry(&rel) else {
            unmake();
            return Err(Errno::EIO);
        };
        let removed = if directory { None } else { self.directory_contents.find_file_removed_at(&rel) };
        let place = folder_place(&parent_directory, name);
        let entry = match &removed {
            Some(base) => {
                self.backing_directory.mark_downloaded_copy(&rel, &base.id).map_err(|e| errno(&e))?;
                let event = EntryCreated::ReplacingRemovedFile { place, file: on_disk, open, base: base.clone() };
                self.record_or_undo(event, format_args!("a new {}", rel.display()), unmake)?;
                // Read after it is recorded: the file is modified here from now on.
                self.directory_contents.describe(Row { name: name.to_owned(), ..base.clone() }, &rel)
            }
            None => {
                let local_only = LocalOnlyEntry { on_disk, place };
                if !self.stays_local(&parent_directory, name) {
                    let event = EntryCreated::Fresh { entry: local_only.clone(), open };
                    self.record_or_undo(event, format_args!("a new {}", rel.display()), unmake)?;
                }
                DirectoryEntry::from_local_only(local_only)
            }
        };
        let node_id = self.node_lookup.assign_node_id(&entry, parent);
        let opened = file.filter(|_| open).map(|file| {
            // What is written into a file just made is its content: the outbox waits for
            // its last writer already.
            self.open_writers.add_writer(node_id, true);
            OpenFile { ino: node_id, id: removed.map(|base| base.id), rel, copy: Some(Arc::new(file)), writes: true }
        });
        Ok((entry, opened))
    }

    /// The directory the node `parent` stands for, if `name` may be made in it as a
    /// directory (`directory`) or a file. Neither the daemon's own names nor a file
    /// manager's trash in the folder itself are made.
    fn check_make(&self, parent: u64, name: &str, directory: bool) -> Result<Directory, Errno> {
        self.access_mode.check_read_write()?;
        let parent_directory = self.node_lookup.find_directory(parent).ok_or(Errno::ENOENT)?;
        if self.upload_rules.daemon_owned(name) || (directory && parent == 1 && self.upload_rules.is_trash(name)) {
            return Err(Errno::EPERM);
        }
        if self.directory_contents.find_entry(&parent_directory, name).is_some() {
            return Err(Errno::EEXIST);
        }
        Ok(parent_directory)
    }
}

#[cfg(test)]
mod tests;
