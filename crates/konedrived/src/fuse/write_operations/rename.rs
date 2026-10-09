//! A file or a directory a program renames or moves inside the folder, in four steps: the
//! check gives a [`RenamePlan`]; the backing directory is renamed ([`BackingDirectoryMove`]); the
//! rename is recorded as a [`EntryRenamed`] event; the node takes its new place.
//!
//! An item stands at its new place at once, and is moved in OneDrive by the outbox. A local
//! object is uploaded from its new place, or no longer uploaded when that place stays on this
//! computer; an item cannot be moved to such a place (`EXDEV`): a program that copies it
//! there and removes it here does what that means. A local file renamed over an item's file
//! is that item's new content, and its node the item's.

use std::path::PathBuf;

use fuser::Errno;

use super::WriteOperations;
use crate::fuse::detections::{DisplacedEntry, MovedEntry, EntryRenamed};
use crate::fuse::local_only_entries::{collect_local_only_entries, folder_place, local_object};
use crate::fuse::shared::errno;
use crate::fuse::directory_contents::{Directory, Entry};

/// A rename the check let through: what moves where, and over what.
struct RenamePlan {
    /// What moves, its node, and whether a program holds it open for writing.
    source: Entry,
    node_id: u64,
    open: bool,
    /// What stands at the new place.
    target: Option<Entry>,
    from: PathBuf,
    to_dir: Directory,
    to_node: u64,
    to_name: String,
    /// The new place is one local objects are uploaded from.
    uploaded: bool,
}

impl RenamePlan {
    fn to(&self) -> PathBuf {
        self.to_dir.rel.join(&self.to_name)
    }

    /// The item whose file a local file is saved over, by rename.
    fn saved_over(&self) -> Option<&str> {
        match (&self.source, &self.target) {
            (Entry::Local(local), Some(Entry::Item(over))) if !local.object.meta.is_dir() => Some(&over.id),
            _ => None,
        }
    }
}

/// What the backing directory held at the old place, which was renamed with the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackingDirectoryMove {
    /// Something stood there, and is at the new place now.
    Moved,
    /// Nothing: an item's file that is not downloaded, a folder no download made yet.
    NothingThere,
}

