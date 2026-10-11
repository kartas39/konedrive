//! Free up space: the downloaded copy of a file removed, the row first.
//!
//! **State owned:** none.
//!
//! **Answers for:** design §9, last row — refused while the local content is not the
//! synced one or a program holds the file open for writing — and design §8's order: "the
//! row says `NotOnDisk` first, then the file goes". Today the file is removed with no row
//! touched (`files_interface.rs:230`). And the mending of a row that says on disk over a
//! file that is not there (the map's E1).
//!
//! **Tested without the rest**, a test per case of the decision, over the fixture:
// test: a_clean_copy_is_freed
// test: a_pinned_file_is_left
// test: a_file_changed_here_is_left_its_copy_is_the_only_one
// test: a_file_open_for_writing_is_left
// test: a_file_being_downloaded_or_sent_is_left
// test: a_file_not_on_disk_and_a_directory_have_nothing_to_free
// test: the_row_says_not_on_disk_before_the_copy_goes
// test: a_copy_changed_between_the_decision_and_the_commit_stays
// test: a_clean_row_that_says_on_disk_over_no_file_is_mended_to_not_on_disk
// test: a_changed_row_that_says_on_disk_over_no_file_is_left_for_the_check_at_start
// test: a_death_between_the_row_and_the_copys_going_leaves_a_copy_the_check_at_start_reads_as_landed
// test: a_copy_that_cannot_be_removed_is_on_disk_in_its_row_again
// test: a_freed_copy_and_a_mended_row_are_announced_and_what_is_left_is_not
//!
//! **A copy that cannot be removed** after its row said not on disk is said on disk again
//! (`ItemStore::commit_landed_content`): the row never says for long that content is not
//! here which the disk holds under the item's name.
//!
//! **Announced** (`ItemAnnouncement::ContentChanged`, by the item and its place) once a
//! copy is gone and once a row is mended: the file's state goes from downloaded to not
//! on disk with nothing of the kernel's saying so, and whoever shows it — a file
//! manager's emblem — asks again. A file that was left, and a copy that could not be
//! removed and is on disk in its row again, are as they were: nothing is announced.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use konedrive_tree::{ContentPresence, ContentVersion, FileSyncState, Item, ItemKind, ItemPlace, LocalItemId, StepCommitOutcome, Store, TreeError};

use super::item_announcements::ItemAnnouncement;
use super::item_download_states::ItemDownloadStates;
use super::item_handles::ItemHandles;
use super::item_inotify_bridge::ItemInotifyBridge;
use super::place_changes::{PlaceChanges, RowPathStat};
use crate::folder::running_steps::RunningSteps;

/// What a free-up does with `item`. Pure; the first reason that holds, in the order of
/// [`FreeUpDecision`]'s cases. Replaces `free_up_decision::decide_free_up`, which takes
/// an entry's state of three sources: "changed here" is the row's
/// (`Item::file_sync_state` is `ModifiedHere` or `LocalOnly`). A file a step runs for is
/// left as one that is being downloaded is.
pub(super) fn decide_item_free_up(item: &Item, is_pinned: bool, is_downloading_or_sent: bool, is_open_for_writing: bool) -> FreeUpDecision {
    match item.file_sync_state() {
        _ if is_pinned => FreeUpDecision::LeavePinned,
        Some(FileSyncState::ModifiedHere | FileSyncState::LocalOnly) => FreeUpDecision::LeaveModifiedHere,
        _ if is_open_for_writing => FreeUpDecision::LeaveOpenForWriting,
        _ if is_downloading_or_sent => FreeUpDecision::LeaveDownloading,
        Some(FileSyncState::Synced) => FreeUpDecision::Free,
        Some(FileSyncState::RemoteOnly) | None => FreeUpDecision::NothingToFree,
    }
}

/// Whether `item` is a clean copy by its row: a file whose content the row says is on
/// disk, is the one the two sides agree on, and that no writer began to change. Its
/// content then. Pure.
pub(super) fn find_clean_copy_content(item: &Item) -> Option<&ContentVersion> {
    let content = item.local.as_ref()?.content.as_ref()?;
    let is_clean = item.kind == ItemKind::File && item.content_presence == ContentPresence::OnDisk && item.synced_content.as_ref() == Some(content) && item.content_before_write.is_none();
    is_clean.then_some(content)
}

