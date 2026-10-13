//! What the mount writes of a file's local content that is no `LocalVersionChange` by
//! itself: its time set, its executable bit, its presence on disk written with a change in
//! one transaction, and its presence written only while the content is still the one the
//! caller saw.
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
// test: an_executable_bit_stays_with_the_file_through_its_other_writes_and_leaves_nothing_unsent
// test: a_directory_has_no_executable_bit
// test: a_file_opened_for_writing_with_nothing_on_disk_is_on_disk_in_the_same_transaction
// test: a_refused_change_leaves_the_presence_as_it_was
// test: a_file_made_open_for_writing_is_made_and_opened_in_one_transaction
// test: a_file_made_open_for_writing_that_is_refused_leaves_no_row
// test: the_files_several_steps_show_are_shown_in_one_transaction_each_by_its_own_versions
// test: a_landing_is_written_when_the_local_content_is_still_the_one_downloaded
// test: a_landing_of_content_the_row_no_longer_has_writes_nothing
// test: a_copy_is_freed_in_the_row_only_while_its_content_is_the_synced_one
// test: a_copy_that_was_changed_since_it_was_asked_about_is_not_freed
use rusqlite::Connection;

use super::item_row::{find_item, set_content_presence, write_item};
use super::place_change_records::{forget_place_change_record, forget_place_change_record_when_written};
use super::step_commit::commit_show_item;
use super::{apply_change, make, refuse, ItemStore, PlaceChangeRecordId, StepCommitOutcome};
use crate::model::{ContentPresence, ContentVersion, Item, ItemKind, ItemPlace, LocalItemId, LocalVersionChange};
use crate::sync_plan::ItemVersions;
use crate::TreeError;

/// `content` with another modification time: the same bytes, told as they were.
pub(super) fn with_modified_time(content: &ContentVersion, mtime: i64, mtime_nsec: u32) -> ContentVersion {
    match content.clone() {
        ContentVersion::Hashed { size, hash, ctag, .. } => ContentVersion::Hashed { size, mtime, mtime_nsec, hash, ctag },
        ContentVersion::ChangedHere { size, change_count, .. } => ContentVersion::ChangedHere { size, mtime, mtime_nsec, change_count },
    }
}

/// The row of `item` with its local content, when it is a file the folder shows; `None`
/// for a row that is gone, an item the folder does not show, and a directory: none of them
/// has a content that could be on disk.
fn find_shown_file(conn: &Connection, item: LocalItemId) -> Result<Option<(Item, ContentVersion)>, TreeError> {
    let Some(row) = find_item(conn, item)? else { return Ok(None) };
    let content = row.local.as_ref().and_then(|local| local.content.clone());
    Ok(content.map(|content| (row, content)))
}

