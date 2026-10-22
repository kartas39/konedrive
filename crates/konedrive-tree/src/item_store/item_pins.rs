//! The pins and the exceptions from them, asked and set by item, and taken off with the
//! binding they follow.
//!
//! **State owned:** the table `pins`, which holds an item's own mark by its OneDrive item
//! id (`docs/design/pinning.md` §2.1: a pin follows the binding): a pin (`is_kept` 1), "always keep on this
//! device", or an exception (`is_kept` 0), "not this one" below a pinned directory
//! (`PinMark`). The nearest mark at an item or above it in the folder decides whether a
//! pin holds for it (`item_row::find_is_pinned`): an exception below a pin wins over the
//! pin, and a pin below an exception over the exception.
//! Written here alone: [`ItemStore::set_item_pin`] makes a pin hold for an item or not,
//! which gives the item the mark that says so against what is above it
//! (`decide_own_pin_mark`) and takes every mark below it off — what is asked of a
//! directory is asked of all of it. And taken off by the store itself where OneDrive
//! ceases to have the item of a binding, or the row of a binding goes —
//! `forget_pin_of_binding` and `forget_pins_of_rows`, called in their own transaction by
//! every statement that takes a row's OneDrive version because the item is gone there, or
//! deletes the row: `confirm_onedrive_version(Removed)` and `confirm_removal_below` (a
//! removal OneDrive confirmed, of an item and of all it had in it),
//! `clear_onedrive_version`, `clear_onedrive_versions_below` and
//! `clear_onedrive_versions_unseen` (a listing's removal, what it takes below, and what a
//! full listing did not see), and `delete_rows_of_binding_alone`. A mark of an item the
//! folder still shows goes with OneDrive's version of it, before the plan takes the item
//! off the folder: there is nothing left in OneDrive that the pin could keep here.
//!
//! **Answers for:** `Files.Pin` / `Unpin` / `FreeUp`; and that no mark outlives what it
//! was of: a pinned file removed here whose removal was confirmed, an item a listing says
//! is gone and what a full listing's sweep takes, lose their mark with no caller's care.
//! An item that is not in OneDrive any more by its row, and is
//! bound still, can be given a pin; it goes with the row.
//!
//! **Tested without the rest:**
// test: a_pin_is_set_on_the_binding_of_the_item
// test: an_item_with_no_binding_cannot_be_pinned
// test: a_pin_goes_with_its_binding_when_the_binding_moves_to_another_item
// test: a_confirmed_removal_takes_the_pin_of_the_item
// test: a_listed_removal_and_a_full_listings_sweep_take_the_pin
// test: the_count_of_pins_is_of_bindings_that_rows_still_have
// test: the_pinned_bytes_are_the_sizes_of_the_files_the_pins_hold
// test: the_own_mark_says_what_the_marks_above_do_not
// test: an_item_below_a_pin_is_excepted_and_a_pin_below_the_exception_holds
// test: pinning_an_excepted_item_takes_the_exception_off
// test: what_is_asked_of_a_directory_takes_the_marks_below_it_off
use rusqlite::{params, Connection, OptionalExtension};

use super::item_row::{find_is_pinned_above, find_item, sum_pinned_file_sizes};
use super::{ItemReader, ItemStore};
use crate::model::{ItemKind, LocalItemId, OneDriveItemReference};
use crate::TreeError;

/// What became of a pin that was asked to hold for an item, or not to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinChange {
    /// A mark was written or taken off: the item's own, or one below it.
    Written,
    /// A pin held for the item already, or held not, as asked, and there was no mark
    /// below it. Nothing was written.
    AlreadySo,
    /// OneDrive does not have the item: there is nothing a pin could follow.
    NoBinding,
    /// The row is gone.
    NoItem,
}

/// An item's own mark in `pins`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PinMark {
    /// "Always keep on this device": holds for the item and for what the folder shows
    /// below it, down to an exception.
    Pin,
    /// "Not this one": no pin holds for the item or for what the folder shows below it,
    /// down to a pin.
    Exception,
}

impl PinMark {
    /// The mark as the column `is_kept` has it.
    fn is_kept(self) -> bool {
        self == PinMark::Pin
    }
}

/// The mark an item is to have of its own, so that a pin holds for it (`is_pinned`) or
/// does not, where the marks above it say `is_pinned_above`: none where they say so
/// already, else the one that says otherwise. Pure.
pub(crate) fn decide_own_pin_mark(is_pinned: bool, is_pinned_above: bool) -> Option<PinMark> {
    match (is_pinned, is_pinned_above) {
        (true, true) | (false, false) => None,
        (true, false) => Some(PinMark::Pin),
        (false, true) => Some(PinMark::Exception),
    }
}

