//! A file or a directory a program renames or moves inside the folder, in five steps: what
//! stands at the two places is looked up; the plan gives a [`RenamePlan`], or the errno of a
//! refusal ([`plan_rename`]); the backing directory is renamed ([`BackingDirectoryMove`]);
//! the rename is recorded as an [`EntryRenamed`] event; the node takes its new place.
//!
//! What is in OneDrive is at its new place at once, and is moved in OneDrive by the outbox.
//! A local-only entry is uploaded from its new place, or no longer uploaded when that place
//! is ignored; what is in OneDrive cannot be moved to such a place (`EXDEV`). A local-only
//! file renamed over a OneDrive file is that file's new content, and its node that file's.

use fuser::Errno;

use super::rename_plan::{plan_rename, RenameDecision, RenameMode, RenamePlaces, RenamePlan};
use super::WriteOperations;
use crate::fuse::backing_directory::EntryKind;
use crate::fuse::detections::{DisplacedEntry, EntryRenamed, MovedEntry, WriteState};
use crate::fuse::directory_contents::{DirectoryEntry, DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::errno::to_errno;
use crate::fuse::local_only_entry::collect_local_only_entries;
use crate::fuse::node_table::NodeId;
use crate::fuse::upload_rules::UploadStatus;

/// What the backing directory held at the old place, which was renamed with the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackingDirectoryMove {
    /// Something was there, and is at the new place now.
    Moved,
    /// Nothing: a OneDrive file that is not downloaded, a directory no download made yet.
    NothingThere,
}

impl WriteOperations {
    /// Renames `name` of the directory node `parent` to `new_name` of `new_parent`; `mode`
    /// says what becomes of what is there.
    pub(in crate::fuse) fn rename(&self, parent: NodeId, name: &str, new_parent: NodeId, new_name: &str, mode: RenameMode) -> Result<(), Errno> {
        self.access_mode_switch.check_read_write()?;
        let plan = match plan_rename(mode, self.find_rename_places(parent, name, new_parent, new_name))? {
            RenameDecision::Rename(plan) => plan,
            RenameDecision::NothingToRename => return Ok(()),
        };
        let node_id = self.node_resolver.assign_node_id(&plan.moved_entry, parent);
        let write_state = self.find_write_state(node_id);
        let moved = self.move_in_backing_directory(&plan)?;
        let event = self.describe_rename(&plan, write_state, moved);
        let to = plan.to.path();
        let undo = || {
            if moved == BackingDirectoryMove::Moved {
                let _ = self.backing_directory.rename(&to, &plan.from);
            }
        };
        self.record_or_undo(event, format_args!("the move of {} to {}", plan.from.display(), to.display()), undo)?;
        self.move_node(&plan, node_id, new_parent);
        Ok(())
    }

    /// What stands at the two places, for the plan of a rename.
    fn find_rename_places(&self, parent: NodeId, name: &str, new_parent: NodeId, new_name: &str) -> RenamePlaces {
        let from_directory = self.node_resolver.find_directory(parent);
        let to_directory = self.node_resolver.find_directory(new_parent);
        let moved_entry = from_directory.as_ref().and_then(|directory| self.directory_contents.find_entry(directory, name));
        let target = to_directory.as_ref().and_then(|directory| self.directory_contents.find_entry(directory, new_name));
        let target_holds_entries = to_directory.as_ref().zip(target.as_ref()).is_some_and(|(directory, target)| self.holds_entries(target, &directory.path.join(new_name)));
        let in_onedrive = |at: &std::path::Path| self.backing_directory.read_onedrive_id(at).is_some();
        let to_is_in_ignored_directory = to_directory.as_ref().is_some_and(|directory| self.upload_rules.is_in_ignored_directory(&directory.path, in_onedrive));
        let to_upload_status = to_directory.as_ref().map_or(UploadStatus::Uploaded, |directory| self.find_upload_status(directory, new_name));
        RenamePlaces {
            from_directory,
            to_directory,
            name: name.to_owned(),
            new_name: new_name.to_owned(),
            new_name_is_daemon_owned: self.upload_rules.daemon_owned(new_name),
            moved_entry,
            target,
            target_holds_entries,
            to_is_in_ignored_directory,
            to_upload_status,
        }
    }

    /// The backing directory first: whatever it holds at the old place moves, and what was
    /// at the new place is removed.
    fn move_in_backing_directory(&self, plan: &RenamePlan) -> Result<BackingDirectoryMove, Errno> {
        let to = plan.to.path();
        self.directory_contents.make_marked_directories(&plan.to.directory.path).map_err(|e| to_errno(&e))?;
        let displaced = self.backing_directory.read_metadata(&to);
        let moved = if self.backing_directory.read_metadata(&plan.from).is_some() { BackingDirectoryMove::Moved } else { BackingDirectoryMove::NothingThere };
        // A file that moves over a file replaces it in the rename itself.
        if let Some(metadata) = displaced.filter(|m| m.is_dir() || moved == BackingDirectoryMove::NothingThere) {
            let _ = if metadata.is_dir() { self.backing_directory.remove_dir(&to) } else { self.backing_directory.remove_file(&to) };
        }
        if moved == BackingDirectoryMove::Moved {
            self.backing_directory.rename(&plan.from, &to).map_err(|e| to_errno(&e))?;
        }
        Ok(moved)
    }

