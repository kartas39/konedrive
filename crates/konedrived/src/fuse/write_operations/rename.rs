//! A file or a directory a program renames or moves inside the folder, in four steps: the
//! check gives a [`RenamePlan`]; the backing directory is renamed
//! ([`BackingDirectoryMove`]); the rename is recorded as an [`EntryRenamed`] event; the node
//! takes its new place.
//!
//! What is in OneDrive is at its new place at once, and is moved in OneDrive by the outbox.
//! A local-only entry is uploaded from its new place, or no longer uploaded when that place
//! stays on this computer; what is in OneDrive cannot be moved to such a place (`EXDEV`): a
//! program that copies it there and removes it here does what that means. A local-only file
//! renamed over a OneDrive file is that file's new content, and its node that file's.

use std::path::PathBuf;

use fuser::Errno;

use super::WriteOperations;
use crate::fuse::detections::{DisplacedEntry, EntryRenamed, LocalOnlyEntry, MovedEntry};
use crate::fuse::directory::Directory;
use crate::fuse::directory_contents::{DirectoryEntry, DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::local_only_entries::{collect_local_only_entries, folder_place};
use crate::fuse::shared::errno;

/// A rename the check let through: what moves where, and over what.
struct RenamePlan {
    /// What moves, its node id, and whether a program holds it open for writing.
    source: DirectoryEntry,
    node_id: u64,
    open: bool,
    /// What is at the new place.
    target: Option<DirectoryEntry>,
    from: PathBuf,
    to_directory: Directory,
    to_node: u64,
    to_name: String,
    /// The new place is one local-only entries are uploaded from.
    uploaded: bool,
}

impl RenamePlan {
    fn to(&self) -> PathBuf {
        self.to_directory.rel.join(&self.to_name)
    }

    /// The local-only file that is saved, by rename, over a OneDrive file, and that file's
    /// OneDrive id.
    fn saved_over(&self) -> Option<(&LocalOnlyEntry, &str)> {
        match (&self.source.state, self.target.as_ref().and_then(DirectoryEntry::onedrive_id)) {
            (EntryState::File(FileSyncState::LocalOnly(file)), Some(over)) => Some((file, over)),
            _ => None,
        }
    }
}

/// What the backing directory held at the old place, which was renamed with the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackingDirectoryMove {
    /// Something was there, and is at the new place now.
    Moved,
    /// Nothing: a OneDrive file that is not downloaded, a directory no download made yet.
    NothingThere,
}

impl WriteOperations {
    /// Renames `name` of the directory node `parent` to `new_name` of `new_parent`, over what
    /// is there unless `keep` says to refuse that.
    pub(in crate::fuse) fn rename(&self, parent: u64, name: &str, new_parent: u64, new_name: &str, keep: bool) -> Result<(), Errno> {
        let Some(plan) = self.check_rename(parent, name, new_parent, new_name, keep)? else { return Ok(()) };
        let moved = self.move_in_backing_directory(&plan)?;
        let event = self.describe_rename(&plan, moved);
        let to = plan.to();
        let undo = || {
            if moved == BackingDirectoryMove::Moved {
                let _ = self.backing_directory.rename(&to, &plan.from);
            }
        };
        self.record_or_undo(event, format_args!("the move of {} to {}", plan.from.display(), to.display()), undo)?;
        self.move_node(&plan);
        Ok(())
    }

    /// What the rename is, or the errno it is refused with. `None`: nothing moves, the name
    /// is its own.
    fn check_rename(&self, parent: u64, name: &str, new_parent: u64, new_name: &str, keep: bool) -> Result<Option<RenamePlan>, Errno> {
        self.access_mode.check_read_write()?;
        let from_directory = self.node_lookup.find_directory(parent).ok_or(Errno::ENOENT)?;
        let to_directory = self.node_lookup.find_directory(new_parent).ok_or(Errno::ENOENT)?;
        if self.upload_rules.daemon_owned(new_name) {
            return Err(Errno::EPERM);
        }
        let source = self.directory_contents.find_entry(&from_directory, name).ok_or(Errno::ENOENT)?;
        let target = self.directory_contents.find_entry(&to_directory, new_name);
        let (from, to) = (from_directory.rel.join(name), to_directory.rel.join(new_name));
        if from == to {
            return Ok(None);
        }
        if let Some(target) = &target {
            let holds = || !self.directory_contents.list_entry_sources(&Directory { id: target.onedrive_id().map(str::to_owned), rel: to.clone() }).is_empty();
            match (&source.state, &target.state) {
                _ if keep => return Err(Errno::EEXIST),
                (EntryState::File(_), EntryState::Directory(_)) => return Err(Errno::EISDIR),
                (EntryState::Directory(_), EntryState::File(_)) => return Err(Errno::ENOTDIR),
                (EntryState::Directory(_), EntryState::Directory(_)) if holds() => return Err(Errno::ENOTEMPTY),
                _ => {}
            }
        }
        if source.onedrive_id().is_some() && self.upload_rules.in_local_dir(&to_directory.rel, |at| self.backing_directory.read_onedrive_id(at).is_some()) {
            return Err(Errno::EXDEV);
        }
        let uploaded = !self.stays_local(&to_directory, new_name);
        let node_id = self.node_lookup.assign_node_id(&source, parent);
        let open = self.open_writers.is_open(node_id);
        Ok(Some(RenamePlan { source, node_id, open, target, from, to_directory, to_node: new_parent, to_name: new_name.to_owned(), uploaded }))
    }

