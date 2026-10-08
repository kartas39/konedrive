//! The downloaded copies in the folder under the mount: which file is an item's, and its state.

use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use konedrive_fs::placeholder::{self, State, XATTR_ITEM_ID};
use konedrive_tree::Row;

use super::shared::Shared;

/// Removes the downloaded copy at `path` if it is the copy of `id` and no other file. Its size
/// on disk, when it went.
pub(super) fn remove_copy(path: &Path, id: &str) -> Option<u64> {
    let file = File::open(path).ok()?;
    if placeholder::read_item_id(&file).ok().flatten().as_deref() != Some(id) {
        return None;
    }
    let bytes = file.metadata().map_or(0, |m| m.blocks() * 512);
    fs::remove_file(path).ok().map(|()| bytes)
}

impl Shared {
    /// The downloaded copy of item `id` at `rel`, if the folder under the mount holds it: a
    /// regular file that carries the item's id. Any other file there is not the item's — one
    /// the user put in the folder while nothing was mounted, or the copy of another item —
    /// and is never shown as its content.
    pub(super) fn copy(&self, rel: &Path, id: &str) -> Option<fs::Metadata> {
        let path = self.under(rel);
        let meta = fs::symlink_metadata(&path).ok().filter(fs::Metadata::is_file)?;
        let carried = xattr::get(&path, XATTR_ITEM_ID).ok().flatten()?;
        (carried == id.as_bytes()).then_some(meta)
    }

    pub(super) fn has_copy(&self, rel: &Path, id: &str) -> bool {
        self.copy(rel, id).is_some()
    }

    pub(super) fn state_of(&self, row: &Row, rel: &Path) -> State {
        if crate::panic::lock(&self.fills).contains_key(&row.id) {
            State::Hydrating
        } else if self.has_copy(rel, &row.id) {
            State::Hydrated
        } else {
            State::OnlineOnly
        }
    }
}
