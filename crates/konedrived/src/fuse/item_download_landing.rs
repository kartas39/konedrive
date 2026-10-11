//! The landing of a downloaded file: the last step of every download, whoever asked for
//! it — a read, `Files.Hydrate`, an open for writing, or the plan for a pinned file.
//!
//! **State owned:** none.
//!
//! **Answers for:** design §8's order for a landing: the file is synced, given its name,
//! its directory synced, and then the row says `OnDisk`; and design §7: a landing is
//! conditional — it checks the versions it was downloaded as, and otherwise changes
//! nothing. The marks: the item id, the whole-copy state and, newly, the cTag the copy
//! was downloaded as (the check at start reads an id-only mark as "holds nothing").
//!
//! **Who syncs what:** the downloader syncs the file's data BEFORE the job is queued,
//! with no lock held. The job — on the write-operation thread, under the tree lock —
//! only marks, links, syncs the directory and commits: no write operation waits for a
//! large file to reach the disk.
//!
//! **The time and the marks** are written by the job, through the descriptor, before the
//! file has a name; they are not synced by themselves (the downloader synced the data,
//! and the job syncs the directory after them). The copy keeps its write bit: the folder's
//! write bits are the backing directory's to take off when no mount lies over it.
//!
//! **An undo that fails** (the named file cannot be removed again) leaves the record: the
//! check at start removes the file, which the row does not say. The landing asks for no
//! take-down — it holds no way to — and answers the errno.
//!
//! **On record before the link:** `PlaceChangeRecord::LinkFile { item, place, content }`,
//! `content` the content version that was downloaded, written
//! (`ItemStore::record_place_change`) after the row and its path were read and before the
//! place change is begun. The commit takes the record in its transaction
//! (`commit_landed_content(item, downloaded, record)`, `commit_download_content(item,
//! record)`); on `RowsChanged`
//! the named file is removed again and the record taken by itself. A death between the
//! link and the commit: the check at start says `OnDisk` when the row still has the
//! recorded content (`PlaceChangeRowWrite::ContentOnDisk`), and removes the file when the
//! row has another by now — left there, it would be read as content the user put there.
//!
//! **Tested without the rest**, over the fixture (`item_fixture`):
// test: a_landed_file_is_named_marked_with_id_state_and_ctag_and_then_on_disk_in_its_row
// test: the_directory_of_a_landed_file_is_synced_before_the_row_is_written
// test: a_landing_whose_row_has_another_content_by_now_removes_what_it_named
// test: a_landing_at_the_place_the_row_has_now_follows_a_rename_made_meanwhile
// test: a_landing_of_an_item_the_folder_does_not_show_any_more_is_dropped
// test: a_landing_at_a_place_the_disk_holds_though_the_row_says_not_on_disk_replaces_nothing
// test: a_landing_for_the_plan_asks_every_version_and_one_for_a_read_the_content_alone
// test: a_landing_for_the_plan_says_its_commit_begins_before_anything_is_recorded_or_linked
// test: a_landing_for_a_step_a_stop_aborted_records_links_and_writes_nothing
// test: the_decision_of_a_landing_has_a_case_for_each_row_it_can_meet
// test: a_landing_of_content_the_row_says_is_on_disk_already_links_nothing
// test: a_death_after_the_record_or_after_the_link_is_settled_by_the_check_at_start
#![allow(dead_code)]

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use konedrive_tree::{ContentPresence, ContentVersion, Item, ItemKind, ItemPlace, ItemVersions, LocalItemId, OneDriveItemId, PlaceChangeRecord, PlaceChangeRecordId, StepCommitOutcome, Store, TreeError};

use super::item_content_files::write_whole_copy_marks;
use super::place_changes::{DirectoryMarks, PlaceChange, PlaceChanges, PlaceRefusal, PlaceUndo};
use crate::folder::running_steps::RunningSteps;

