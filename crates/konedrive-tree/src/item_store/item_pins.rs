//! The pins, asked and set by item, and taken off with the binding they follow.
//!
//! **State owned:** the table `pins`, which holds OneDrive item ids (design §5: a pin
//! follows the binding). Written here alone once the switch is made: set and taken off by
//! [`ItemStore::set_item_pin`], and taken off by the store itself where a binding goes —
//! [`forget_pin_of_binding`], called in their own transaction by the three places a row
//! is left with its binding alone or deleted with it: `confirm_onedrive_version(Removed)`,
//! a listing's removal (`clear_onedrive_version`) and `delete_rows_of_binding_alone` (the
//! one line each gains when this is filled in).
//!
//! **Answers for:** `Files.Pin` / `Unpin`; and that no pin outlives what it was of: a
//! pinned file removed here whose removal was confirmed, and a row a full listing's sweep
//! takes, lose their pin with no caller's care (the map's UNSURE-2.c).
//!
//! **Tested without the rest:**
// test: a_pin_is_set_on_the_binding_of_the_item
// test: an_item_with_no_binding_cannot_be_pinned
// test: a_pin_goes_with_its_binding_when_the_binding_moves_to_another_item
// test: a_confirmed_removal_takes_the_pin_of_the_item
// test: a_listed_removal_and_a_full_listings_sweep_take_the_pin
// test: the_count_of_pins_is_of_bindings_that_rows_still_have
#![allow(dead_code, unused_variables)]

use rusqlite::Connection;

use super::{ItemReader, ItemStore};
use crate::model::{LocalItemId, OneDriveItemReference};
use crate::TreeError;

/// What became of a pin that was asked to be set or taken off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinChange {
    Written,
    /// OneDrive does not have the item: there is nothing a pin could follow.
    NoBinding,
    /// The row is gone.
    NoItem,
}

/// Takes the pin off `binding`, when it has one. Called inside the transaction of
/// whatever leaves a row with its binding alone or deletes it.
pub(crate) fn forget_pin_of_binding(conn: &Connection, binding: &OneDriveItemReference) -> Result<(), TreeError> {
    todo!()
}

impl ItemStore<'_> {
    /// Sets or takes off the pin of `item`, by its binding, read in the same transaction.
    /// Called by `fuse::ItemFilesInterface::set_pin` as one job of the store's thread; the
    /// caller wakes the plan, which downloads what the pin holds.
    pub fn set_item_pin(&mut self, item: LocalItemId, is_pinned: bool) -> Result<PinChange, TreeError> {
        todo!()
    }
}

impl ItemReader<'_> {
    /// How many pins there are: `Folder.PinnedCount`.
    pub fn count_pins(&self) -> Result<u64, TreeError> {
        todo!()
    }
}
