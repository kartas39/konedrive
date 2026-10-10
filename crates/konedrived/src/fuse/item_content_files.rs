//! The local content of an item as a step towards OneDrive reads it, and the marks a
//! confirmed step leaves on what it sent.
//!
//! **State owned:** none; it reaches the backing directory by the path of the item's
//! local version (design §8: "a row finds its file by the path of its local version"; no
//! file handle is stored), through `PlaceChanges::open_at_row_path`: the file it opens is
//! the file of the row it read, whatever is renamed meanwhile. It is the one way a part
//! outside the mount gets at a backing file: the upload worker's own finding of entries
//! on disk goes (design §11).
//!
//! **Answers for:** the bytes a step sends and hashes being those of the version it was
//! planned from; and design §8's marks after an upload: the OneDrive item id, the
//! whole-copy state and the cTag the content was confirmed as (whatever agrees a content
//! writes all three: the notes of the check at start).
//!
//! **Tested without the rest**, over the fixture (`item_fixture`):
// test: the_content_is_opened_at_the_path_of_the_local_version
// test: a_file_renamed_twice_between_the_row_and_the_open_is_still_the_rows_file
// test: a_file_whose_size_or_time_is_not_the_planned_contents_is_not_opened
// test: a_file_a_program_holds_open_for_writing_is_not_opened
// test: a_fragment_read_after_the_file_changed_says_so
// test: the_hash_of_the_content_is_the_quickxorhash_of_its_bytes
// test: a_sent_file_is_marked_with_the_item_id_the_whole_copy_state_and_the_ctag
// test: a_file_freed_or_renamed_during_its_upload_is_marked_through_its_descriptor
// test: a_directory_made_in_onedrive_is_marked_with_its_item_id_when_it_is_on_disk
// test: an_item_that_gave_its_binding_up_loses_its_marks
#![allow(dead_code, unused_variables)]

use std::fs::File;
use std::sync::Arc;

use konedrive_tree::{ConfirmedItem, ContentVersion, ItemVersions, LocalItemId, Store};

use super::item_handles::ItemHandles;
use super::place_changes::PlaceChanges;
use crate::folder::step_outcome::StepTrouble;

/// The backing file of an item's local content, open for reading, as it was when it was
/// opened.
pub struct LocalContentFile {
    item: LocalItemId,
    file: File,
    /// The content the step was planned from: every read asks that the file still has its
    /// size and its time.
    content: ContentVersion,
}

impl LocalContentFile {
    /// The size of the content, in bytes.
    pub fn size(&self) -> u64 {
        todo!()
    }

    /// `len` bytes from `offset`, after asking that the file's size and time are still the
    /// planned content's (`upload/local.rs:301`, `read`). A free-up of the same file
    /// cannot come in between: free-up leaves a file a step runs for. Blocking: called
    /// off the runtime.
    pub fn read_fragment(&self, offset: u64, len: usize) -> Result<Vec<u8>, StepTrouble> {
        todo!()
    }

    /// The QuickXorHash of the whole content, base64 (`upload/content/session.rs:171`,
    /// `Job::hash`). Blocking; no lock is held. Called by `RowStep::HashLocalContent` and
    /// by a step that asks whether what OneDrive has is what it sent.
    pub fn hash(&self) -> Result<String, StepTrouble> {
        todo!()
    }
}

/// The local contents of one folder.
pub struct ItemContentFiles {
    place_changes: Arc<PlaceChanges>,
    store: Store,
    item_handles: Arc<ItemHandles>,
}

impl ItemContentFiles {
    /// Made by the mount's build.
    pub(super) fn new(place_changes: Arc<PlaceChanges>, store: Store, item_handles: Arc<ItemHandles>) -> Self {
        todo!()
    }

    /// Opens the local content of the file `item` was planned as: through
    /// `PlaceChanges::open_at_row_path`, whose read closure
    /// (`Store::read_items_blocking`) answers the path only while the row's local content
    /// is still the planned one; then no program may hold the file open for writing, and
    /// the file must have that content's size and time. An error of the open is turned
    /// by `place_changes::decide_open_trouble`. Blocking; it reads through
    /// `Store::read_items_blocking`, the one reading connection the plan and the bus read
    /// through too: an open may wait behind a plan that closes a large set.
    ///
    /// Called by a step towards OneDrive that sends content and by
    /// `RowStep::HashLocalContent`, before any request and with no lock. Replaces
    /// `upload/local.rs:109` (`find`), `:216` (`open_for_upload`) and
    /// `steps/shared.rs:133` (`locate`).
    pub fn open_to_send(&self, item: &ItemVersions) -> Result<LocalContentFile, StepTrouble> {
        todo!()
    }

    /// Marks the file that was sent with what OneDrive confirmed: the item id, the
    /// whole-copy state and the cTag, synced (`upload/local.rs:323`, `commit_attributes`;
    /// `:336`, `commit_id`), through the descriptor it was read through, wherever the
    /// file is by now. Blocking, with NO lock: it goes through the descriptor, and one
    /// step per item keeps `remove_onedrive_marks` away. Called by a step towards
    /// OneDrive after its commit was written; a failure is logged: the marks serve the
    /// check at start and a lost store, not the running daemon (design §8).
    pub fn mark_sent_file(&self, file: &LocalContentFile, sent: &ConfirmedItem) -> std::io::Result<()> {
        todo!()
    }

    /// Marks the directory of `item` with the item id OneDrive gave it, when the directory
    /// is in the backing directory (`upload/local.rs:346`, `commit_dir`): opened through
    /// `PlaceChanges::open_at_row_path`. Called by the runner of `MakeDirectory` after its
    /// commit.
    pub fn mark_made_directory(&self, item: &ItemVersions, made: &ConfirmedItem) -> std::io::Result<()> {
        todo!()
    }

    /// Marks what is on disk of `item` as the whole copy of the content the two sides
    /// agree on now, with the binding's item id and the cTag: after `MergeItems` and
    /// `AgreeOnContent`. Opened through `PlaceChanges::open_at_row_path`.
    pub fn mark_agreed_content(&self, item: LocalItemId) -> std::io::Result<()> {
        todo!()
    }

    /// Takes the OneDrive marks off what is on disk of `item`: it gave its binding up and
    /// is an item OneDrive does not have (`upload/local.rs:360`, `strip_found`). Called by
    /// the runner of `GiveUpBinding` after its commit.
    pub fn remove_onedrive_marks(&self, item: LocalItemId) -> std::io::Result<()> {
        todo!()
    }
}