/// Who asked for the download that lands, which says what its commit asks of the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LandingCondition {
    /// A read, an open for writing or `Files.Hydrate`: the item's local content must
    /// still be the one that was downloaded (`ItemStore::commit_landed_content`).
    ContentUnchanged { item: LocalItemId, downloaded: ContentVersion },
    /// The plan's `FolderStep::DownloadContent`: every version it was planned from
    /// (`ItemStore::commit_download_content`).
    VersionsUnchanged { item: ItemVersions },
}

impl LandingCondition {
    /// The item the landing is for.
    pub(super) fn item(&self) -> LocalItemId {
        match self {
            LandingCondition::ContentUnchanged { item, .. } => *item,
            LandingCondition::VersionsUnchanged { item } => item.local_item_id,
        }
    }
}

/// What became of a landing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ItemLandingOutcome {
    /// The file is the item's copy and the row says so: the place the row has, and its
    /// path relative to the folder, as they were when the row was written — what the
    /// landing is announced by and the activity log says.
    Landed { place: ItemPlace, path: PathBuf },
    /// The row is not the one the download was for: nothing was written, and what was
    /// named on disk was removed again. The program that waited gets `ESTALE`.
    RowsChanged,
    /// Something is at the item's name on disk, at `path`, though its row says not on
    /// disk (a held row of the check at start, a leftover): nothing is replaced, `EEXIST`.
    PlaceHeld { path: PathBuf },
    /// The disk or the store refused: the errno.
    Failed { errno: i32 },
    /// The plan's step this landing commits as runs no more: a stop of the sync aborted
    /// it before the landing began. Nothing was recorded, linked or written; the next run
    /// of the sync plans the download again.
    StepStopped,
}

/// What a landing does with the row it finds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LandingDecision {
    /// The row is the one the download was for and says not on disk: the file is linked
    /// at `place` as the whole copy of `downloaded`, marked with `onedrive_item_id`.
    Link { place: ItemPlace, onedrive_item_id: OneDriveItemId, downloaded: ContentVersion },
    /// The row says that very content is on disk already: nothing is to do, and whoever
    /// waited has what it asked for.
    AlreadyOnDisk { place: ItemPlace },
    /// The row went, the folder does not show the item, it has no binding, or — by the
    /// condition — its content or any of its versions is another.
    RowsChanged,
}

/// What a landing under `condition` does when the item's row is `row` now. Pure. The
/// plan's landing asks every version before the disk is touched, as its commit does
/// after it; anyone else's the local content alone.
pub(super) fn decide_landing(condition: &LandingCondition, row: Option<&Item>) -> LandingDecision {
    let Some(row) = row.filter(|row| row.kind == ItemKind::File) else { return LandingDecision::RowsChanged };
    let (Some(local), Some(onedrive_item)) = (&row.local, &row.onedrive_item) else { return LandingDecision::RowsChanged };
    let (Some(place), Some(content)) = (&local.place, &local.content) else { return LandingDecision::RowsChanged };
    let is_asked_row = match condition {
        LandingCondition::ContentUnchanged { downloaded, .. } => content == downloaded,
        LandingCondition::VersionsUnchanged { item } => item.are_those_of(row),
    };
    match (is_asked_row, row.content_presence, condition) {
        (false, _, _) => LandingDecision::RowsChanged,
        (true, ContentPresence::NotOnDisk, _) => LandingDecision::Link { place: place.clone(), onedrive_item_id: onedrive_item.item_id.clone(), downloaded: content.clone() },
        (true, ContentPresence::OnDisk, LandingCondition::ContentUnchanged { .. }) => LandingDecision::AlreadyOnDisk { place: place.clone() },
        // A step planned from a copy on disk is no download: its commit refuses it.
        (true, ContentPresence::OnDisk, LandingCondition::VersionsUnchanged { .. }) => LandingDecision::RowsChanged,
    }
}

/// A downloaded file that has its name, on record, and whose row is not written yet.
#[derive(Debug)]
struct LinkedFile {
    place: ItemPlace,
    path: PathBuf,
    record: PlaceChangeRecordId,
    undo: PlaceUndo,
}