    /// What the rename is recorded as: the plan, with what the backing directory holds at
    /// the new place now.
    fn describe_rename(&self, plan: &RenamePlan, write_state: WriteState, moved: BackingDirectoryMove) -> EntryRenamed {
        if let Some((file, over)) = plan.saved_over() {
            return EntryRenamed::SavedOverOneDriveFile { file: file.on_disk.clone(), write_state, over: over.to_owned(), to: plan.to.clone() };
        }
        let entry = match (&plan.moved_entry.state, plan.to_upload_status) {
            (EntryState::File(FileSyncState::RemoteOnly(row) | FileSyncState::Synced(row, _) | FileSyncState::ModifiedHere(row, _)), _) => self.describe_moved_onedrive_entry(&row.id, EntryKind::File, plan, moved),
            (EntryState::Directory(DirectorySyncState::InOneDrive(row)), _) => self.describe_moved_onedrive_entry(&row.id, EntryKind::Directory, plan, moved),
            (EntryState::File(FileSyncState::LocalOnly(file)), UploadStatus::Uploaded) => MovedEntry::LocalOnlyFile { file: file.on_disk.clone(), write_state },
            (EntryState::File(FileSyncState::LocalOnly(file)), UploadStatus::Ignored) => MovedEntry::IgnoredLocalOnlyFile { file: file.on_disk.clone() },
            (EntryState::Directory(DirectorySyncState::LocalOnly(directory)), UploadStatus::Ignored) => MovedEntry::IgnoredLocalOnlyDirectory { handle: directory.on_disk.handle.clone() },
            (EntryState::Directory(DirectorySyncState::LocalOnly(directory)), UploadStatus::Uploaded) => {
                let uploaded_entries = collect_local_only_entries(&self.backing_directory, &self.upload_rules, &plan.to.directory, &plan.to.name);
                // A walk that finds nothing to upload — the directory is marked with a
                // OneDrive id the store does not have yet — is recorded as a directory that
                // is ignored from now on.
                match uploaded_entries.is_empty() {
                    true => MovedEntry::IgnoredLocalOnlyDirectory { handle: directory.on_disk.handle.clone() },
                    false => MovedEntry::LocalOnlyDirectory { uploaded_entries },
                }
            }
        };
        let displaced = plan.target.as_ref().map(|target| match &target.state {
            EntryState::File(FileSyncState::RemoteOnly(row) | FileSyncState::Synced(row, _) | FileSyncState::ModifiedHere(row, _)) | EntryState::Directory(DirectorySyncState::InOneDrive(row)) => DisplacedEntry::InOneDrive(row.id.clone()),
            EntryState::File(FileSyncState::LocalOnly(local_only)) | EntryState::Directory(DirectorySyncState::LocalOnly(local_only)) => DisplacedEntry::LocalOnly(local_only.on_disk.handle.clone()),
        });
        EntryRenamed::Moved { entry, from: plan.from.clone(), to: plan.to.clone(), displaced }
    }

    /// The file or directory of OneDrive `id` that moved, with the handle of what the
    /// backing directory holds for it at the new place.
    fn describe_moved_onedrive_entry(&self, id: &str, kind: EntryKind, plan: &RenamePlan, moved: BackingDirectoryMove) -> MovedEntry {
        let handle = (moved == BackingDirectoryMove::Moved).then(|| self.backing_directory.find_file_handle(&plan.to.path())).flatten();
        MovedEntry::InOneDrive { id: id.to_owned(), kind, handle }
    }

    /// The node `node_id` takes its new place, in the directory node `new_parent`. A
    /// local-only file saved over a OneDrive file is that file's from here on — its
    /// downloaded copy, and its node, so that whoever looks the name up gets the node the
    /// new content is read and written through. A local-only entry that was displaced is
    /// gone.
    fn move_node(&self, plan: &RenamePlan, node_id: NodeId, new_parent: NodeId) {
        let taken = plan.saved_over().map(|(file, over)| {
            let _ = self.backing_directory.mark_downloaded_copy(&plan.to.path(), over);
            (&file.on_disk.handle, over)
        });
        self.node_resolver.move_node(node_id, new_parent, &plan.to.name, taken);
        if let Some(displaced) = plan.target.as_ref().and_then(DirectoryEntry::local_only) {
            self.node_resolver.drop_node(&displaced.on_disk.handle);
        }
    }
}

#[cfg(test)]
mod tests;
