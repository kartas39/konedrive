//! Where a change from OneDrive meets something the user made here.
//!
//! An item that comes to a name where a file or directory without an item stands does not
//! hide it: the user's own is kept beside it under a copy's name, `name-<machine>.ext`, and
//! is uploaded from there. A folder OneDrive removed that still holds something of the
//! user's stays, as a folder of the user's own, and is made again in OneDrive.

use std::fs;
use std::path::{Path, PathBuf};

use konedrive_fs::placeholder::XATTR_ITEM_ID;
use konedrive_tree::outbox::{Detection, OutboxOp};

use super::changes::{rebase, Place};
use super::local::{carried, Dir};
use super::shared::Shared;
use crate::local::names::copy_name;

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

impl Shared {
    /// An item comes to `rel`, in the directory of item `above`: what stands there without
    /// an item is renamed to a copy's name beside it.
    pub(super) fn out_of_the_way(&self, rel: &Path, above: Option<&str>, met: &mut Met) {
        let (Some(name), Some(parent)) = (rel.file_name().and_then(|n| n.to_str()), rel.parent()) else { return };
        let path = self.under(rel);
        let Ok(meta) = fs::symlink_metadata(&path) else { return };
        if carried(&path).is_some() || !(meta.is_file() || meta.is_dir()) {
            return;
        }
        let dir = Dir { id: above.map(str::to_owned), rel: parent.to_path_buf() };
        for n in 1..=100 {
            let copy = copy_name(name, &self.writing.machine_name, n);
            let to = parent.join(&copy);
            if fs::symlink_metadata(self.under(&to)).is_ok() || self.entry(&dir, &copy).is_some() {
                continue;
            }
            if let Err(e) = fs::rename(&path, self.under(&to)) {
                tracing::warn!("{} could not be kept beside the item that comes to its name: {e}", rel.display());
                return;
            }
            if meta.is_dir() {
                met.ops.push(rebase(rel, &to));
            }
            if !self.stays_local(&dir, &copy) {
                met.detections.push(Place { dir: &dir, name: &copy }.new(&meta, false));
            }
            met.copied.push((rel.to_path_buf(), to));
            return;
        }
    }

    /// The folder item `id` is gone from OneDrive: its directory at `rel` goes when nothing
    /// is left in it. One that still holds something of the user's stays as the user's own,
    /// and is made again in OneDrive with what it holds.
    pub(super) fn folder_gone(&self, id: &str, rel: &Path, sends: bool, met: &mut Met) {
        let path = self.under(rel);
        if carried(&path).is_some_and(|carried| carried != id) || fs::remove_dir(&path).is_ok() || fs::symlink_metadata(&path).is_err() {
            return;
        }
        let _ = xattr::remove(&path, XATTR_ITEM_ID);
        let (Some(name), Some(parent)) = (rel.file_name().and_then(|n| n.to_str()), rel.parent()) else { return };
        if sends {
            // Its directory above is found by its place: it may be gone and kept as well.
            let above = Dir { id: None, rel: parent.to_path_buf() };
            self.below(&above, name, &mut met.detections);
            met.kept.push(id.to_owned());
        }
    }
}
