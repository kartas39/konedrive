//! The content an upload of this folder left in an item of OneDrive that is not the
//! content it sent: the typed fact "OneDrive's content X is this folder's own upload of
//! the local content L".
//!
//! **State owned:** one fact per item, kept with what the steps remember (a column pair
//! of `step_memory`, written and cleared here until the step memory's module takes it:
//! it follows the item as the mismatched item does, and goes with the row). It stands
//! while the row's OneDrive content is `onedrive_content`.
//!
//! **Answers for:** the review's S19: without it, an upload into an existing item that
//! ended with another hash reads at the next plan as "changed on both sides", and the
//! user gets a conflict copy of his own file against this folder's failed upload — once
//! per upload if OneDrive rewrites what it is sent. With it the plan can tell the case;
//! what it then does is the owner's (`upload::onedrive_steps::content_upload`,
//! `OwnUploadFollowUp`).
//!
//! **Tested without the rest:**
// test: the_fact_is_answered_while_onedrives_content_is_the_one_the_upload_left
// test: the_fact_is_not_answered_once_a_listing_wrote_another_content
// test: the_fact_goes_with_the_items_row_and_stays_with_the_item_when_its_binding_moves
#![allow(dead_code, unused_variables)]

use super::{ItemReader, ItemStore};
use crate::model::{ContentVersion, LocalItemId};
use crate::TreeError;

/// What an upload of this folder left in OneDrive in place of what it sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnUploadContent {
    /// The content OneDrive has of the item since that upload, as its answer told it.
    pub onedrive_content: ContentVersion,
    /// The local content that was sent.
    pub local_content: ContentVersion,
}

impl ItemStore<'_> {
    /// An upload of `item`'s local content ended with other content in OneDrive. Written
    /// by the step before it answers `StepRefusal::ContentMismatch`. One transaction.
    pub fn remember_own_upload_content(&mut self, item: LocalItemId, upload: &OwnUploadContent) -> Result<(), TreeError> {
        todo!()
    }

    /// The fact is done with: the content was sent again and confirmed, or OneDrive's was
    /// taken.
    pub fn forget_own_upload_content(&mut self, item: LocalItemId) -> Result<(), TreeError> {
        todo!()
    }
}

impl ItemReader<'_> {
    /// The fact for `item`, while its row's OneDrive content is still the one the upload
    /// left. Called by the plan's input and by `SendContent` before it sends again.
    pub fn find_own_upload_content(&self, item: LocalItemId) -> Result<Option<OwnUploadContent>, TreeError> {
        todo!()
    }
}