    /// The backing directory first: whatever it holds at the old place moves, and what was
    /// at the new place is removed.
    fn move_in_backing_directory(&self, plan: &RenamePlan) -> Result<BackingDirectoryMove, Errno> {
        let to = plan.to();
        self.directory_contents.make_marked_directories(&plan.to_directory.rel).map_err(|e| errno(&e))?;
        let displaced = self.backing_directory.stat(&to);
        let moved = if self.backing_directory.stat(&plan.from).is_some() { BackingDirectoryMove::Moved } else { BackingDirectoryMove::NothingThere };
        // A file that moves over a file replaces it in the rename itself.
        if let Some(metadata) = displaced.filter(|m| m.is_dir() || moved == BackingDirectoryMove::NothingThere) {
            let _ = if metadata.is_dir() { self.backing_directory.remove_dir(&to) } else { self.backing_directory.remove_file(&to) };
        }
        if moved == BackingDirectoryMove::Moved {
            self.backing_directory.rename(&plan.from, &to).map_err(|e| errno(&e))?;
        }
        Ok(moved)
    }

    /// What the rename is recorded as.
    fn describe_rename(&self, plan: &RenamePlan, moved: BackingDirectoryMove) -> EntryRenamed {
        let to = folder_place(&plan.to_directory, &plan.to_name);
        if let Some((file, over)) = plan.saved_over() {
            return EntryRenamed::SavedOverOneDriveFile { file: file.on_disk.clone(), open: plan.open, over: over.to_owned(), to };
        }
        let entry = match &plan.source.state {
            EntryState::File(FileSyncState::RemoteOnly(row) | FileSyncState::Synced(row, _) | FileSyncState::ModifiedHere(row, _)) | EntryState::Directory(DirectorySyncState::Synced(row)) => {
                let handle = (moved == BackingDirectoryMove::Moved).then(|| self.backing_directory.find_file_handle(&plan.to())).flatten();
                MovedEntry::InOneDrive { id: row.id.clone(), directory: matches!(plan.source.state, EntryState::Directory(_)), handle }
            }
            EntryState::Directory(DirectorySyncState::LocalOnly(directory)) => {
                let uploaded = if plan.uploaded { collect_local_only_entries(&self.backing_directory, &self.upload_rules, &plan.to_directory, &plan.to_name) } else { Vec::new() };
                MovedEntry::LocalOnlyDirectory { handle: directory.on_disk.handle.clone(), uploaded }
            }
            EntryState::File(FileSyncState::LocalOnly(file)) => MovedEntry::LocalOnlyFile { file: file.on_disk.clone(), open: plan.open, uploaded: plan.uploaded },
        };
        let displaced = plan.target.as_ref().map(|target| match &target.state {
            EntryState::File(FileSyncState::RemoteOnly(row) | FileSyncState::Synced(row, _) | FileSyncState::ModifiedHere(row, _)) | EntryState::Directory(DirectorySyncState::Synced(row)) => DisplacedEntry::InOneDrive(row.id.clone()),
            EntryState::File(FileSyncState::LocalOnly(local_only)) | EntryState::Directory(DirectorySyncState::LocalOnly(local_only)) => DisplacedEntry::LocalOnly(local_only.on_disk.handle.clone()),
        });
        EntryRenamed::Moved { entry, from: plan.from.clone(), to, displaced }
    }

    /// The node takes its new place. A local-only file saved over a OneDrive file is that
    /// file's from here on — its downloaded copy, and its node, so that whoever looks the
    /// name up gets the node the new content is read and written through. A local-only entry
    /// that was displaced is gone.
    fn move_node(&self, plan: &RenamePlan) {
        let taken = plan.saved_over().map(|(file, over)| {
            let _ = self.backing_directory.mark_downloaded_copy(&plan.to(), over);
            (&file.on_disk.handle, over)
        });
        self.node_lookup.move_node(plan.node_id, plan.to_node, &plan.to_name, taken);
        if let Some(displaced) = plan.target.as_ref().and_then(DirectoryEntry::local_only) {
            self.node_lookup.forget(&displaced.on_disk.handle);
        }
    }
}

#[cfg(test)]
mod tests;
