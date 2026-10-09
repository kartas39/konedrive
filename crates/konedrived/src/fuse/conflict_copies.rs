//! Where a change from OneDrive finds something the user made here in its way: what is done
//! in the backing directory. What that is recorded as is decided in [`super::detections`]
//! ([`EntryKeptAside`], [`is_directory_of`]).
//!
//! A file or directory of OneDrive that comes to a name where a local-only entry is does not
//! hide it: the entry is moved aside, to a conflict copy's name, `name-<machine>.ext`, and is
//! uploaded from there. A directory OneDrive removed that still holds local-only entries
//! stays, as a local-only directory, and is made again in OneDrive.
//!
//! [`ConflictCopies`] owns the machine's name, which a conflict copy is named with. What a
//! cycle's conflicts left is collected in a [`CycleConflicts`], which the cycle records.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_tree::outbox::{Detection, OutboxOp};

use crate::fuse::entry_place::EntryPlace;
use super::access_mode_switch::AccessMode;
use super::backing_directory::BackingDirectory;
use super::detections::{detect_local_only_entries, is_directory_of, EntryKeptAside};
use super::directory_reference::DirectoryReference;
use super::directory_contents::DirectoryContents;
use super::local_only_entry::collect_local_only_entries;
use super::upload_rules::UploadRules;
use crate::local::names::copy_name;

/// What the conflicts of one cycle left for the outbox, and what they did in the folder.
#[derive(Default)]
pub struct CycleConflicts {
    /// What the cycle recorded in the outbox for them.
    pub detections: Vec<Detection>,
    pub ops: Vec<OutboxOp>,
    /// The user's own, moved aside: the path each was at, and the path it is at now.
    pub moved_aside: Vec<(PathBuf, PathBuf)>,
    /// The directories removed from OneDrive that stay here, by OneDrive id.
    pub kept_directories: Vec<String>,
}

pub(super) struct ConflictCopies {
    backing_directory: Arc<BackingDirectory>,
    directory_contents: Arc<DirectoryContents>,
    upload_rules: Arc<UploadRules>,
    /// The machine's name, for a conflict copy's: `name-<machine>.ext`.
    machine_name: String,
}

impl ConflictCopies {
    pub(super) fn new(backing_directory: Arc<BackingDirectory>, directory_contents: Arc<DirectoryContents>, upload_rules: Arc<UploadRules>, machine_name: String) -> Self {
        ConflictCopies { backing_directory, directory_contents, upload_rules, machine_name }
    }

    /// The name the user's own `name` is kept under beside what comes to it from OneDrive,
    /// at the `n`th try.
    fn copy_name(&self, name: &str, n: u32) -> String {
        copy_name(name, &self.machine_name, n)
    }

    /// Something of OneDrive comes to `rel`, in the directory with the OneDrive id
    /// `parent_id`: the local-only entry that is there is renamed to a conflict copy's name
    /// beside it. `now` is the time the rows are dated by.
    pub(super) fn move_aside(&self, rel: &Path, parent_id: Option<&str>, now: i64, conflicts: &mut CycleConflicts) {
        let (Some(name), Some(parent)) = (rel.file_name().and_then(|n| n.to_str()), rel.parent()) else { return };
        let Some(on_disk) = self.backing_directory.find_unmarked_disk_entry(rel) else { return };
        let directory = DirectoryReference { onedrive_id: parent_id.map(str::to_owned), path: parent.to_path_buf() };
        for n in 1..=100 {
            let copy = self.copy_name(name, n);
            let to = parent.join(&copy);
            if self.backing_directory.read_metadata(&to).is_some() || self.directory_contents.find_entry(&directory, &copy).is_some() {
                continue;
            }
            if let Err(e) = self.backing_directory.rename(rel, &to) {
                tracing::warn!("{} could not be kept beside what comes to its name from OneDrive: {e}", rel.display());
                return;
            }
            let uploaded = !self.upload_rules.is_ignored(&directory.path, &copy, |at| self.backing_directory.read_onedrive_id(at).is_some());
            let kept = EntryKeptAside { on_disk, from: rel.to_path_buf(), to: EntryPlace::new(&directory, &copy), uploaded }.detect(now);
            conflicts.ops.extend(kept.ops);
            conflicts.detections.extend(kept.detections);
            conflicts.moved_aside.push((rel.to_path_buf(), to));
            return;
        }
    }

    /// The directory with the OneDrive id `id` was removed from OneDrive: the directory at
    /// `rel` in the backing directory is removed when nothing is left in it. One that still
    /// holds local-only entries stays as a local-only directory, and, in a folder that is
    /// read-write by `access_mode`, is made again in OneDrive with what it holds.
    pub(super) fn keep_removed_directory(&self, id: &str, rel: &Path, access_mode: AccessMode, now: i64, conflicts: &mut CycleConflicts) {
        if !is_directory_of(self.backing_directory.read_onedrive_id(rel).as_deref(), id) || self.backing_directory.remove_dir(rel).is_ok() || self.backing_directory.read_metadata(rel).is_none() {
            return;
        }
        self.backing_directory.clear_onedrive_id(rel);
        let (Some(name), Some(parent)) = (rel.file_name().and_then(|n| n.to_str()), rel.parent()) else { return };
        if access_mode == AccessMode::ReadWrite {
            // The directory it is in is found by its path: it may be removed and kept as
            // well.
            let directory = DirectoryReference { onedrive_id: None, path: parent.to_path_buf() };
            conflicts.detections.extend(detect_local_only_entries(&collect_local_only_entries(&self.backing_directory, &self.upload_rules, &directory, name), now));
            conflicts.kept_directories.push(id.to_owned());
        }
    }
}

#[cfg(test)]
mod tests;
