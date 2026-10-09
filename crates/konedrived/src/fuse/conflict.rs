//! Where a change from OneDrive meets something the user made here.
//!
//! An item that comes to a name where a file or directory without an item stands does not
//! hide it: the user's own is kept beside it under a copy's name, `name-<machine>.ext`, and
//! is uploaded from there. A folder OneDrive removed that still holds something of the
//! user's stays, as a folder of the user's own, and is made again in OneDrive.

use std::path::{Path, PathBuf};

use konedrive_tree::outbox::{Detection, OutboxOp};

use super::changes::{rebase, Place};
use super::view::Dir;
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
        let Some(object) = self.under.object(rel) else { return };
        if self.under.carried(rel).is_some() || !(object.meta.is_file() || object.meta.is_dir()) {
            return;
        }
        let dir = Dir { id: above.map(str::to_owned), rel: parent.to_path_buf() };
        for n in 1..=100 {
            let copy = copy_name(name, &self.writing.machine_name, n);
            let to = parent.join(&copy);
            if self.under.stat(&to).is_some() || self.view.entry(&dir, &copy).is_some() {
                continue;
            }
            if let Err(e) = self.under.rename(rel, &to) {
                tracing::warn!("{} could not be kept beside the item that comes to its name: {e}", rel.display());
                return;
            }
            if object.meta.is_dir() {
                met.ops.push(rebase(rel, &to));
            }
            if !self.stays_local(&dir, &copy) {
                met.detections.push(Place { dir: &dir, name: &copy }.new(&object, false));
            }
            met.copied.push((rel.to_path_buf(), to));
            return;
        }
    }

    /// The folder item `id` is gone from OneDrive: its directory at `rel` goes when nothing
    /// is left in it. One that still holds something of the user's stays as the user's own,
    /// and is made again in OneDrive with what it holds.
    pub(super) fn folder_gone(&self, id: &str, rel: &Path, sends: bool, met: &mut Met) {
        if self.under.carried(rel).is_some_and(|carried| carried != id) || self.under.remove_dir(rel).is_ok() || self.under.stat(rel).is_none() {
            return;
        }
        self.under.carry_nothing(rel);
        let (Some(name), Some(parent)) = (rel.file_name().and_then(|n| n.to_str()), rel.parent()) else { return };
        if sends {
            // Its directory above is found by its place: it may be gone and kept as well.
            let above = Dir { id: None, rel: parent.to_path_buf() };
            self.below(&above, name, &mut met.detections);
            met.kept.push(id.to_owned());
        }
    }
}
