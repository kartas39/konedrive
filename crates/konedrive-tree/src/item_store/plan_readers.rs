//! The readers a sync plan is made from, over the reader: what `ItemStore` alone has
//! today, so that a plan need not be read through the writer.
//!
//! **State owned:** none. Each calls the function of `item_row` (or `pinned_items`) that
//! `ItemStore`'s reader of the same name calls, over the reader's connection and inside
//! its one read transaction.
//!
//! **Answers for:** the review's S2 and S13: the plan's reads are of one moment and wait
//! for no listing page and no write operation.
//!
//! **Tested without the rest:**
// test: each_plan_reader_answers_what_the_item_stores_reader_of_the_same_name_answers
// test: the_items_whose_writer_has_not_closed_are_those_with_a_content_before_the_write
#![allow(dead_code, unused_variables)]

use std::collections::BTreeSet;

use super::ItemReader;
use crate::model::{Item, LocalItemId};
use crate::{FoldedName, SentRemovalWindow, TreeError};

impl ItemReader<'_> {
    /// As `ItemStore::list_changed_in_onedrive`: the items whose OneDrive version is not
    /// the synced one, by local item id, after `after`, at most `limit`.
    pub fn list_changed_in_onedrive(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<Item>, TreeError> {
        todo!()
    }

    /// As `ItemStore::list_pinned_files_to_download`. Reads every row below a pinned
    /// directory: the plan's task asks it only when a pin or a listing's page changed
    /// what it answers (`WakeCause::UserAnswered`, `ListingPageWritten`).
    pub fn list_pinned_files_to_download(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<Item>, TreeError> {
        todo!()
    }

    /// As `ItemStore::list_shown_items_that_cannot_be_shown`: by its index.
    pub fn list_shown_items_that_cannot_be_shown(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<Item>, TreeError> {
        todo!()
    }

    /// As `ItemStore::list_children_in_onedrive`.
    pub fn list_children_in_onedrive(&self, directory: LocalItemId) -> Result<Vec<Item>, TreeError> {
        todo!()
    }

    /// As `ItemStore::list_below_in_onedrive`.
    pub fn list_below_in_onedrive(&self, directory: LocalItemId) -> Result<Vec<Item>, TreeError> {
        todo!()
    }

    /// As `ItemStore::list_children_in_onedrive_with_folded_name`.
    pub fn list_children_in_onedrive_with_folded_name(&self, directory: LocalItemId, name: &FoldedName) -> Result<Vec<Item>, TreeError> {
        todo!()
    }

    /// As `ItemStore::count_shown_items`.
    pub fn count_shown_items(&self) -> Result<u64, TreeError> {
        todo!()
    }

    /// As `ItemStore::count_unsent_removals`.
    pub fn count_unsent_removals(&self, confirmed: &BTreeSet<LocalItemId>) -> Result<u64, TreeError> {
        todo!()
    }

    /// As `ItemStore::find_sent_removal_window`: its `removal_count` is the plan's
    /// `sent_removal_count`. The store keeps the window; the plan's task begins it anew
    /// through the writer (`ItemStore::begin_sent_removal_window`).
    pub fn find_sent_removal_window(&self) -> Result<SentRemovalWindow, TreeError> {
        todo!()
    }

    /// As `ItemStore::confirmation_count`: read by the listing before a page is fetched.
    pub fn confirmation_count(&self) -> Result<u64, TreeError> {
        todo!()
    }
}