/// The landing of one mount's downloads.
pub(super) struct ItemDownloadLanding {
    place_changes: Arc<PlaceChanges>,
    store: Store,
    /// Told when the landing of the plan's download begins to commit: from then on a
    /// stop leaves that step alone.
    running_steps: Arc<RunningSteps>,
}

fn to_errno(error: &std::io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO)
}

impl ItemDownloadLanding {
    /// Made by the mount's build, with the folder's running steps.
    pub(super) fn new(place_changes: Arc<PlaceChanges>, store: Store, running_steps: Arc<RunningSteps>) -> Self {
        Self { place_changes, store, running_steps }
    }

    /// Lands the unnamed file `file`, whole, checked and synced, as the copy of the item
    /// of `condition`. For the plan's download (`VersionsUnchanged`), first of all: its
    /// commit begins (`RunningSteps::begin_commit` of the step's item) — a stop of the
    /// sync aborts the step's task no more, and the plan's loop settles what this landing
    /// answers; told that the step runs no more (a stop aborted it before this job),
    /// nothing is recorded, linked or written: `StepStopped`. Then: one job of the store
    /// reads the row and its path now (the item may
    /// have been renamed during the download, `ItemReader::find_path_with_marks`) and
    /// writes the record (`LinkFile { item, place, content }`), when the row is still the
    /// one the download was for ([`decide_landing`]); a
    /// place change is begun; the file's time is set to the content's and it is marked;
    /// it is linked under its name (`PlaceChange::link_file`: the directories above made
    /// and marked, nothing replaced, the directory synced); then the commit `condition`
    /// names, which takes the record; on `RowsChanged` the undo (the named file removed
    /// again) and the record taken by itself; the change ended.
    ///
    /// Called as a job of the write-operation thread, under the tree lock, by
    /// `ItemDownloader` alone.
    pub(super) fn land(&self, file: &File, condition: &LandingCondition) -> ItemLandingOutcome {
        if let LandingCondition::VersionsUnchanged { item } = condition {
            if !self.running_steps.begin_commit(item.local_item_id) {
                return ItemLandingOutcome::StepStopped;
            }
        }
        match self.link_on_record(file, condition) {
            Ok((change, linked)) => self.commit_linked(&change, linked, condition),
            Err(outcome) => outcome,
        }
    }

    /// The first half of a landing: the row read and judged, the record written, the
    /// place change begun, the file marked and linked. What was linked, with the change
    /// it is linked under; or what the landing answers when nothing is linked — then no
    /// record stands and the disk is as it was.
    fn link_on_record(&self, file: &File, condition: &LandingCondition) -> Result<(PlaceChange<'_>, LinkedFile), ItemLandingOutcome> {
        let fail = |error: TreeError| {
            tracing::warn!("the landing of item {} could not read or record: {error}", condition.item());
            ItemLandingOutcome::Failed { errno: libc::EIO }
        };
        let (item, judged) = (condition.item(), condition.clone());
        // One job: the row, its path, and the record of what is about to be linked.
        let read = self.store.call_blocking(move |store| {
            let row = store.item_store().reader().find_item(item)?;
            let decision = decide_landing(&judged, row.as_ref());
            let Some((path, marks)) = store.item_store().reader().find_path_with_marks(item)? else { return Ok((LandingDecision::RowsChanged, PathBuf::new(), DirectoryMarks::new(), None)) };
            let record = match &decision {
                LandingDecision::Link { place, downloaded, .. } => Some(store.item_store().record_place_change(&PlaceChangeRecord::LinkFile { item, place: place.clone(), content: downloaded.clone() })?),
                LandingDecision::AlreadyOnDisk { .. } | LandingDecision::RowsChanged => None,
            };
            Ok((decision, path, marks, record))
        });
        let (decision, path, marks, record) = read.map_err(fail)?;
        let (place, onedrive_item_id, downloaded, record) = match (decision, record) {
            (LandingDecision::Link { place, onedrive_item_id, downloaded }, Some(record)) => (place, onedrive_item_id, downloaded, record),
            (LandingDecision::AlreadyOnDisk { place }, _) => return Err(ItemLandingOutcome::Landed { place, path }),
            (LandingDecision::RowsChanged, _) | (LandingDecision::Link { .. }, None) => return Err(ItemLandingOutcome::RowsChanged),
        };
        let change = self.place_changes.begin();
        let marked = konedrive_fs::placeholder::set_mtime(file, crate::clock::to_system_time(downloaded.mtime().0)).and_then(|()| write_whole_copy_marks(file, onedrive_item_id.as_str(), &downloaded));
        let linked = match marked {
            Ok(()) => change.link_file(file, &path, &marks),
            Err(error) => Err(PlaceRefusal::Disk { errno: to_errno(&error), path: path.clone() }),
        };
        match linked {
            Ok(undo) => Ok((change, LinkedFile { place, path, record, undo })),
            Err(refusal) => {
                // Nothing was linked: the record says a change that was not made.
                drop(change);
                self.forget_record(item, record);
                Err(match refusal {
                    PlaceRefusal::PlaceHeld { path } => ItemLandingOutcome::PlaceHeld { path },
                    PlaceRefusal::Disk { errno, path } => {
                        tracing::warn!("the downloaded copy of item {item} could not be named {}: errno {errno}", path.display());
                        ItemLandingOutcome::Failed { errno }
                    }
                })
            }
        }
    }

