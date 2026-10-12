//! The pins, asked and set by item, and taken off with the binding they follow.
//!
//! **State owned:** the table `pins`, which holds OneDrive item ids (design §5: a pin
//! follows the binding). Written here alone once the switch is made: set and taken off by
//! [`ItemStore::set_item_pin`], and taken off by the store itself where OneDrive ceases to
//! have the item of a binding, or the row of a binding goes — `forget_pin_of_binding`
//! and `forget_pins_of_rows`, called in their own transaction by every statement that
//! takes a row's OneDrive version because the item is gone there, or deletes the row:
//! `confirm_onedrive_version(Removed)` and `confirm_removal_below` (a removal OneDrive
//! confirmed, of an item and of all it had in it), `clear_onedrive_version`,
//! `clear_onedrive_versions_below` and `clear_onedrive_versions_unseen` (a listing's
//! removal, what it takes below, and what a full listing did not see), and
//! `delete_rows_of_binding_alone`. A pin of an item the folder still shows goes with
//! OneDrive's version of it, before the plan takes the item off the folder: there is
//! nothing left in OneDrive that the pin could keep here.
//! Until the switch today's mount writes the table too, by item id
//! (`TreeStore::set_pin`), with no row asked.
//!
//! **Answers for:** `Files.Pin` / `Unpin`; and that no pin outlives what it was of: a
//! pinned file removed here whose removal was confirmed, an item a listing says is gone
//! and what a full listing's sweep takes, lose their pin with no caller's care (the map's
//! UNSURE-2.c). An item that is not in OneDrive any more by its row, and is bound still,
//! can be given a pin; it goes with the row.
//!
//! **Tested without the rest:**
// test: a_pin_is_set_on_the_binding_of_the_item
// test: an_item_with_no_binding_cannot_be_pinned
// test: a_pin_goes_with_its_binding_when_the_binding_moves_to_another_item
// test: a_confirmed_removal_takes_the_pin_of_the_item
// test: a_listed_removal_and_a_full_listings_sweep_take_the_pin
// test: the_count_of_pins_is_of_bindings_that_rows_still_have
use rusqlite::Connection;

use super::item_row::find_item;
use super::{ItemReader, ItemStore};
use crate::model::{LocalItemId, OneDriveItemReference};
use crate::TreeError;

/// What became of a pin that was asked to be set or taken off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinChange {
    /// The pin was set, or taken off.
    Written,
    /// The item's own pin was so already: set where it was to be set, absent where it
    /// was to be taken off. Nothing was written.
    AlreadySo,
    /// OneDrive does not have the item: there is nothing a pin could follow.
    NoBinding,
    /// The row is gone.
    NoItem,
}

/// Takes the pins off the bindings of the rows of `items` that `rows` finds: SQL that
/// follows `FROM items` — an index to find them by, or none, and the `WHERE` — with
/// `values` for its parameters. Called by the statements of `item_row` that take the
/// OneDrive version from several rows at once, or delete them, each before its own
/// statement and with that statement's own words for the rows.
pub(super) fn forget_pins_of_rows(conn: &Connection, rows: &str, values: impl rusqlite::Params) -> Result<(), TreeError> {
    conn.prepare_cached(&format!("DELETE FROM pins WHERE id IN (SELECT onedrive_item_id FROM items {rows})"))?.execute(values)?;
    Ok(())
}

/// Takes the pin off `binding`, when it has one. Called inside the transaction of
/// whatever leaves a row with its binding alone or deletes it.
pub(crate) fn forget_pin_of_binding(conn: &Connection, binding: &OneDriveItemReference) -> Result<(), TreeError> {
    conn.prepare_cached("DELETE FROM pins WHERE id = ?1")?.execute([binding.item_id.as_str()])?;
    Ok(())
}

impl ItemStore<'_> {
    /// Sets or takes off the pin of `item`, by its binding, read in the same transaction.
    /// Called by `fuse::ItemFilesInterface::set_pin` as one job of the store's thread; the
    /// caller wakes the plan, which downloads what the pin holds.
    pub fn set_item_pin(&mut self, item: LocalItemId, is_pinned: bool) -> Result<PinChange, TreeError> {
        self.write(|tx| {
            let Some(row) = find_item(tx, item)? else { return Ok(PinChange::NoItem) };
            let Some(binding) = row.onedrive_item else { return Ok(PinChange::NoBinding) };
            let changed_rows = match is_pinned {
                true => tx.prepare_cached("INSERT OR IGNORE INTO pins (id) VALUES (?1)")?.execute([binding.item_id.as_str()])?,
                false => tx.prepare_cached("DELETE FROM pins WHERE id = ?1")?.execute([binding.item_id.as_str()])?,
            };
            // No row changed: the pin was set, or absent, already.
            Ok(if changed_rows == 0 { PinChange::AlreadySo } else { PinChange::Written })
        })
    }
}

impl ItemReader<'_> {
    /// How many pins there are: `Folder.PinnedCount`.
    pub fn count_pins(&self) -> Result<u64, TreeError> {
        let count: i64 = self.conn().prepare_cached("SELECT COUNT(*) FROM pins JOIN items ON items.onedrive_item_id = pins.id")?.query_row([], |row| row.get(0))?;
        Ok(count.max(0) as u64)
    }
}

#[cfg(test)]
mod tests;
