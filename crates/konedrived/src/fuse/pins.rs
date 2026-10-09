//! "Always keep on this device": the pins.
//!
//! A pin is a row of the store ([`konedrive_tree::TreeStore::pins`]): a file that is not
//! downloaded has nothing on disk to carry one. [`Pins`] holds the OneDrive ids that carry a
//! pin of their own, as the store has them: read from the store when the mount starts, and
//! changed only together with the store. A pin on a directory keeps everything below it, so
//! what keeps a file is found up its chain of parents ([`Pins::pinned_by`]), which
//! [`DirectoryContents`] tells.
//!
//! What a pin keeps is downloaded by [`download_pinned_files`].

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use konedrive_graph::pool::Class;
use konedrive_tree::{Row, Store, TreeError};

use super::directory_contents::{DirectoryContents, FileSyncState};
use super::file_download::FileDownloader;

/// The pins: the OneDrive ids that carry a pin of their own.
pub(super) struct Pins {
    pinned_ids: Mutex<HashSet<String>>,
    /// The store's one writer: a pin is put on and taken off there first.
    store: Store,
    directory_contents: Arc<DirectoryContents>,
    /// Where the pins of what a cycle removed are taken off the store.
    runtime: tokio::runtime::Handle,
}

impl Pins {
    /// The OneDrive ids `store` has a pin on.
    pub(super) async fn load_pinned_ids(store: &Store) -> Result<Vec<String>, TreeError> {
        store.call(|s| s.pins()).await
    }

    /// The pins of a folder, `pinned_ids` being what its store has
    /// ([`load_pinned_ids`](Self::load_pinned_ids)).
    pub(super) fn new(pinned_ids: Vec<String>, store: Store, directory_contents: Arc<DirectoryContents>, runtime: tokio::runtime::Handle) -> Self {
        Pins { pinned_ids: Mutex::new(pinned_ids.into_iter().collect()), store, directory_contents, runtime }
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
    /// pin.
    pub(super) fn pinned_by(&self, row: &Row) -> Option<Row> {
        let pinned_ids = crate::panic::lock(&self.pinned_ids);
        let mut at = Some(row.clone());
        while let Some(row) = at {
            if pinned_ids.contains(&row.id) {
                return Some(row);
            }
            at = self.directory_contents.parent(&row);
        }
        None
    }

    /// A cycle removed `removed_ids` from the folder: what is gone keeps nothing on this
    /// device, so the pins among them are taken off, here and then in the store.
    pub(super) fn unpin_removed(&self, removed_ids: Vec<String>) {
        let unpinned: Vec<String> = {
            let mut pinned_ids = crate::panic::lock(&self.pinned_ids);
            removed_ids.into_iter().filter(|id| pinned_ids.remove(id)).collect()
        };
        if !unpinned.is_empty() {
            let store = self.store.clone();
            self.runtime.spawn(async move {
                let _ = store.call(move |s| unpinned.iter().try_for_each(|id| s.set_pin(id, false))).await;
            });
        }
    }
}

/// Downloads every pinned file that is not downloaded: run when the mount starts, when a pin
/// is put on and after every cycle, so that a new file in a pinned directory and a pinned
/// file whose content changed are downloaded by it.
pub(super) fn download_pinned_files(pins: &Arc<Pins>, directory_contents: &Arc<DirectoryContents>, file_downloader: &Arc<FileDownloader>, runtime: &tokio::runtime::Handle) {
    let (pins, directory_contents, file_downloader) = (Arc::clone(pins), Arc::clone(directory_contents), Arc::clone(file_downloader));
    // Off the caller's thread: a cycle calls this while it holds the folder's tree lock, and
    // a large pinned directory is a long walk.
    runtime.spawn_blocking(move || {
        for id in pins.list_pinned_ids() {
            let Some(row) = directory_contents.row(&id) else { continue };
            for (file, rel) in directory_contents.files_below(&row) {
                if let FileSyncState::RemoteOnly(file) = file {
                    file_downloader.download(file, rel, Class::Download, None);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests;