/// The mark the binding `item_id` has of its own.
fn find_own_pin_mark(conn: &Connection, item_id: &str) -> Result<Option<PinMark>, TreeError> {
    let is_kept: Option<bool> = conn.prepare_cached("SELECT is_kept FROM pins WHERE id = ?1")?.query_row([item_id], |row| row.get(0)).optional()?;
    Ok(is_kept.map(|is_kept| if is_kept { PinMark::Pin } else { PinMark::Exception }))
}

/// Takes off the marks of the items the folder shows below `directory`, at any depth; how
/// many. Found from the marks up — there are few — and not from the directory down.
fn forget_pins_below(conn: &Connection, directory: LocalItemId) -> Result<usize, TreeError> {
    let marked: Vec<(String, i64)> = conn
        .prepare_cached("SELECT pins.id, items.local_item_id FROM pins JOIN items ON items.onedrive_item_id = pins.id WHERE items.local_parent_id IS NOT NULL")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let is_below_sql = format!(
        "WITH RECURSIVE above(id, depth) AS (
             SELECT local_parent_id, 1 FROM items WHERE local_item_id = ?1 AND local_parent_id IS NOT NULL
             UNION ALL
             SELECT items.local_parent_id, above.depth + 1 FROM above JOIN items ON items.local_item_id = above.id WHERE items.local_parent_id IS NOT NULL AND above.depth <= {})
         SELECT EXISTS (SELECT 1 FROM above WHERE id = ?2)",
        crate::MAX_CHAIN
    );
    let mut forgotten = 0;
    for (item_id, marked_item) in marked {
        let is_below: bool = conn.prepare_cached(&is_below_sql)?.query_row(params![marked_item, directory.as_i64()], |row| row.get(0))?;
        if is_below {
            forgotten += conn.prepare_cached("DELETE FROM pins WHERE id = ?1")?.execute([item_id.as_str()])?;
        }
    }
    Ok(forgotten)
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
    /// Makes a pin hold for `item` (`is_pinned`), or not, in one transaction: every mark
    /// the folder shows below a directory comes off, and the item gets the mark of its own
    /// that says so against the marks above it ([`decide_own_pin_mark`]), by its binding.
    /// Called by `mount::ItemFilesInterface::set_pin` as one job of the store's thread; the
    /// caller wakes the plan, which downloads what the pin holds.
    pub fn set_item_pin(&mut self, item: LocalItemId, is_pinned: bool) -> Result<PinChange, TreeError> {
        self.write(|tx| {
            let Some(row) = find_item(tx, item)? else { return Ok(PinChange::NoItem) };
            let Some(binding) = row.onedrive_item else { return Ok(PinChange::NoBinding) };
            let forgotten_below = if row.kind == ItemKind::Directory { forget_pins_below(tx, item)? } else { 0 };
            let had = find_own_pin_mark(tx, binding.item_id.as_str())?;
            let has = decide_own_pin_mark(is_pinned, find_is_pinned_above(tx, item)?);
            match has {
                _ if has == had => {}
                Some(mark) => {
                    tx.prepare_cached("INSERT OR REPLACE INTO pins (id, is_kept) VALUES (?1, ?2)")?.execute(params![binding.item_id.as_str(), mark.is_kept()])?;
                }
                None => {
                    tx.prepare_cached("DELETE FROM pins WHERE id = ?1")?.execute([binding.item_id.as_str()])?;
                }
            }
            Ok(if has == had && forgotten_below == 0 { PinChange::AlreadySo } else { PinChange::Written })
        })
    }
}

impl ItemReader<'_> {
    /// How many pins there are, the exceptions not among them: `Folder.PinnedCount`.
    pub fn count_pins(&self) -> Result<u64, TreeError> {
        let count: i64 = self.conn().prepare_cached("SELECT COUNT(*) FROM pins JOIN items ON items.onedrive_item_id = pins.id WHERE pins.is_kept = 1")?.query_row([], |row| row.get(0))?;
        Ok(count.max(0) as u64)
    }

    /// The sizes the folder shows of the files a pin holds for, summed: `Folder.PinnedBytes`.
    /// A file made here counts, an excepted one does not. Reads every row below a pinned
    /// directory, as the reader of the pinned files to download does.
    pub fn sum_pinned_file_sizes(&self) -> Result<u64, TreeError> {
        sum_pinned_file_sizes(self.conn())
    }
}

#[cfg(test)]
mod tests;