    /// The second half: the commit `condition` names, which takes the record; when it
    /// finds the rows changed, or fails, the named file is removed again and the record
    /// taken by itself. `change` ends with this call.
    fn commit_linked(&self, change: &PlaceChange<'_>, linked: LinkedFile, condition: &LandingCondition) -> ItemLandingOutcome {
        let LinkedFile { place, path, record, undo } = linked;
        let (item, committed_as) = (condition.item(), condition.clone());
        let committed = self.store.call_blocking(move |store| match &committed_as {
            LandingCondition::ContentUnchanged { item, downloaded } => store.item_store().commit_landed_content(*item, downloaded, Some(record)),
            LandingCondition::VersionsUnchanged { item } => store.item_store().commit_download_content(item, Some(record)),
        });
        let refused = match committed {
            Ok(StepCommitOutcome::Written) => return ItemLandingOutcome::Landed { place, path },
            Ok(StepCommitOutcome::RowsChanged) => ItemLandingOutcome::RowsChanged,
            Err(error) => {
                tracing::warn!("the landing of item {item} could not be committed: {error}");
                ItemLandingOutcome::Failed { errno: libc::EIO }
            }
        };
        match change.undo(undo) {
            Ok(()) => {
                self.forget_record(item, record);
                refused
            }
            Err(error) => {
                // The record stands: the check at start removes what the row does not say.
                tracing::error!("the downloaded copy of item {item} at {} is not its row's and could not be removed again: {error}", path.display());
                ItemLandingOutcome::Failed { errno: to_errno(&error) }
            }
        }
    }

    /// The place and the path, relative to the folder, the folder shows `item` at now, on
    /// the store's reading connection; `None` for an item it does not show, and when the
    /// rows cannot be read. Called by the downloader, which holds no store, for what a
    /// download is shown and announced under while it runs.
    pub(super) async fn find_shown_place(&self, item: LocalItemId) -> Option<(ItemPlace, PathBuf)> {
        let read = self.store.read_items(move |reader| {
            let Some(place) = reader.find_item(item)?.and_then(|row| row.local).and_then(|local| local.place) else { return Ok(None) };
            Ok(reader.find_path(item)?.map(|path| (place, path)))
        });
        read.await.unwrap_or_else(|error| {
            tracing::warn!("the place of item {item} could not be read: {error}");
            None
        })
    }

    /// Takes `record` by itself: its change was not made, or was undone.
    fn forget_record(&self, item: LocalItemId, record: PlaceChangeRecordId) {
        if let Err(error) = self.store.call_blocking(move |store| store.item_store().forget_place_change_record(record)) {
            tracing::warn!("the record of the landing of item {item} could not be taken: {error}");
        }
    }
}

#[cfg(test)]
mod tests;
