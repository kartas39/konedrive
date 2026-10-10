//! What the mount writes of a file's local content that is no `LocalVersionChange` by
//! itself: its time set, its presence on disk written with a change in one transaction,
//! and its presence written only while the content is still the one the caller saw.
//!
//! **State owned:** none of its own: the local content's time, and `content_presence`,
//! which `content_presence` writes unconditionally and `step_commit` for a step of the
//! plan. The operations here are for the mount's own work, which is no step: a download a
//! read asked for, a free-up, a truncation of a file that is not on disk.
//!
//! **Answers for:** design §8's rule — a row never says content is on disk that the disk
//! does not hold for good, and never says a copy is clean while the disk may hold the
//! user's bytes — where two columns must change together or not at all.
//!
//! **Tested without the rest:**
// test: a_time_set_on_a_clean_copy_leaves_the_file_synced
// test: a_time_set_on_changed_content_is_the_times_of_that_content
// test: a_time_set_is_refused_while_a_writer_holds_the_file
// test: a_file_opened_for_writing_with_nothing_on_disk_is_on_disk_in_the_same_transaction
// test: a_refused_change_leaves_the_presence_as_it_was
// test: a_file_made_open_for_writing_is_made_and_opened_in_one_transaction
// test: a_file_made_open_for_writing_that_is_refused_leaves_no_row
// test: the_files_several_steps_show_are_shown_in_one_transaction_each_by_its_own_versions
// test: a_landing_is_written_when_the_local_content_is_still_the_one_downloaded
// test: a_landing_of_content_the_row_no_longer_has_writes_nothing
// test: a_copy_is_freed_in_the_row_only_while_its_content_is_the_synced_one
// test: a_copy_that_was_changed_since_it_was_asked_about_is_not_freed
#![allow(dead_code, unused_variables)]

use super::{ItemStore, StepCommitOutcome};
use crate::model::{ContentVersion, ItemPlace, LocalItemId, LocalVersionChange};
use crate::sync_plan::ItemVersions;
use crate::TreeError;

impl ItemStore<'_> {
    /// The modification time of the file `item` was set, and nothing else of it changed:
    /// the time of its local content is this one from now on, and of its synced content
    /// too when the two were equal, so that a clean copy stays clean and nothing is sent.
    /// Refused for a directory, for an item the folder does not show, and while a writer
    /// holds the file (`content_before_write` set): then the close writes the times.
    ///
    /// Called by the write operation `set_modified_time` (setattr with a time and no
    /// size), under the tree lock, after the time went to the backing file.
    /// A time set on a clean copy stays local, as today
    /// (`write_operations/write.rs:183-191`): the plan compares contents by hash, so
    /// nothing is sent and the copy can still be freed.
    pub fn set_local_modified_time(&mut self, item: LocalItemId, mtime: i64, mtime_nsec: u32) -> Result<(), TreeError> {
        todo!()
    }

    /// Applies `change` as [`apply`](Self::apply) does and says the file's content is on
    /// disk, in one transaction: for the first writer's open, a truncation to nothing and a
    /// last close of a file whose content was not on disk, where the backing file the
    /// mount has just made is the whole content from then on. Refused, with nothing
    /// written, as `apply` refuses, and for a change that is of no file.
    ///
    /// Called by the mount's content writes, under the tree lock, before the first byte
    /// goes to disk (design §8: the row first). The map's "one transaction for opened for
    /// writing and on disk" (`ItemStore::write` is private to the store).
    pub fn apply_with_content_on_disk(&mut self, change: &LocalVersionChange) -> Result<LocalItemId, TreeError> {
        todo!()
    }

    /// The whole content `downloaded` of the file `item` is in the backing directory, under
    /// its name, synced: the row says `OnDisk` when the item's local content is still
    /// `downloaded`, and otherwise nothing is written ([`StepCommitOutcome::RowsChanged`]):
    /// the caller removes the file it landed.
    ///
    /// Called by the landing of a download a read or `Files.Hydrate` asked for, on the
    /// write-operation thread, after the file was synced, named and its directory synced
    /// (design §8). A download the plan asked for commits through
    /// `commit_download_content` instead, which asks every version it was planned from.
    pub fn commit_landed_content(&mut self, item: LocalItemId, downloaded: &ContentVersion) -> Result<StepCommitOutcome, TreeError> {
        todo!()
    }

    /// The copy of the file `item` is to go from the backing directory: the row says
    /// `NotOnDisk` when the item's local content is `content` and is the synced one, and
    /// no writer holds it; otherwise nothing is written. Written before the file goes
    /// (design §8: "the row says `NotOnDisk` first, then the file goes").
    ///
    /// Called by free-up (`fuse::item_free_up`), on the write-operation thread under the
    /// tree lock, with the content it decided on.
    pub fn commit_freed_content(&mut self, item: LocalItemId, content: &ContentVersion) -> Result<StepCommitOutcome, TreeError> {
        todo!()
    }

    /// A file was made at `place` and is open for writing from its making (the kernel's
    /// create): the row is made as `LocalVersionChange::Made` makes it and its first
    /// writer's open written as `OpenedForWriting`, in one transaction, the second by the
    /// id the first gave. The made file's id. Refused, with nothing written, as `Made` is.
    ///
    /// Called by `fuse`'s `make` for `MadeKind::FileOpenedForWriting`, after the file was
    /// made on disk. `apply_all` cannot do it: it maps each change by itself, and the
    /// second needs the first one's id (the review's S14).
    pub fn apply_made_open_for_writing(&mut self, place: &ItemPlace, content: &ContentVersion, is_name_ignored: bool, replaced: Option<LocalItemId>) -> Result<LocalItemId, TreeError> {
        todo!()
    }

    /// The commits of several `FolderStep::ShowItem` of one plan in one transaction: each
    /// item with whether its name is ignored, each checked and written by itself as
    /// `commit_show_item` does, and answered in the order given. A directory is given
    /// before what is in it; an item whose directory's commit answered `RowsChanged`
    /// answers so too.
    ///
    /// Called by `fuse::FolderSteps::run_show_items`: a first listing of N items is then a
    /// few jobs of the store and not N (the review's S2).
    pub fn commit_show_items(&mut self, items: &[(ItemVersions, bool)]) -> Result<Vec<StepCommitOutcome>, TreeError> {
        todo!()
    }
}
