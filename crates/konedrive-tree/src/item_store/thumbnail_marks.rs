//! Which version of a file's content a thumbnail was last made for, kept by local item id.
//!
//! **State owned:** what tells that a thumbnail in the cache is of the content the row has
//! now: today `onedrive_items.thumb_key` (cTag, path, time). Written by
//! [`ItemStore::set_thumbnail_made`] alone.
//! Kept in a table of its own, `thumbnail_marks`, keyed by local item id, which goes with
//! the item's row by the trigger `step_memory` uses. The table comes with the raise of the
//! schema's version that only adds (`schema::old_store`): made in step 0b, not by this
//! module's piece.
//!
//! **Answers for:** the thumbnail filler's two questions (`desktop/thumbs.rs:110`, `:258`).
//!
//! **Tested without the rest:**
// test: the_shown_images_and_videos_without_a_thumbnail_of_their_content_are_candidates
// test: a_file_whose_content_or_path_changed_since_its_thumbnail_is_a_candidate_again
// test: a_file_the_folder_does_not_show_is_no_candidate
#![allow(dead_code, unused_variables)]

use std::path::PathBuf;

use super::{ItemReader, ItemStore};
use crate::model::{ContentVersion, Item, LocalItemId};
use crate::TreeError;

/// A file a thumbnail is to be made for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailCandidate {
    pub item: Item,
    /// Where the folder shows it, relative to the root: the cache names a thumbnail by it.
    pub path: PathBuf,
}

impl ItemReader<'_> {
    /// The files the folder shows that OneDrive has, whose MIME type is an image's or a
    /// video's and that have no thumbnail of the content OneDrive has now at the path they
    /// are at, by local item id, after `after`, at most `limit`. Called by the thumbnail
    /// filler after a listing ended and after steps changed the folder.
    pub fn list_thumbnail_candidates(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<ThumbnailCandidate>, TreeError> {
        todo!()
    }
}

impl ItemStore<'_> {
    /// A thumbnail of `content` was made for the file `item` at `path`; nothing is written
    /// when the row's OneDrive content is another by now. Called by the thumbnail filler.
    /// One transaction.
    pub fn set_thumbnail_made(&mut self, item: LocalItemId, content: &ContentVersion, path: &std::path::Path) -> Result<(), TreeError> {
        todo!()
    }
}
