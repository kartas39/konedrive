//! A file or a directory a program makes in the folder: made in the backing directory,
//! where it is local only, and recorded for the outbox to send.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use fuser::Errno;
use konedrive_tree::Row;

use super::make_check::{check_make, choose_made_entry_record, EntryToMake, MadeEntryRecord, MakePlaceContents};
use super::WriteOperations;
use crate::fuse::backing_directory::{DiskEntry, EntryKind};
use crate::fuse::detections::{EntryCreated, WriteState};
use crate::fuse::directory_contents::DirectoryEntry;
use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::entry_place::EntryPlace;
use crate::fuse::errno::to_errno;
use crate::fuse::local_only_entry::LocalOnlyEntry;
use crate::fuse::node_table::NodeId;
use crate::fuse::open_handles::Fh;

impl WriteOperations {
    /// Makes `name` in the directory node `parent`, as `entry_to_make` says. The node id of
    /// what was made, its entry, and the `fh` of the file when the program holds it open.
    ///
    /// In these steps: the place is checked ([`check_make`]); the entry is made in the
    /// backing directory ([`make_on_disk`](Self::make_on_disk)); what it is recorded as is
    /// chosen ([`choose_made_entry_record`]); it is described as it stands before it is
    /// recorded ([`describe_made_entry`](Self::describe_made_entry)), and given its node id;
    /// a file the program holds open is put into the open handles
    /// ([`add_writer_of_made_file`](Self::add_writer_of_made_file)); and it is recorded
    /// ([`record_made_entry`](Self::record_made_entry)). The node id and the open handle
    /// come before the record: whether the file is open for writing is then asked of the
    /// open handles, where it is true, and not worked out from what was asked for.
    pub(in crate::fuse) fn make(&self, parent: NodeId, name: &str, entry_to_make: EntryToMake) -> Result<(NodeId, DirectoryEntry, Option<Fh>), Errno> {
        self.access_mode_switch.check_read_write()?;
        let parent_directory = check_make(entry_to_make, self.find_make_place_contents(parent, name))?;
        let path = parent_directory.path.join(name);
        let (on_disk, file) = self.make_on_disk(&parent_directory, &path, entry_to_make.kind())?;
        // Asked once the directories down to the place are made and marked: a directory of
        // OneDrive is never ignored, whatever its name.
        let record = choose_made_entry_record(entry_to_make, self.directory_contents.find_file_removed_at(&path), self.decide_upload(&parent_directory, name));
        let local_only = LocalOnlyEntry { on_disk, place: EntryPlace::new(&parent_directory, name) };
        let made = self.describe_made_entry(&record, &local_only, &path)?;
        let node_id = self.node_resolver.assign_node_id(&made, parent);
        let opened = file.filter(|_| entry_to_make == EntryToMake::FileOpenedForWriting).map(|file| self.add_writer_of_made_file(node_id, file));
        let handle = local_only.on_disk.handle.clone();
        let undo = || {
            if let Some(fh) = opened {
                self.open_handles.close_file(fh);
            }
            self.node_resolver.drop_node(&handle);
            self.unmake_in_backing_directory(&path, entry_to_make.kind());
        };
        let entry = self.record_made_entry(record, local_only, made, self.find_write_state(node_id), &path, undo)?;
        Ok((node_id, entry, opened))
    }

    /// Makes the directories down to `parent_directory`, and the new entry at `path` in it:
    /// what the backing directory holds there now, and the file, open, when a file was made.
    /// What cannot be looked at is not recorded, and is not left in the folder either
    /// (`EIO`).
    fn make_on_disk(&self, parent_directory: &DirectoryReference, path: &Path, kind: EntryKind) -> Result<(DiskEntry, Option<File>), Errno> {
        self.directory_contents.make_marked_directories(&parent_directory.path).map_err(|e| to_errno(&e))?;
        let file = self.make_in_backing_directory(path, kind)?;
        match self.backing_directory.find_disk_entry(path) {
            Some(on_disk) => Ok((on_disk, file)),
            None => {
                self.unmake_in_backing_directory(path, kind);
                Err(Errno::EIO)
            }
        }
    }

