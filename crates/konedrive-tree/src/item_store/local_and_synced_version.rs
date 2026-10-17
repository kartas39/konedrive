//! An item of OneDrive that the folder ceases to show without it being removed here: its
//! local version and what the two sides agreed on are taken, and its binding and what
//! OneDrive has of it stay. Written here by what learns that the item has no place in the
//! folder any more and that nobody removed it.

use super::item_row::{find_item, write_item};
use super::{check_empty, refuse, ItemStore};
use crate::model::{ContentPresence, LocalItemId};
use crate::TreeError;

impl ItemStore<'_> {
    /// The folder does not show the item of OneDrive `item` any more, and it was not
    /// removed here: its local version, the place and the content the two sides last agreed
    /// on, and what it had before a writer's open are cleared, and its content is not on
    /// disk. The binding stays, and all that is OneDrive's of the item: the row says an item
    /// OneDrive has and the folder does not show, with nothing unsent. A change of its
    /// place or content that was not sent is lost with that.
    ///
    /// Refused for the root, for an id with no row, for an item that has no binding (its
    /// row would say nothing), for an item the folder does not show, and for a directory
    /// the folder shows something in: what is in it goes first, deepest first.
    pub fn clear_local_and_synced_version(&mut self, item: LocalItemId) -> Result<(), TreeError> {
        self.write(|tx| {
            if item == LocalItemId::ROOT {
                return Err(refuse("the root of the folder is always shown".to_owned()));
            }
            let mut row = find_item(tx, item)?.ok_or_else(|| refuse(format!("item {item} has no row")))?;
            if row.onedrive_item.is_none() {
                return Err(refuse(format!("item {item} has no binding: without a local version its row would say nothing")));
            }
            if row.local.is_none() {
                return Err(refuse(format!("the folder does not show item {item}: it has no local version to lose")));
            }
            check_empty(tx, item)?;
            row.local = None;
            row.synced_place = None;
            row.synced_content = None;
            row.content_presence = ContentPresence::NotOnDisk;
            row.content_before_write = None;
            write_item(tx, &row)
        })
    }
}

#[cfg(test)]
mod tests;