/// What became of one file a free-up was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FreedCopy {
    /// The copy is gone: how many bytes it took on disk.
    Freed { bytes: u64 },
    /// It was left, and why.
    Left(FreeUpDecision),
    /// The row changed between the decision and the commit: left as it is.
    RowsChanged,
}

/// A copy whose row says not on disk already, and which is still in the backing directory.
#[derive(Debug)]
struct FreedRow {
    item: LocalItemId,
    /// Where the folder shows the file: what its freeing is announced by.
    place: ItemPlace,
    path: PathBuf,
    content: ContentVersion,
    /// What the copy takes on disk.
    bytes: u64,
}

/// The free-up of one mount.
pub(super) struct ItemFreeUp {
    place_changes: Arc<PlaceChanges>,
    store: Store,
    item_handles: Arc<ItemHandles>,
    download_states: Arc<ItemDownloadStates>,
    running_steps: Arc<RunningSteps>,
    /// Told of a copy that is gone and of a row that was mended.
    inotify_bridge: Arc<ItemInotifyBridge>,
}

impl ItemFreeUp {
    /// Made by the mount's build, after the bridge.
    pub(super) fn new(place_changes: Arc<PlaceChanges>, store: Store, item_handles: Arc<ItemHandles>, download_states: Arc<ItemDownloadStates>, running_steps: Arc<RunningSteps>, inotify_bridge: Arc<ItemInotifyBridge>) -> Self {
        Self { place_changes, store, item_handles, download_states, running_steps, inotify_bridge }
    }

    /// Frees the copy of the file `item`: one job of the store reads the row, its pin and
    /// its path; [`decide_item_free_up`]; `ItemStore::commit_freed_content` with the
    /// content it decided on; and only then the backing file removed, as a place change
    /// (`PlaceChange::set_aside`, its end, `PlaceChanges::discard`), and the change
    /// announced. A daemon that dies between the two leaves a file the row says is not on
    /// disk: the check at start reads it as the copy, landed before its row (design §10).
    ///
    /// Called by `ItemFilesInterface::free_up`, as a job of the write-operation thread,
    /// under the tree lock, once per file.
    pub(super) fn free_downloaded_copy(&self, item: LocalItemId) -> FreedCopy {
        match self.write_not_on_disk(item) {
            Ok(freed) => self.remove_freed_copy(freed),
            Err(left) => left,
        }
    }

    /// The first half of a free-up: the decision and the row. The copy whose row says not
    /// on disk now; or what the free-up answers when the row was left as it is.
    fn write_not_on_disk(&self, item: LocalItemId) -> Result<FreedRow, FreedCopy> {
        let fail = |error: TreeError| {
            tracing::warn!("the copy of item {item} is not freed: {error}");
            FreedCopy::RowsChanged
        };
        let read = self.store.call_blocking(move |store| {
            let item_store = store.item_store();
            let reader = item_store.reader();
            let Some(row) = reader.find_item(item)? else { return Ok(None) };
            let Some(path) = reader.find_path(item)? else { return Ok(None) };
            Ok(Some((reader.is_pinned(item)?, row, path)))
        });
        let Some((is_pinned, row, path)) = read.map_err(fail)? else { return Err(FreedCopy::Left(FreeUpDecision::NothingToFree)) };
        // What is not on disk takes no space, whatever keeps it.
        let shown = row.local.as_ref().and_then(|local| local.content.clone().zip(local.place.clone())).filter(|_| row.kind == ItemKind::File && row.content_presence == ContentPresence::OnDisk);
        let Some((content, place)) = shown else { return Err(FreedCopy::Left(FreeUpDecision::NothingToFree)) };
        let is_downloading_or_sent = self.download_states.is_downloading(item) || self.running_steps.has_running_step(item);
        match decide_item_free_up(&row, is_pinned, is_downloading_or_sent, self.item_handles.is_open_for_writing(item)) {
            FreeUpDecision::Free => {}
            left => return Err(FreedCopy::Left(left)),
        }
        // No place change runs on this thread now: the stat is of the row's file.
        let found = path.clone();
        let bytes = match self.place_changes.stat_at_row_path(move || Ok(Some((found.clone(), ())))) {
            Ok(RowPathStat::Facts { facts, .. }) => facts.blocks * 512,
            _ => 0,
        };
        let decided = content.clone();
        match self.store.call_blocking(move |store| store.item_store().commit_freed_content(item, &decided)).map_err(fail)? {
            StepCommitOutcome::Written => Ok(FreedRow { item, place, path, content, bytes }),
            StepCommitOutcome::RowsChanged => Err(FreedCopy::RowsChanged),
        }
    }