    /// The entry that was made at `path`, as it stands before it is recorded as `record`:
    /// its node is what a writer is counted by. A file that replaces one the user removed
    /// is marked as that file's downloaded copy, and is that file's entry; anything else is
    /// local only.
    fn describe_made_entry(&self, record: &MadeEntryRecord, local_only: &LocalOnlyEntry, path: &Path) -> Result<DirectoryEntry, Errno> {
        match record {
            MadeEntryRecord::ReplacingRemovedFile(base) => {
                self.backing_directory.mark_downloaded_copy(path, &base.id).map_err(|e| to_errno(&e))?;
                Ok(self.directory_contents.describe(Row { name: local_only.place.name.clone(), ..base.clone() }, path))
            }
            MadeEntryRecord::Fresh | MadeEntryRecord::NotRecorded => Ok(DirectoryEntry::from_local_only(local_only.clone())),
        }
    }

    /// Puts the writer of the file just made, whose node is `node_id`, into the open handles: the `fh` the program holds `file` open by. What is
    /// written into a file just made is its content: the node is marked written, and the
    /// outbox waits for its last writer already.
    fn add_writer_of_made_file(&self, node_id: NodeId, file: File) -> Fh {
        let fh = self.open_handles.add_writer(node_id, Arc::new(file)).0;
        self.open_handles.mark_written(node_id);
        fh
    }

    /// Records the entry `made` at `path`, which is `local_only` on disk, as `record` says;
    /// `write_state` is whether a program holds it open for writing. A record that fails is
    /// taken back by `undo` and answered `EIO`. The entry as it stands once it is recorded:
    /// a file that replaces one the user removed is described again, modified here from now
    /// on, which its entry says only once the outbox holds its row.
    fn record_made_entry(&self, record: MadeEntryRecord, local_only: LocalOnlyEntry, made: DirectoryEntry, write_state: WriteState, path: &Path, undo: impl FnOnce()) -> Result<DirectoryEntry, Errno> {
        let what = format!("a new {}", path.display());
        match record {
            MadeEntryRecord::ReplacingRemovedFile(base) => {
                let event = EntryCreated::ReplacingRemovedFile { place: local_only.place, file: local_only.on_disk, write_state, base: base.clone() };
                self.record_or_undo(event, what, undo)?;
                Ok(self.directory_contents.describe(Row { name: made.name, ..base }, path))
            }
            MadeEntryRecord::Fresh => {
                self.record_or_undo(EntryCreated::Fresh { entry: local_only, write_state }, what, undo)?;
                Ok(made)
            }
            MadeEntryRecord::NotRecorded => Ok(made),
        }
    }

    /// What stands at `name` of the directory node `parent`, for the check of a make.
    fn find_make_place_contents(&self, parent: NodeId, name: &str) -> MakePlaceContents {
        let parent_directory = self.node_resolver.find_directory(parent);
        let name_is_taken = parent_directory.as_ref().is_some_and(|directory| self.directory_contents.find_entry(directory, name).is_some());
        MakePlaceContents {
            parent_directory,
            is_in_root: parent == NodeId::ROOT,
            name_is_daemon_owned: self.upload_rules.is_daemon_owned(name),
            name_is_trash: self.upload_rules.is_trash(name),
            name_is_taken,
        }
    }

    /// A new directory at `rel`, or a new file, which is handed back open.
    fn make_in_backing_directory(&self, rel: &Path, kind: EntryKind) -> Result<Option<File>, Errno> {
        match kind {
            EntryKind::Directory => self.backing_directory.make_directory(rel).map(|()| None),
            EntryKind::File => self.backing_directory.make_file(rel).map(Some),
        }
        .map_err(|e| to_errno(&e))
    }

    /// Takes back what [`make_in_backing_directory`](Self::make_in_backing_directory) made.
    fn unmake_in_backing_directory(&self, rel: &Path, kind: EntryKind) {
        let _ = match kind {
            EntryKind::Directory => self.backing_directory.remove_directory(rel),
            EntryKind::File => self.backing_directory.remove_file(rel),
        };
    }
}

#[cfg(test)]
mod tests;
