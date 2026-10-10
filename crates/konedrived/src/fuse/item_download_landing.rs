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
#![allow(dead_code, unused_variables)]

use std::fs::File;
use std::sync::Arc;

use konedrive_tree::{ContentVersion, ItemVersions, LocalItemId, Store};

use super::place_changes::PlaceChanges;
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

/// What became of a landing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ItemLandingOutcome {
    /// The file is the item's copy and the row says so.
    Landed,
    /// The row is not the one the download was for: nothing was written, and what was
    /// named on disk was removed again. The program that waited gets `ESTALE`.
    RowsChanged,
    /// Something is at the item's name on disk though its row says not on disk (a held
    /// row of the check at start, a leftover): nothing is replaced, `EEXIST`.
    PlaceHeld,
    /// The disk or the store refused: the errno.
    Failed { errno: i32 },
    /// The plan's step this landing commits as runs no more: a stop of the sync aborted
    /// it before the landing began. Nothing was recorded, linked or written; the next run
    /// of the sync plans the download again.
    StepStopped,
}

/// The landing of one mount's downloads.
pub(super) struct ItemDownloadLanding {
    place_changes: Arc<PlaceChanges>,
    store: Store,
    /// Told when the landing of the plan's download begins to commit: from then on a
    /// stop leaves that step alone.
    running_steps: Arc<RunningSteps>,
}

impl ItemDownloadLanding {
    /// Made by the mount's build, with the folder's running steps.
    pub(super) fn new(place_changes: Arc<PlaceChanges>, store: Store, running_steps: Arc<RunningSteps>) -> Self {
        todo!()
    }

    /// Lands the unnamed file `file`, whole, checked and synced, as the copy of the item
    /// of `condition`. For the plan's download (`VersionsUnchanged`), first of all: its
    /// commit begins (`RunningSteps::begin_commit` of the step's item) — a stop of the
    /// sync aborts the step's task no more, and the plan's loop settles what this landing
    /// answers; told that the step runs no more (a stop aborted it before this job),
    /// nothing is recorded, linked or written: `StepStopped`. Then: one job of the store
    /// reads the row and its path now (the item may
    /// have been renamed during the download, `ItemReader::find_path_with_marks`); the
    /// record is written (`LinkFile { item, place, content }`); a
    /// place change is begun; the file's time is set to the content's and it is marked;
    /// it is linked under its name (`PlaceChange::link_file`: the directories above made
    /// and marked, nothing replaced, the directory synced); then the commit `condition`
    /// names, which takes the record; on `RowsChanged` the undo (the named file removed
    /// again) and the record taken by itself; the change ended.
    ///
    /// Called as a job of the write-operation thread, under the tree lock, by
    /// `ItemDownloader` alone.
    pub(super) fn land(&self, file: &File, condition: &LandingCondition) -> ItemLandingOutcome {
        todo!()
    }
}