impl WriteOperations {
    /// Renames `name` of the directory node `parent` to `new_name` of `new_parent`, over what
    /// stands there unless `keep` says to refuse that.
    pub(in crate::fuse) fn rename(&self, parent: u64, name: &str, new_parent: u64, new_name: &str, keep: bool) -> Result<(), Errno> {
        let Some(plan) = self.check_rename(parent, name, new_parent, new_name, keep)? else { return Ok(()) };
        let backing = self.rename_backing(&plan)?;
        let event = self.renamed(&plan, backing);
        let to = plan.to();
        let undo = || {
            if backing == BackingDirectoryMove::Moved {
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
        self.check_writable()?;
        let from_dir = self.node_lookup.find_directory(parent).ok_or(Errno::ENOENT)?;
        let to_dir = self.node_lookup.find_directory(new_parent).ok_or(Errno::ENOENT)?;
        if self.rules.daemon_owned(new_name) {
            return Err(Errno::EPERM);
        }
        let source = self.directory_contents.find_entry(&from_dir, name).ok_or(Errno::ENOENT)?;
        let target = self.directory_contents.find_entry(&to_dir, new_name);
        let (from, to) = (from_dir.rel.join(name), to_dir.rel.join(new_name));
        if from == to {
            return Ok(None);
        }
        if let Some(target) = &target {
            let holds = || !self.directory_contents.list_entries(&Directory { id: target.id().map(str::to_owned), rel: to.clone() }).is_empty();
            match (source.is_dir(), target.is_dir()) {
                _ if keep => return Err(Errno::EEXIST),
                (false, true) => return Err(Errno::EISDIR),
                (true, false) => return Err(Errno::ENOTDIR),
                (true, true) if holds() => return Err(Errno::ENOTEMPTY),
                _ => {}
            }
        }
        if source.id().is_some() && self.rules.in_local_dir(&to_dir.rel, |at| self.backing_directory.read_onedrive_id(at).is_some()) {
            return Err(Errno::EXDEV);
        }
        let uploaded = !self.stays_local(&to_dir, new_name);
        let node_id = self.node_lookup.assign_node_id(&source, parent);
        let open = self.open_writers.is_open(node_id);
        Ok(Some(RenamePlan { source, node_id, open, target, from, to_dir, to_node: new_parent, to_name: new_name.to_owned(), uploaded }))
    }

    /// The backing directory first: whatever the item or the local object has there moves,
    /// and what stood at the new place goes.
    fn rename_backing(&self, plan: &RenamePlan) -> Result<BackingDirectoryMove, Errno> {
        let to = plan.to();
        self.directory_contents.make_marked_directories(&plan.to_dir.rel).map_err(|e| errno(&e))?;
        let displaced = self.backing_directory.stat(&to);
        let backing = if self.backing_directory.stat(&plan.from).is_some() { BackingDirectoryMove::Moved } else { BackingDirectoryMove::NothingThere };
        // A file that moves over a file replaces it in the rename itself.
        if let Some(meta) = displaced.filter(|m| m.is_dir() || backing == BackingDirectoryMove::NothingThere) {
            let _ = if meta.is_dir() { self.backing_directory.remove_dir(&to) } else { self.backing_directory.remove_file(&to) };
        }
        if backing == BackingDirectoryMove::Moved {
            self.backing_directory.rename(&plan.from, &to).map_err(|e| errno(&e))?;
        }
        Ok(backing)
    }

    /// What the rename is recorded as.
    fn renamed(&self, plan: &RenamePlan, backing: BackingDirectoryMove) -> EntryRenamed {
        let to = folder_place(&plan.to_dir, &plan.to_name);
        if let (Some(over), Entry::Local(local)) = (plan.saved_over(), &plan.source) {
            return EntryRenamed::SavedOverOneDriveFile { file: local_object(&local.object), open: plan.open, over: over.to_owned(), to };
        }
        let entry = match &plan.source {
            Entry::Item(row) => {
                let object = (backing == BackingDirectoryMove::Moved).then(|| self.backing_directory.find_disk_entry(&plan.to())).flatten();
                MovedEntry::InOneDrive { id: row.id.clone(), dir: plan.source.is_dir(), object: object.map(|o| o.handle) }
            }
            Entry::Local(local) if local.object.meta.is_dir() => {
                let uploaded = if plan.uploaded { collect_local_only_entries(&self.backing_directory, &self.rules, &plan.to_dir, &plan.to_name) } else { Vec::new() };
                MovedEntry::LocalOnlyDirectory { handle: local.object.handle.clone(), uploaded }
            }
            Entry::Local(local) => MovedEntry::LocalOnlyFile { file: local_object(&local.object), open: plan.open, uploaded: plan.uploaded },
        };
        let displaced = plan.target.as_ref().map(|target| match target {
            Entry::Item(row) => DisplacedEntry::InOneDrive(row.id.clone()),
            Entry::Local(local) => DisplacedEntry::LocalOnly(local.object.handle.clone()),
        });
        EntryRenamed::Moved { entry, from: plan.from.clone(), to, displaced }
    }

    /// The node takes its new place. A file saved over an item's file is that item's from
    /// here on — its downloaded copy, and its node, so that whoever looks the name up gets
    /// the node the new content is read and written through. A local object that was
    /// displaced is gone.
    fn move_node(&self, plan: &RenamePlan) {
        let taken = match (plan.saved_over(), &plan.source) {
            (Some(id), Entry::Local(local)) => {
                let _ = self.backing_directory.mark_downloaded_copy(&plan.to(), id);
                Some((&local.object.handle, id))
            }
            _ => None,
        };
        self.node_lookup.move_node(plan.node_id, plan.to_node, &plan.to_name, taken);
        if let Some(Entry::Local(displaced)) = &plan.target {
            self.node_lookup.forget(&displaced.object.handle);
        }
    }
}

#[cfg(test)]
mod tests;
