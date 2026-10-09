//! A file or a directory a program makes in the folder: made in the backing directory,
//! where it is local only, and recorded for the outbox to send.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use fuser::Errno;
use konedrive_tree::Row;

use super::make_check::{check_make, choose_made_entry_record, EntryToMake, MadeEntryRecord, MakePlace};
use super::WriteOperations;
use crate::fuse::backing_directory::EntryKind;
use crate::fuse::detections::EntryCreated;
use crate::fuse::directory_contents::DirectoryEntry;
use crate::fuse::entry_place::EntryPlace;
use crate::fuse::errno::to_errno;
use crate::fuse::local_only_entry::LocalOnlyEntry;
use crate::fuse::node_table::NodeId;
use crate::fuse::open_handles::Fh;

impl WriteOperations {
    /// Makes `name` in the directory node `parent`, as `entry_to_make` says. The entry that
    /// was made, and the `fh` of the file when the program holds it open.
    pub(in crate::fuse) fn make(&self, parent: NodeId, name: &str, entry_to_make: EntryToMake) -> Result<(DirectoryEntry, Option<Fh>), Errno> {
        self.access_mode_switch.check_read_write()?;
        let parent_directory = check_make(entry_to_make, self.find_make_place(parent, name))?;
        let rel = parent_directory.path.join(name);
        self.directory_contents.make_marked_directories(&parent_directory.path).map_err(|e| to_errno(&e))?;
        let file = self.make_in_backing_directory(&rel, entry_to_make.kind())?;
        let unmake = || self.unmake_in_backing_directory(&rel, entry_to_make.kind());
        // What cannot be looked at is not recorded, and is not left in the folder either.
        let Some(on_disk) = self.backing_directory.find_disk_entry(&rel) else {
            unmake();
            return Err(Errno::EIO);
        };
        // Asked once the directories down to the place are made and marked: a directory of
        // OneDrive is never ignored, whatever its name.
        let record = choose_made_entry_record(entry_to_make, self.directory_contents.find_file_removed_at(&rel), self.find_upload_status(&parent_directory, name));
        let (place, write_state) = (EntryPlace::new(&parent_directory, name), entry_to_make.write_state());
        let (entry, onedrive_id) = match record {
            MadeEntryRecord::ReplacingRemovedFile(base) => {
                self.backing_directory.mark_downloaded_copy(&rel, &base.id).map_err(|e| to_errno(&e))?;
                let event = EntryCreated::ReplacingRemovedFile { place, file: on_disk, write_state, base: base.clone() };
                self.record_or_undo(event, format_args!("a new {}", rel.display()), unmake)?;
                // Read after it is recorded: the file is modified here from now on.
                (self.directory_contents.describe(Row { name: name.to_owned(), ..base.clone() }, &rel), Some(base.id))
            }
            MadeEntryRecord::Fresh => {
                let local_only = LocalOnlyEntry { on_disk, place };
                self.record_or_undo(EntryCreated::Fresh { entry: local_only.clone(), write_state }, format_args!("a new {}", rel.display()), unmake)?;
                (DirectoryEntry::from_local_only(local_only), None)
            }
            MadeEntryRecord::NotRecorded => (DirectoryEntry::from_local_only(LocalOnlyEntry { on_disk, place }), None),
        };
        let node_id = self.node_resolver.assign_node_id(&entry, parent);
        let opened = file.filter(|_| entry_to_make == EntryToMake::FileOpenedForWriting).map(|file| {
            // What is written into a file just made is its content: the outbox waits for
            // its last writer already.
            self.open_handles.add_writer(node_id, onedrive_id, Arc::new(file), true).fh
        });
        Ok((entry, opened))
    }

    /// What stands at `name` of the directory node `parent`, for the check of a make.
    fn find_make_place(&self, parent: NodeId, name: &str) -> MakePlace {
        let parent_directory = self.node_resolver.find_directory(parent);
        let name_is_taken = parent_directory.as_ref().is_some_and(|directory| self.directory_contents.find_entry(directory, name).is_some());
        MakePlace {
            parent_directory,
            is_in_root: parent == NodeId::ROOT,
            name_is_daemon_owned: self.upload_rules.daemon_owned(name),
            name_is_trash: self.upload_rules.is_trash(name),
            name_is_taken,
        }
    }

    /// A new directory at `rel`, or a new file, which is handed back open.
    fn make_in_backing_directory(&self, rel: &Path, kind: EntryKind) -> Result<Option<File>, Errno> {
        match kind {
            EntryKind::Directory => self.backing_directory.make_dir(rel).map(|()| None),
            EntryKind::File => self.backing_directory.make_file(rel).map(Some),
        }
        .map_err(|e| to_errno(&e))
    }

    /// Takes back what [`make_in_backing_directory`](Self::make_in_backing_directory) made.
    fn unmake_in_backing_directory(&self, rel: &Path, kind: EntryKind) {
        let _ = match kind {
            EntryKind::Directory => self.backing_directory.remove_dir(rel),
            EntryKind::File => self.backing_directory.remove_file(rel),
        };
    }
}

#[cfg(test)]
mod tests;
