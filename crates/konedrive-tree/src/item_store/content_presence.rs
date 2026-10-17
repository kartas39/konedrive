//! Whether the whole content of a file the folder shows is in the backing directory, as its
//! row says it: written here by what learns that the content came to be on disk, or ceased
//! to be, with no change of the item's local version.

use super::item_row::{find_item, set_content_presence};
use super::{refuse, ItemStore};
use crate::model::{ContentPresence, ItemKind, LocalItemId};
use crate::TreeError;

impl ItemStore<'_> {
    /// The whole content of the file `item` is in the backing directory from now on, or is
    /// not: nothing else of the row changes. Refused for an id with no row, for an item the
    /// folder does not show and for a directory, none of which has a content to be on disk.
    pub fn set_content_presence(&mut self, item: LocalItemId, content_presence: ContentPresence) -> Result<(), TreeError> {
        self.write(|tx| {
            let row = find_item(tx, item)?.ok_or_else(|| refuse(format!("item {item} has no row")))?;
            if row.local.is_none() {
                return Err(refuse(format!("the folder does not show item {item}: it has no content to be on disk")));
            }
            if row.kind != ItemKind::File {
                return Err(refuse(format!("item {item} is no file: it has no content to be on disk")));
            }
            set_content_presence(tx, item, content_presence)
        })
    }
}

#[cfg(test)]
mod tests;
