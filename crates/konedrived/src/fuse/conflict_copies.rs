//! Where a change from OneDrive meets something the user made here: what is done in the
//! backing directory. What that is recorded as is decided in [`super::detections`]
//! ([`EntryKeptAside`], [`is_directory_of`]).
//!
//! An item that comes to a name where a local object stands does not hide it: the object is
//! kept beside it under a copy's name, `name-<machine>.ext`, and is uploaded from there. A
//! folder OneDrive removed that still holds local objects stays, as a local directory, and
//! is made again in OneDrive.

use std::path::{Path, PathBuf};

use konedrive_tree::outbox::{Detection, OutboxOp};

use super::detections::{create_local_only_entries, is_directory_of, EntryKeptAside};
use super::local_only_entries::{collect_local_only_entries, folder_place, local_object};
use super::backing_directory::BackingDirectory;
use super::upload_rules::UploadRules;
use super::directory_contents::{Directory, DirectoryContents};

/// What the meetings of one cycle left for the outbox, and what they did in the folder.
#[derive(Default)]
pub(super) struct CycleConflicts {
    pub(super) detections: Vec<Detection>,
    pub(super) ops: Vec<OutboxOp>,
    /// The user's own, moved out of an item's way: where it stood, and where it is now.
    pub(super) copied: Vec<(PathBuf, PathBuf)>,
    /// The folders gone from OneDrive that stay here, by item.
    pub(super) kept: Vec<String>,
}

/// The parts a meeting uses.
pub(super) struct ConflictCopies<'a> {
    pub(super) backing_directory: &'a BackingDirectory,
    pub(super) directory_contents: &'a DirectoryContents,
    pub(super) rules: &'a UploadRules,
    /// The time the rows are dated by.
    pub(super) now: i64,
}

impl ConflictCopies<'_> {
    /// An item comes to `rel`, in the directory of item `above`: the local object that
    /// stands there is renamed to a copy's name beside it.
    pub(super) fn move_aside(&self, rel: &Path, above: Option<&str>, met: &mut CycleConflicts) {
        let (Some(name), Some(parent)) = (rel.file_name().and_then(|n| n.to_str()), rel.parent()) else { return };
        let Some(object) = self.backing_directory.find_disk_entry(rel) else { return };
        if self.backing_directory.read_onedrive_id(rel).is_some() || !(object.meta.is_file() || object.meta.is_dir()) {
            return;
        }
        let dir = Directory { id: above.map(str::to_owned), rel: parent.to_path_buf() };
        for n in 1..=100 {
            let copy = self.rules.copy_name(name, n);
            let to = parent.join(&copy);
            if self.backing_directory.stat(&to).is_some() || self.directory_contents.find_entry(&dir, &copy).is_some() {
                continue;
            }
            if let Err(e) = self.backing_directory.rename(rel, &to) {
                tracing::warn!("{} could not be kept beside the item that comes to its name: {e}", rel.display());
                return;
            }
            let uploaded = !self.rules.stays_local(&dir.rel, &copy, |at| self.backing_directory.read_onedrive_id(at).is_some());
            let kept = EntryKeptAside { object: local_object(&object), from: rel.to_path_buf(), to: folder_place(&dir, &copy), uploaded }.detect(self.now);
            met.ops.extend(kept.ops);
            met.detections.extend(kept.detections);
            met.copied.push((rel.to_path_buf(), to));
            return;
        }
    }

    /// The folder item `id` is gone from OneDrive: its directory at `rel` goes when nothing
    /// is left in it. One that still holds local objects stays as a local directory, and,
    /// when the folder `uploads`, is made again in OneDrive with what it holds.
    pub(super) fn keep_removed_directory(&self, id: &str, rel: &Path, uploads: bool, met: &mut CycleConflicts) {
        if !is_directory_of(self.backing_directory.read_onedrive_id(rel).as_deref(), id) || self.backing_directory.remove_dir(rel).is_ok() || self.backing_directory.stat(rel).is_none() {
            return;
        }
        self.backing_directory.clear_onedrive_id(rel);
        let (Some(name), Some(parent)) = (rel.file_name().and_then(|n| n.to_str()), rel.parent()) else { return };
        if uploads {
            // Its directory above is found by its place: it may be gone and kept as well.
            let above = Directory { id: None, rel: parent.to_path_buf() };
            met.detections.extend(create_local_only_entries(&collect_local_only_entries(self.backing_directory, self.rules, &above, name), self.now));
            met.kept.push(id.to_owned());
        }
    }
}
