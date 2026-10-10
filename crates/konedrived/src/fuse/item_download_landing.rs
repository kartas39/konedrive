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
//! **Tested without the rest**, over the fixture (`item_fixture`):
// test: a_landed_file_is_named_marked_with_id_state_and_ctag_and_then_on_disk_in_its_row
// test: the_directory_of_a_landed_file_is_synced_before_the_row_is_written
// test: a_landing_whose_row_has_another_content_by_now_removes_what_it_named
// test: a_landing_at_the_place_the_row_has_now_follows_a_rename_made_meanwhile
// test: a_landing_of_an_item_the_folder_does_not_show_any_more_is_dropped
// test: a_landing_at_a_place_the_disk_holds_though_the_row_says_not_on_disk_replaces_nothing
// test: a_landing_for_the_plan_asks_every_version_and_one_for_a_read_the_content_alone
#![allow(dead_code, unused_variables)]

use std::fs::File;
use std::sync::Arc;

use konedrive_tree::{ContentVersion, ItemVersions, LocalItemId, Store};

use super::place_changes::PlaceChanges;

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
}

/// The landing of one mount's downloads.
pub(super) struct ItemDownloadLanding {
    place_changes: Arc<PlaceChanges>,
    store: Store,
}

impl ItemDownloadLanding {
    /// Made by the mount's build.
    pub(super) fn new(place_changes: Arc<PlaceChanges>, store: Store) -> Self {
        todo!()
    }

    /// Lands the unnamed file `file`, whole, checked and synced, as the copy of the item
    /// of `condition`: one job of the store reads the row and its path now (the item may
    /// have been renamed during the download, `ItemReader::find_path_with_marks`); a
    /// place change is begun; the file's time is set to the content's and it is marked;
    /// it is linked under its name (`PlaceChange::link_file`: the directories above made
    /// and marked, nothing replaced, the directory synced); then the commit `condition`
    /// names; on `RowsChanged` the undo (the named file removed again); the change ended.
    ///
    /// Called as a job of the write-operation thread, under the tree lock, by
    /// `ItemDownloader` alone.
    pub(super) fn land(&self, file: &File, condition: &LandingCondition) -> ItemLandingOutcome {
        todo!()
    }
}
