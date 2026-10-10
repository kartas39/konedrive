//! Where a change from OneDrive finds something the user made here in its way: what is done
//! in the backing directory. What that is recorded as is decided in [`super::detections`]
//! ([`EntryKeptAside`], [`is_directory_of`]).
//!
//! A file or directory of OneDrive that comes to a name where a local-only entry is does not
//! hide it: the entry is moved aside, to a conflict copy's name, `name-<machine>.ext`, and is
//! uploaded from there. The downloaded copy of a OneDrive file that a program still writes
//! when a change takes the file's content, or the file, is the user's own in the same way:
//! unmarked, and moved aside ([`ConflictCopies::move_written_copy_aside`]). A directory OneDrive removed that still holds local-only entries
//! stays, as a local-only directory, and is made again in OneDrive; what became of the
//! directory on disk is a [`RemovedDirectoryOutcome`].
//!
//! [`ConflictCopies`] owns the machine's name, which a conflict copy is named with. What a
//! cycle's conflicts left is collected in a [`CycleConflicts`], which the cycle step records
//! through the outbox recorder; it records nothing itself. In a read-only folder the disk
//! is changed all the same and nothing is left to record: such a folder's outbox holds no
//! row, and what was kept is found by the scan for unrecorded entries once the folder takes
//! changes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_tree::outbox::{Detection, OutboxOp};

use crate::fuse::entry_place::EntryPlace;
use super::access_mode_switch::AccessMode;
use super::backing_directory::BackingDirectory;
use super::detections::{detect_local_only_entries, is_directory_of, EntryKeptAside, WriteState};
use super::directory_reference::DirectoryReference;
use super::directory_contents::DirectoryContents;
use super::local_only_entry::{collect_local_only_entries, decide_upload};
use super::upload_rules::UploadRules;
use crate::local::names::copy_name;

/// What the conflicts of one cycle left for the outbox, and what they did in the folder.
#[derive(Default)]
pub struct CycleConflicts {
    /// What the cycle recorded in the outbox for them: nothing in a read-only folder.
    pub detections: Vec<Detection>,
    pub ops: Vec<OutboxOp>,
    /// The user's own, moved aside.
    pub moved_aside: Vec<ConflictCopy>,
    /// The directories removed from OneDrive that stay here, by OneDrive id.
    pub kept_directories: Vec<String>,
}

/// One file or directory of the user's own that a cycle moved aside: the path it was at,
/// where something of OneDrive came, and the path its conflict copy is at now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictCopy {
    pub original: PathBuf,
    pub copy: PathBuf,
}

/// What became of the user's own at a path something of OneDrive comes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MoveAsideOutcome {
    /// Nothing of the user's own was there.
    NothingInTheWay,
    /// It is beside the path now, under a conflict copy's name.
    MovedAside,
    /// It could not be moved, and is still at the path.
    StillInTheWay,
}