/// Written when `is_written`, and otherwise the rows changed.
fn outcome_of(is_written: bool) -> StepCommitOutcome {
    if is_written {
        StepCommitOutcome::Written
    } else {
        StepCommitOutcome::RowsChanged
    }
}

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
    /// (`write_operations/write.rs:183-191`): the row's local content and the content
    /// agreed on stay one, so nothing is unsent and the copy can still be freed, as the
    /// content with the time that was set. OneDrive's content keeps OneDrive's time, and
    /// no plan takes the time back: a time alone is no change of a content, so the row is
    /// not among those changed in OneDrive (`item_row::CHANGED_IN_ONEDRIVE`) and the
    /// plan's cell for a content that is the same on both sides answers that nothing is
    /// to do (`sync_plan::item_decision::decide_content`).
    pub fn set_local_modified_time(&mut self, item: LocalItemId, mtime: i64, mtime_nsec: u32) -> Result<(), TreeError> {
        self.write(|tx| {
            let mut row = find_item(tx, item)?.ok_or_else(|| refuse(format!("item {item} has no row")))?;
            let Some(local) = row.local.as_mut() else { return Err(refuse(format!("the folder does not show item {item}: it has no time to set"))) };
            let Some(content) = local.content.as_mut().filter(|_| row.kind == ItemKind::File) else { return Err(refuse(format!("item {item} is no file: its row keeps no time"))) };
            if row.content_before_write.is_some() {
                return Err(refuse(format!("a writer holds item {item}: its close writes the times")));
            }
            let is_synced = row.synced_content.as_ref() == Some(&*content);
            *content = with_modified_time(content, mtime, mtime_nsec);
            if is_synced {
                row.synced_content = Some(content.clone());
            }
            write_item(tx, &row)
        })
    }

    /// The file `item` has its owner's executable bit from now on, or has it no more: what
    /// a chmod through the mount asked ([`Item::is_executable`](crate::Item)). Nothing
    /// else of the row changes, so nothing becomes unsent. One transaction. Refused for an
    /// id with no row and for a directory.
    pub fn set_executable_bit(&mut self, item: LocalItemId, is_executable: bool) -> Result<(), TreeError> {
        self.write(|tx| match super::item_row::set_executable_bit(tx, item, is_executable)? {
            true => Ok(()),
            false => Err(refuse(format!("item {item} is no file with a row: it has no executable bit"))),
        })
    }

    /// Applies `change` as [`apply`](Self::apply) does and says the file's content is on
    /// disk, in one transaction: for the first writer's open, a truncation to nothing and a
    /// last close of a file whose content was not on disk, where the backing file the
    /// mount has just made is the whole content from then on. Refused, with nothing
    /// written, as `apply` refuses, and for a change that is of no file's content (a
    /// making, a move, a save over, a removal). A change of a file the folder does not
    /// show any more, or whose row went, is nothing and no error, as in `apply`.
    ///
    /// Called by the mount's content writes, under the tree lock, before the first byte
    /// goes to disk (design §8: the row first). The map's "one transaction for opened for
    /// writing and on disk" (`ItemStore::write` is private to the store).
    pub fn apply_with_content_on_disk(&mut self, change: &LocalVersionChange) -> Result<LocalItemId, TreeError> {
        self.write(|tx| {
            match change {
                LocalVersionChange::OpenedForWriting { .. } | LocalVersionChange::ClosedChanged { .. } | LocalVersionChange::ClosedUnchanged { .. } | LocalVersionChange::Truncated { .. } => {}
                LocalVersionChange::Made { .. } | LocalVersionChange::Moved { .. } | LocalVersionChange::SavedOver { .. } | LocalVersionChange::Removed { .. } => {
                    return Err(refuse("the change is of no file's content: it does not say that a content is on disk".to_owned()));
                }
            }
            let item = apply_change(tx, change)?;
            // A file that is gone, which a program still holds open: nothing, as `apply`.
            if find_item(tx, item)?.is_some_and(|row| row.local.is_some()) {
                set_content_presence(tx, item, ContentPresence::OnDisk)?;
            }
            Ok(item)
        })
    }

    /// The whole content `downloaded` of the file `item` is in the backing directory, under
    /// its name, synced: the row says `OnDisk` when the item's local content is still
    /// `downloaded`, and otherwise nothing is written ([`StepCommitOutcome::RowsChanged`]):
    /// the caller removes the file it landed. Otherwise is: the row went, the folder does
    /// not show the item, the content is another — also that of a writer's open, which
    /// raised its change count.
    ///
    /// Called by the landing of a download a read or `Files.Hydrate` asked for, on the
    /// write-operation thread, after the file was synced, named and its directory synced
    /// (design §8). A download the plan asked for commits through
    /// `commit_download_content` instead, which asks every version it was planned from.
    /// `record` is the record of the place change this commit ends, when one was written:
    /// it goes in this transaction when the commit is written
    /// (`place_change_records::forget_place_change_record_when_written`, or
    /// `forget_place_change_record` for a write that has no outcome), and stands otherwise.
    pub fn commit_landed_content(&mut self, item: LocalItemId, downloaded: &ContentVersion, record: Option<PlaceChangeRecordId>) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| {
            let is_still_downloaded = find_shown_file(tx, item)?.is_some_and(|(_, content)| content == *downloaded);
            if is_still_downloaded {
                set_content_presence(tx, item, ContentPresence::OnDisk)?;
            }
            forget_place_change_record_when_written(tx, record, outcome_of(is_still_downloaded))
        })
    }

    /// The copy of the file `item` is to go from the backing directory: the row says
    /// `NotOnDisk` when the item's local content is `content` and is the synced one, and
    /// no writer holds it; otherwise nothing is written
    /// ([`StepCommitOutcome::RowsChanged`]). Of a copy the row already says is not on disk
    /// it is written again. Written before the file goes
    /// (design §8: "the row says `NotOnDisk` first, then the file goes").
    ///
    /// Called by free-up (`mount::item_free_up`), on the write-operation thread under the
    /// tree lock, with the content it decided on.
    pub fn commit_freed_content(&mut self, item: LocalItemId, content: &ContentVersion) -> Result<StepCommitOutcome, TreeError> {
        self.write(|tx| {
            let is_clean = find_shown_file(tx, item)?.is_some_and(|(row, local_content)| local_content == *content && row.synced_content.as_ref() == Some(content) && row.content_before_write.is_none());
            if is_clean {
                set_content_presence(tx, item, ContentPresence::NotOnDisk)?;
            }
            Ok(outcome_of(is_clean))
        })
    }

    /// A file was made at `place` and is open for writing from its making (the kernel's
    /// create): the row is made as `LocalVersionChange::Made` makes it and its first
    /// writer's open written as `OpenedForWriting`, in one transaction, the second by the
    /// id the first gave. The made file's id. Refused, with nothing written, as `Made` is.
    ///
    /// Called by `fuse`'s `make` for `MadeKind::FileOpenedForWriting`, after the file was
    /// made on disk. `apply_all` cannot do it: it maps each change by itself, and the
    /// second needs the first one's id (the review's S14).
    /// `record` is the record of the place change this commit ends, when one was written:
    /// it goes in this transaction when the commit is written
    /// (`place_change_records::forget_place_change_record_when_written`, or
    /// `forget_place_change_record` for a write that has no outcome), and stands otherwise.
    pub fn apply_made_open_for_writing(&mut self, place: &ItemPlace, content: &ContentVersion, is_name_ignored: bool, replaced: Option<LocalItemId>, record: Option<PlaceChangeRecordId>) -> Result<LocalItemId, TreeError> {
        self.write(|tx| {
            let made = make(tx, ItemKind::File, place, Some(content), is_name_ignored, replaced)?;
            apply_change(tx, &LocalVersionChange::OpenedForWriting { item: made })?;
            if let Some(record) = record {
                forget_place_change_record(tx, record)?;
            }
            Ok(made)
        })
    }

    /// The commits of several `FolderStep::ShowItem` of one plan in one transaction: each
    /// item with whether its name is ignored, each checked and written by itself as
    /// `commit_show_item` does, and answered in the order given. A directory is given
    /// before what is in it; an item whose directory's commit answered `RowsChanged`
    /// answers so too, since the folder does not show the directory. A step no plan asks
    /// for (an item the folder shows already, or that OneDrive does not have) is an error,
    /// and then nothing of the call is written.
    ///
    /// Called by `fuse::FolderSteps::run_show_items`: a first listing of N items is then a
    /// few jobs of the store and not N (the review's S2).
    pub fn commit_show_items(&mut self, items: &[(ItemVersions, bool)]) -> Result<Vec<StepCommitOutcome>, TreeError> {
        self.write(|tx| {
            let mut outcomes = Vec::with_capacity(items.len());
            for (item, is_name_ignored) in items {
                outcomes.push(commit_show_item(tx, item, *is_name_ignored)?);
            }
            Ok(outcomes)
        })
    }
}

#[cfg(test)]
mod tests;
