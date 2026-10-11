//! The readers a sync plan is made from, over the reader: what `ItemStore` alone has
//! today, so that a plan need not be read through the writer.
//!
//! **State owned:** none. Each calls the function of `item_row` (or of the module that
//! owns what is read) that `ItemStore`'s reader of the same name calls, over the reader's
//! connection and inside its one read transaction.
//!
//! **Answers for:** the review's S2 and S13: the plan's reads are of one moment and wait
//! for no listing page and no write operation.
//!
//! **What a caller reads beside these**, of the reader's other part
//! ([`ItemReader`]): the unsent items (`list_unsent`), an item by its id, what the folder
//! shows in a directory and under a folded name, whether an item is left here, whether
//! the folder can show it, whether a pin holds for it, and the listing under way. The
//! test caller of the plan, `sync_plan/tests/convergence/plan_input.rs`, is the reference
//! for how a set of rows is closed with them.
//!
//! **The files a program holds open for writing** have no reader of their own: a row
//! keeps the content from before its first writer (`content_before_write`) until the
//! last writer closed, the first open makes the local content another one than the one
//! agreed on, and so such a file is among the unsent items and a caller reads the fact
//! off the rows it was given. One that is left here is not among them, and is never
//! sent.
//!
//! **What is found by an index and what is not.** By an index, bounded by `limit` or by
//! what one directory holds: what OneDrive changed (`items_changed_in_onedrive`), the
//! shown items that cannot be shown (`items_shown_against_placement`), what OneDrive has
//! in a directory and under a folded name there (`items_onedrive_place`,
//! `items_onedrive_folded_place`), the confirmation count (a key of `meta`). Not bounded,
//! and said with each: all OneDrive has below a directory, the pinned files to download.
//!
//! **Tested without the rest:**
// test: each_plan_reader_answers_what_the_item_stores_reader_of_the_same_name_answers
// test: the_items_whose_writer_has_not_closed_are_those_with_a_content_before_the_write


use super::onedrive_versions::read_confirmation_count;
use super::{item_row, ItemReader};
use crate::model::{Item, LocalItemId};
use crate::{FoldedName, TreeError};

impl ItemReader<'_> {
    /// As `ItemStore::list_changed_in_onedrive`: the items whose OneDrive version is not
    /// the synced one, a content's time left out, by local item id, after `after`, at most
    /// `limit`. By the index of those rows.
    pub fn list_changed_in_onedrive(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<Item>, TreeError> {
        item_row::list_changed_in_onedrive(self.conn(), after, limit)
    }

    /// As `ItemStore::list_pinned_files_to_download`. Reads every row below a pinned
    /// directory, and the plan's task asks it with every plan while a pinned file may be
    /// missing its content: a plan that left the pinned files out would drop the downloads
    /// the last one planned. No index makes
    /// it cheap: a pin holds for what is below it by the places in the folder, which
    /// change with every move, so a row cannot keep "a pin holds for me" without every
    /// move of a directory rewriting all that is below it.
    pub fn list_pinned_files_to_download(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<Item>, TreeError> {
        item_row::list_pinned_files_to_download(self.conn(), after, limit)
    }

    /// As `ItemStore::list_shown_items_that_cannot_be_shown`: by its index.
    pub fn list_shown_items_that_cannot_be_shown(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<Item>, TreeError> {
        item_row::list_shown_items_that_cannot_be_shown(self.conn(), after, limit)
    }

    /// As `ItemStore::list_children_in_onedrive`: what OneDrive has in `directory`, by
    /// name there. By the index of places in OneDrive; as many as the directory holds.
    pub fn list_children_in_onedrive(&self, directory: LocalItemId) -> Result<Vec<Item>, TreeError> {
        item_row::list_children_in_onedrive(self.conn(), directory)
    }

    /// As `ItemStore::list_below_in_onedrive`: all OneDrive has in `directory` and below
    /// it, in no order. A walk down by the index of places in OneDrive, as long as what is
    /// below the directory: a caller asks it of a directory that was removed here, whose
    /// removal is decided from all of it, and of no other — of the root it is every item
    /// OneDrive has.
    pub fn list_below_in_onedrive(&self, directory: LocalItemId) -> Result<Vec<Item>, TreeError> {
        item_row::list_below_in_onedrive(self.conn(), directory)
    }

    /// As `ItemStore::list_children_in_onedrive_with_folded_name`: by the index of folded
    /// names in OneDrive.
    pub fn list_children_in_onedrive_with_folded_name(&self, directory: LocalItemId, name: &FoldedName) -> Result<Vec<Item>, TreeError> {
        item_row::list_children_in_onedrive_with_folded_name(self.conn(), directory, name)
    }

    /// As `ItemStore::confirmation_count`: read by the listing before a page is fetched.
    pub fn confirmation_count(&self) -> Result<u64, TreeError> {
        read_confirmation_count(self.conn())
    }
}

#[cfg(test)]
mod tests;