/// What became of the directory on disk of a OneDrive directory that a cycle removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemovedDirectoryOutcome {
    /// The directory on disk is marked with another OneDrive id: it backs that other
    /// directory, and was left alone.
    LeftForOtherDirectory,
    /// It held nothing and was removed, or nothing was there.
    Removed,
    /// It holds the user's own: it stays, as a local-only directory — recorded to be made
    /// again in OneDrive in a read-write folder, not recorded in a read-only one.
    KeptAsLocalOnly,
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
    /// beside it, whatever the folder's access mode — nothing of the user's own is ever
    /// hidden or overwritten. In a folder that is read-write by `access_mode` it is recorded
    /// to be uploaded from there, dated `now`; in a read-only one nothing is recorded, and
    /// the scan for unrecorded entries finds it when the folder is switched to read-write.
    /// `write_state` is whether a program holds the entry open for writing: its row waits
    /// for that writer then. What became of the entry.
    pub(super) fn move_aside(&self, rel: &Path, parent_id: Option<&str>, write_state: WriteState, access_mode: AccessMode, now: i64, conflicts: &mut CycleConflicts) -> MoveAsideOutcome {
        let Some(on_disk) = self.backing_directory.find_unmarked_disk_entry(rel) else { return MoveAsideOutcome::NothingInTheWay };
        let (Some(name), Some(parent)) = (rel.file_name().and_then(|n| n.to_str()), rel.parent()) else { return MoveAsideOutcome::StillInTheWay };
        let directory = DirectoryReference { onedrive_id: parent_id.map(str::to_owned), path: parent.to_path_buf() };
        for n in 1..=100 {
            let copy = self.copy_name(name, n);
            let to = parent.join(&copy);
            if self.backing_directory.read_metadata(&to).is_some() || self.directory_contents.find_entry(&directory, &copy).is_some() {
                continue;
            }
            if let Err(e) = self.backing_directory.rename(rel, &to) {
                tracing::warn!("{} could not be kept beside what comes to its name from OneDrive: {e}", rel.display());
                return MoveAsideOutcome::StillInTheWay;
            }
            if access_mode == AccessMode::ReadWrite {
                let upload_decision = decide_upload(&self.backing_directory, &self.upload_rules, &directory.path, &copy);
                let kept = EntryKeptAside { on_disk, from: rel.to_path_buf(), to: EntryPlace::new(&directory, &copy), upload_decision, write_state }.detect(now);
                conflicts.ops.extend(kept.ops);
                conflicts.detections.extend(kept.detections);
            }
            conflicts.moved_aside.push(ConflictCopy { original: rel.to_path_buf(), copy: to });
            return MoveAsideOutcome::MovedAside;
        }
        MoveAsideOutcome::StillInTheWay
    }

    /// The downloaded copy of the OneDrive file `onedrive_id` at `path`, in the directory
    /// with the OneDrive id `parent_id`, is written by a program, and a change from OneDrive
    /// takes the file's content or the file itself: the copy is never removed under its
    /// writer. Its marks are taken off — what the program writes is the user's own from here
    /// on — and it is moved aside as any local-only entry in the way of a change is
    /// ([`move_aside`](Self::move_aside)), its row waiting for the writer. What became of
    /// it; nothing was in the way when the path holds no file marked with that id.
    pub(super) fn move_written_copy_aside(&self, path: &Path, onedrive_id: &str, parent_id: Option<&str>, access_mode: AccessMode, now: i64, conflicts: &mut CycleConflicts) -> MoveAsideOutcome {
        if !self.backing_directory.unmark_downloaded_copy(path, onedrive_id) {
            return MoveAsideOutcome::NothingInTheWay;
        }
        self.move_aside(path, parent_id, WriteState::OpenForWriting, access_mode, now, conflicts)
    }

    /// The directory with the OneDrive id `id` was removed from OneDrive: the directory at
    /// `rel` in the backing directory is removed when nothing is left in it. One that still
    /// holds local-only entries stays as a local-only directory, and, in a folder that is
    /// read-write by `access_mode`, is made again in OneDrive with what it holds. What
    /// became of the directory.
    pub(super) fn remove_directory_if_empty(&self, id: &str, rel: &Path, access_mode: AccessMode, now: i64, conflicts: &mut CycleConflicts) -> RemovedDirectoryOutcome {
        // A directory marked with another id backs that other directory, and is left alone.
        if !is_directory_of(self.backing_directory.read_onedrive_id(rel).as_deref(), id) {
            return RemovedDirectoryOutcome::LeftForOtherDirectory;
        }
        // The removal is the test of whether it holds anything: one that holds nothing goes.
        // One that was not removed because nothing is there was never made here.
        if self.backing_directory.remove_directory(rel).is_ok() || self.backing_directory.read_metadata(rel).is_none() {
            return RemovedDirectoryOutcome::Removed;
        }
        self.backing_directory.clear_onedrive_id(rel);
        if let (AccessMode::ReadWrite, Some(name), Some(parent)) = (access_mode, rel.file_name().and_then(|n| n.to_str()), rel.parent()) {
            // The directory it is in is found by its path: it may be removed and kept as
            // well.
            let directory = DirectoryReference { onedrive_id: None, path: parent.to_path_buf() };
            conflicts.detections.extend(detect_local_only_entries(&collect_local_only_entries(&self.backing_directory, &self.upload_rules, &directory, name), now));
            conflicts.kept_directories.push(id.to_owned());
        }
        RemovedDirectoryOutcome::KeptAsLocalOnly
    }
}

#[cfg(test)]
mod tests;