    /// The second half: the copy goes from the backing directory, and its going is
    /// announced. One that cannot be removed is said on disk in its row again.
    fn remove_freed_copy(&self, freed: FreedRow) -> FreedCopy {
        let FreedRow { item, place, path, content, bytes } = freed;
        let change = self.place_changes.begin();
        let set_aside = change.set_aside(&path, item);
        drop(change);
        match set_aside {
            Ok(undo) => {
                self.place_changes.discard(undo);
                self.inotify_bridge.announce(vec![ItemAnnouncement::ContentChanged { item, place }]);
                FreedCopy::Freed { bytes }
            }
            Err(refusal) => {
                tracing::warn!("the copy of item {item} at {} could not be removed: {refusal:?}", path.display());
                if let Err(error) = self.store.call_blocking(move |store| store.item_store().commit_landed_content(item, &content, None)) {
                    tracing::error!("the row of item {item} says not on disk over a copy that is there: {error}");
                }
                FreedCopy::RowsChanged
            }
        }
    }

    /// The row of `item` says its content is on disk and an open found no file there:
    /// when the row is a clean copy (its local content is the synced one) it is written
    /// not on disk (`ItemStore::commit_freed_content`), and the next open reads from
    /// OneDrive; a changed one is left as it is, for the check at start to read as a
    /// removal by hand. Whether it was mended; a mended row is announced. The disk is
    /// asked once more here, where no place change runs: a file that is there by now is
    /// no missing copy.
    ///
    /// Called as a job of the write-operation thread by the open request that got
    /// `ENOENT` through `PlaceChanges::open_at_row_path` (which has ruled out a rename
    /// under way), before the open is tried once more.
    pub(super) fn mend_missing_copy(&self, item: LocalItemId) -> bool {
        let read = self.store.call_blocking(move |store| {
            let item_store = store.item_store();
            let reader = item_store.reader();
            let Some(row) = reader.find_item(item)? else { return Ok(None) };
            let Some(content) = find_clean_copy_content(&row).cloned() else { return Ok(None) };
            let Some(place) = row.local.and_then(|local| local.place) else { return Ok(None) };
            Ok(reader.find_path(item)?.map(|path| (content, place, path)))
        });
        let Ok(Some((content, place, path))) = read else { return false };
        if !matches!(self.place_changes.stat_at_row_path(move || Ok(Some((path.clone(), ())))), Ok(RowPathStat::NothingOnDisk { .. })) {
            return false;
        }
        match self.store.call_blocking(move |store| store.item_store().commit_freed_content(item, &content)) {
            Ok(StepCommitOutcome::Written) => {
                self.inotify_bridge.announce(vec![ItemAnnouncement::ContentChanged { item, place }]);
                true
            }
            Ok(StepCommitOutcome::RowsChanged) => false,
            Err(error) => {
                tracing::warn!("the row of item {item}, whose copy is missing, could not be mended: {error}");
                false
            }
        }
    }
}

/// What a free-up does with a file or directory of OneDrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeUpDecision {
    /// Its downloaded copy is removed: the file stays in the folder as a name.
    Free,
    /// A pin keeps it on this device: its own, or a directory's above it.
    LeavePinned,
    /// Its content was changed here and is not sent yet: its copy is the only one there is.
    LeaveModifiedHere,
    /// A program holds it open for writing.
    LeaveOpenForWriting,
    /// It is being downloaded whole.
    LeaveDownloading,
    /// Nothing of it is downloaded, and nothing is to free: a file that is not downloaded,
    /// and a directory, which has no copy of its own.
    NothingToFree,
}

#[cfg(test)]
mod tests;
