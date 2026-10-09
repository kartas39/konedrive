//! Where a change from OneDrive meets something the user made here: what is done in the
//! backing directory. What that is recorded as is decided in [`super::detections`]
//! ([`KeptCopy`], [`is_folder_of`]).
//!
//! An item that comes to a name where a local object stands does not hide it: the object is
//! kept beside it under a copy's name, `name-<machine>.ext`, and is uploaded from there. A
//! folder OneDrive removed that still holds local objects stays, as a local directory, and
//! is made again in OneDrive.

use std::path::{Path, PathBuf};

use konedrive_tree::outbox::{Detection, OutboxOp};

use super::detections::{create_entries, is_folder_of, KeptCopy};
use super::local_entries::{collect_local_entries, folder_place, local_object};
use super::backing_directory::BackingDirectory;
use super::upload_rules::UploadRules;
use super::directory_contents::{Directory, DirectoryContents};

/// What the meetings of one cycle left for the outbox, and what they did in the folder.
#[derive(Default)]
pub(super) struct Met {
    pub(super) detections: Vec<Detection>,
    pub(super) ops: Vec<OutboxOp>,
    /// The user's own, moved out of an item's way: where it stood, and where it is now.
    pub(super) copied: Vec<(PathBuf, PathBuf)>,
    /// The folders gone from OneDrive that stay here, by item.
    pub(super) kept: Vec<String>,
}

/// The parts a meeting uses.
pub(super) struct Meeting<'a> {
    pub(super) under: &'a BackingDirectory,
    pub(super) view: &'a DirectoryContents,
    pub(super) rules: &'a UploadRules,
    /// The time the rows are dated by.
    pub(super) now: i64,
}

impl Meeting<'_> {
    /// An item comes to `rel`, in the directory of item `above`: the local object that
    /// stands there is renamed to a copy's name beside it.
    pub(super) fn keep_beside_item(&self, rel: &Path, above: Option<&str>, met: &mut Met) {
        let (Some(name), Some(parent)) = (rel.file_name().and_then(|n| n.to_str()), rel.parent()) else { return };
        let Some(object) = self.under.object(rel) else { return };
        if self.under.carried(rel).is_some() || !(object.meta.is_file() || object.meta.is_dir()) {
            return;
        }
        let dir = Directory { id: above.map(str::to_owned), rel: parent.to_path_buf() };
        for n in 1..=100 {
            let copy = self.rules.copy_name(name, n);
            let to = parent.join(&copy);
            if self.under.stat(&to).is_some() || self.view.entry(&dir, &copy).is_some() {
                continue;
            }
            if let Err(e) = self.under.rename(rel, &to) {
                tracing::warn!("{} could not be kept beside the item that comes to its name: {e}", rel.display());
                return;
            }
            let uploaded = !self.rules.stays_local(&dir.rel, &copy, |at| self.under.carried(at).is_some());
            let kept = KeptCopy { object: local_object(&object), from: rel.to_path_buf(), to: folder_place(&dir, &copy), uploaded }.detect(self.now);
            met.ops.extend(kept.ops);
            met.detections.extend(kept.detections);
            met.copied.push((rel.to_path_buf(), to));
            return;
        }
    }

    /// The folder item `id` is gone from OneDrive: its directory at `rel` goes when nothing
    /// is left in it. One that still holds local objects stays as a local directory, and,
    /// when the folder `uploads`, is made again in OneDrive with what it holds.
    pub(super) fn keep_gone_folder(&self, id: &str, rel: &Path, uploads: bool, met: &mut Met) {
        if !is_folder_of(self.under.carried(rel).as_deref(), id) || self.under.remove_dir(rel).is_ok() || self.under.stat(rel).is_none() {
            return;
        }
        self.under.carry_nothing(rel);
        let (Some(name), Some(parent)) = (rel.file_name().and_then(|n| n.to_str()), rel.parent()) else { return };
        if uploads {
            // Its directory above is found by its place: it may be gone and kept as well.
            let above = Directory { id: None, rel: parent.to_path_buf() };
            met.detections.extend(create_entries(&collect_local_entries(self.under, self.rules, &above, name), self.now));
            met.kept.push(id.to_owned());
        }
    }
}
