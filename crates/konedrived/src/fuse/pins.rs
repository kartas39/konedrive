//! "Always keep on this device": the pins.
//!
//! A pin is a row of the store ([`konedrive_tree::TreeStore::pins`]): a file that is not
//! downloaded has nothing on disk to carry one. [`Pins`] holds the OneDrive ids that carry a
//! pin of their own, as the store has them: read from the store when the mount starts, and
//! changed only together with the store, which it writes. A pin on a directory keeps
//! everything below it, so what keeps a file is found up its chain of parents
//! ([`Pins::find_pinning_row`]), which [`DirectoryContents`] tells.
//!
//! It starts nothing: what a pin keeps is downloaded by
//! [`PinnedFileDownloader`](super::pinned_file_downloader::PinnedFileDownloader).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use konedrive_tree::{Row, Store, TreeError};

use super::directory_contents::DirectoryContents;

/// The pins: the OneDrive ids that carry a pin of their own.
pub(super) struct Pins {
    pinned_ids: Mutex<HashSet<String>>,
    /// The store's one writer: a pin is put on and taken off there first.
    store: Store,
    directory_contents: Arc<DirectoryContents>,
}

impl Pins {
    /// The OneDrive ids `store` has a pin on.
    pub(super) async fn load_pinned_ids(store: &Store) -> Result<Vec<String>, TreeError> {
        store.call(|s| s.pins()).await
    }

    /// The pins of a folder, `pinned_ids` being what its store has
    /// ([`load_pinned_ids`](Self::load_pinned_ids)).
    pub(super) fn new(pinned_ids: Vec<String>, store: Store, directory_contents: Arc<DirectoryContents>) -> Self {
        Pins { pinned_ids: Mutex::new(pinned_ids.into_iter().collect()), store, directory_contents }
    }

    /// Puts the pin on the OneDrive id `id`, or takes it off: in the store, and then here.
    /// Whether that changed anything.
    pub(super) async fn set_pin(&self, id: &str, pinned: bool) -> Result<bool, TreeError> {
        let in_store = id.to_owned();
        self.store.call(move |s| s.set_pin(&in_store, pinned)).await?;
        let mut pinned_ids = crate::panic::lock(&self.pinned_ids);
        Ok(if pinned { pinned_ids.insert(id.to_owned()) } else { pinned_ids.remove(id) })
    }

    /// Whether the OneDrive id `id` carries a pin of its own.
    pub(super) fn is_pinned(&self, id: &str) -> bool {
        crate::panic::lock(&self.pinned_ids).contains(id)
    }

    /// How many OneDrive ids carry a pin of their own.
    pub(super) fn count(&self) -> u64 {
        crate::panic::lock(&self.pinned_ids).len() as u64
    }

    /// The OneDrive ids that carry a pin of their own.
    pub(super) fn list_pinned_ids(&self) -> Vec<String> {
        crate::panic::lock(&self.pinned_ids).iter().cloned().collect()
    }

    /// What keeps `row` on this device: itself or a directory above it, whichever carries a
    /// pin. The pins are those of the moment it is asked: the chain of parents is walked
    /// through the directory contents with the pins' own lock let go.
    pub(super) fn find_pinning_row(&self, row: &Row) -> Option<Row> {
        let pinned_ids = crate::panic::lock(&self.pinned_ids).clone();
        let mut at = Some(row.clone());
        while let Some(row) = at {
            if pinned_ids.contains(&row.id) {
                return Some(row);
            }
            at = self.directory_contents.find_parent_row(&row);
        }
        None
    }

    /// A cycle removed `removed_ids` from the folder: what is gone keeps nothing on this
    /// device, so the pins among them are taken off, here and then in the store. Called
    /// where no async runtime runs: on the thread of the cycle that was committed.
    pub(super) fn unpin_removed(&self, removed_ids: Vec<String>) {
        let unpinned: Vec<String> = {
            let mut pinned_ids = crate::panic::lock(&self.pinned_ids);
            removed_ids.into_iter().filter(|id| pinned_ids.remove(id)).collect()
        };
        if !unpinned.is_empty() {
            let _ = self.store.call_blocking(move |s| unpinned.iter().try_for_each(|id| s.set_pin(id, false)));
        }
    }
}

#[cfg(test)]
mod tests;
