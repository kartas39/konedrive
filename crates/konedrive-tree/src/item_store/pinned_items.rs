//! The files a pin holds for ("always keep on this device"), by local item id. A pin is kept
//! by the item id of OneDrive of the file or directory it was put on
//! ([`TreeStore::pins`](crate::TreeStore::pins)): it follows the binding, and one on a
//! directory holds for every file the folder shows in it and below it. What is here reads
//! the pins against the rows; nothing here writes one.

use super::item_row::{find_is_pinned, list_pinned_files_to_download};
use super::ItemStore;
use crate::model::{Item, LocalItemId};
use crate::TreeError;

impl ItemStore<'_> {
    /// Whether a pin holds for `item`: its own, or that of a directory above it in the
    /// folder. `false` for an item the folder does not show and for an id with no row.
    pub fn is_pinned(&self, item: LocalItemId) -> Result<bool, TreeError> {
        find_is_pinned(&self.store.conn, item)
    }

    /// The pinned files that agree on everything and whose content is not on disk: the
    /// two sides have the file at one place with one content, so that it is among neither
    /// the unsent items nor those OneDrive changed, and all there is to do is to download
    /// it. At most `limit` of them by local item id, after the item `after` when one is
    /// given. Found from the pins down: every row the folder shows below a pinned directory
    /// is read.
    pub fn list_pinned_files_to_download(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<Item>, TreeError> {
        list_pinned_files_to_download(&self.store.conn, after, limit)
    }
}

#[cfg(test)]
mod tests;
