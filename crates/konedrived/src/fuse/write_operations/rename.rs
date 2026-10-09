//! A file or a directory a program renames or moves inside the folder, in five steps: what
//! stands at the two places is looked up; the plan gives a [`RenamePlan`], or the errno of a
//! refusal ([`plan_rename`]); the backing directory is renamed ([`BackingDirectoryMove`]);
//! the rename is recorded as the event [`describe_rename`] gives; the node takes its new
//! place. Every decision is in [`rename_plan`](super::rename_plan): here are the lookups,
//! the disk and the record.
//!
//! What is in OneDrive is at its new place at once, and is moved in OneDrive by the outbox.
//! A local-only entry is uploaded from its new place, or no longer uploaded when that place
//! is ignored; what is in OneDrive cannot be moved to such a place (`EXDEV`). A local-only
//! file renamed over a OneDrive file is that file's new content, and its node that file's.

use fuser::Errno;

use super::rename_plan::{choose_removed_at_new_place, describe_rename, plan_rename, BackingDirectoryMove, RenameDecision, RenameMode, RenamePlaces, RenamePlan};
use super::WriteOperations;
use crate::fuse::backing_directory::EntryKind;
use crate::fuse::directory_contents::DirectoryEntry;
use crate::fuse::errno::to_errno;
use crate::fuse::local_only_entry::{collect_local_only_entries, is_in_ignored_directory, LocalOnlyEntry};
use crate::fuse::node_table::NodeId;
use crate::fuse::upload_rules::UploadStatus;

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
        let to = plan.to.path();
        // What the record is decided from, gathered now that the backing directory moved.
        let handle_at_new_place = (moved == BackingDirectoryMove::Moved).then(|| self.backing_directory.find_file_handle(&to)).flatten();
        let event = describe_rename(&plan, write_state, handle_at_new_place, self.collect_uploaded_entries(&plan));
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
        let to_is_in_ignored_directory = to_directory.as_ref().is_some_and(|directory| is_in_ignored_directory(&self.backing_directory, &self.upload_rules, &directory.path));
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
    /// at the new place is removed ([`choose_removed_at_new_place`]).
    fn move_in_backing_directory(&self, plan: &RenamePlan) -> Result<BackingDirectoryMove, Errno> {
        let to = plan.to.path();
        self.directory_contents.make_marked_directories(&plan.to.directory.path).map_err(|e| to_errno(&e))?;
        let held_at_new_place = self.backing_directory.read_metadata(&to).map(|metadata| if metadata.is_dir() { EntryKind::Directory } else { EntryKind::File });
        let moved = if self.backing_directory.read_metadata(&plan.from).is_some() { BackingDirectoryMove::Moved } else { BackingDirectoryMove::NothingThere };
        let _ = match choose_removed_at_new_place(held_at_new_place, moved) {
            Some(EntryKind::Directory) => self.backing_directory.remove_dir(&to),
            Some(EntryKind::File) => self.backing_directory.remove_file(&to),
            None => Ok(()),
        };
        if moved == BackingDirectoryMove::Moved {
            self.backing_directory.rename(&plan.from, &to).map_err(|e| to_errno(&e))?;
        }
        Ok(moved)
    }

    /// What a walk of the local-only directory that moved finds at its new place, to be
    /// uploaded from there; nothing for anything else that moved.
    fn collect_uploaded_entries(&self, plan: &RenamePlan) -> Vec<LocalOnlyEntry> {
        match plan.uploads_moved_directory() {
            true => collect_local_only_entries(&self.backing_directory, &self.upload_rules, &plan.to.directory, &plan.to.name),
            false => Vec::new(),
        }
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
