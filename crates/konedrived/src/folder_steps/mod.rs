//! The runner of the sync plan's steps towards the folder: one function per case of
//! `FolderStep`, each a change of the backing directory and its commit, in the order of
//! design §8, with its undo.
//!
//! **State owned:** none; [`FolderSteps`] holds the parts a step goes through. Every step
//! but a download's fetching runs as one job of the write-operation thread, under the
//! folder's tree lock, as a write operation of the mount does (design §4): no program
//! changes the folder between a step's change of the disk and its row. So steps towards
//! the folder need no limit of their own: the thread runs one job at a time.
//!
//! **Every step, in this order:**
//!
//! 1. ONE job of the store reads the item's row, its path and what the step needs of
//!    other rows, through the writer; the versions the step was planned from must still
//!    be the row's, and no program may hold the file open for writing — asked before the
//!    disk is touched, so that what is left to race with the commit is a listing's page
//!    alone, which does not take the tree lock;
//! 2. the item's former place is taken from that row, for the announcement;
//! 3. the record of what is about to be done to the disk's names
//!    (`ItemStore::record_place_change`, a transaction of its own; none for a step that
//!    changes no name on disk: `ShowItem`, an item with nothing on disk); then
//!    `PlaceChanges::begin`, then the backing directory changed through the place
//!    change, which keeps the undo and replaces nothing the rows do not say;
//! 4. its commit of `step_commit`, with whether the item's new name is ignored
//!    (`UploadRules::is_name_ignored`) where the commit asks, and with the record's id
//!    as its last argument (`record: Option<PlaceChangeRecordId>`, `None` for a step that
//!    wrote none): a commit that writes takes the record in its own transaction; one that
//!    answers `RowsChanged` leaves it;
//! 5. on `RowsChanged` the undo, then the record taken by itself
//!    (`ItemStore::forget_place_change_record`), and `StepOutcome::RowsChanged(Nothing)`;
//!    an undo that fails asks for the mount's take-down, and the record stands for the
//!    check at start; the place change ended; and only then, on `Written`, what was set
//!    aside removed for good (`PlaceChanges::discard`, outside the change: no opener
//!    waits for an unlink);
//! 6. the change announced through the bridge by item and former name; what the download
//!    states and the range fetcher keep of an item that left or changed content
//!    forgotten; what happened answered for the activity log.
//!
//! | Case | Record | Disk | Commit | Order |
//! |---|---|---|---|---|
//! | `ShowItem` | none | nothing | `commit_show_items` | the commit alone, several in one job |
//! | `MoveItem` | `MoveEntry` | move to OneDrive's place | `commit_move_item` | record, disk, commit |
//! | `TakeMoveBack` | `MoveEntry` | move to OneDrive's place | `commit_take_move_back` | record, disk, commit |
//! | `MoveToTemporaryName` | `MoveEntry` | move to the temporary name | `commit_move_to_temporary_name` | record, disk, commit |
//! | `RenameToConflictCopy` | `MoveEntry` | move to the copy's name | `commit_rename_to_conflict_copy_on_record` | record, disk, commit |
//! | `RemoveItem` | `SetAside` | the entry set aside, then removed | `commit_remove_item` | record, set aside, commit, remove |
//! | `TakeOneDriveContent` | `SetAside` | the copy set aside, then removed | `commit_take_onedrive_content` | record, set aside, commit, remove |
//! | `DownloadContent` | `LinkFile` | the file landed | `commit_download_content` | synced, record, named, directory synced, commit |
//!
//! The record is the same for a step and for a program's operation, and does not say whose
//! it is. So the check at start never writes a step's row from it: a move or a set-aside
//! that died before its commit is undone on disk, the rows are as they were, and the plan
//! plans the step again.
//!
//! `TakeOneDriveContent` is NOT "commit, then disk", as a free-up is: its copy is of the
//! OLD content. A death after the commit and before the disk would leave that copy under
//! a row that says not on disk with the NEW content, which the check at start reads as
//! content the user put there, and the old content would be sent over OneDrive's new one.
//! Set aside first and on record, a death before the commit leaves the old row and the
//! record: the check at start puts the old copy back under its name, and the plan takes
//! OneDrive's content again. A death after the commit leaves the new row, which says not
//! on disk, and no record: the copy that was set aside is not put back
//! (`AsideRow::Otherwise`) and goes with the cleaning of what was set aside.
//!
//! **Tested without the rest**, over the fixture (`item_fixture`), with no mount and no
//! OneDrive; and by the races' test of design §12, which puts a failure and a death at
//! every point between a disk change and its commit:
// test: each_case_changes_the_disk_and_the_rows_as_its_line_of_the_table_says
// test: a_step_planned_from_other_versions_than_the_rows_touches_nothing
// test: a_step_for_a_file_a_program_opened_for_writing_since_touches_nothing
// test: a_commit_that_finds_the_rows_changed_undoes_the_disk
// test: an_undo_that_fails_asks_for_the_mounts_take_down
// test: an_item_with_nothing_on_disk_is_moved_in_its_row_alone
// test: a_moved_directory_gets_its_directories_above_made_and_marked
// test: a_place_held_on_disk_by_what_no_row_says_stops_the_step_and_is_said
// test: a_removed_file_is_put_back_when_its_commit_is_refused
// test: a_directory_that_leaves_is_removed_on_disk_only_when_it_is_empty_there
// test: a_death_after_the_copy_of_a_file_that_takes_onedrives_content_was_set_aside_is_not_read_as_an_edit
// test: a_conflict_copy_is_on_record_with_its_commit
// test: the_items_of_several_show_steps_are_committed_in_one_job_each_by_its_own_versions
// test: every_step_announces_the_former_name_and_the_new_one
// test: a_step_that_writes_nothing_announces_nothing
// test: a_death_between_the_disk_and_the_commit_is_undone_by_the_check_at_start_from_the_steps_record
// test: a_commit_that_writes_takes_the_steps_record_and_one_that_finds_the_rows_changed_leaves_it
// test: what_a_download_answered_is_the_steps_outcome_case_by_case
// test: a_pinned_files_download_that_onedrive_refused_once_lands_when_the_step_runs_again
// test: a_step_after_a_take_down_was_asked_for_and_one_whose_thread_stopped_do_not_run
// test: steps_towards_the_folder_run_against_a_programs_writes_and_renames_and_deaths
//!
//! **What an item has on disk** is asked of the disk, on the write-operation thread,
//! where no name changes meanwhile: a directory that is there, and a file that is there
//! and whose row says its content is on disk. A file the row says is not on disk has
//! nothing of its own there: what stands at its path is what no row says, and no step
//! moves, sets aside or removes it.
//!
//! **Two halves.** Every case that changes a name is `prepare` — the rows read and
//! judged, the record, the place change begun, the disk — and `commit_on_record` — the
//! commit with the record, and then the undo or the discard. What a listing's page does
//! between a step's disk and its commit, and what a death leaves there, is between the
//! two.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;

use konedrive_tree::{
    ActivityKind, ConflictKind, ConflictRow, ContentPresence, FolderStep, Item, ItemKind, ItemPlace, ItemVersions, LocalItemId, PlaceChangeRecord, PlaceChangeRecordId, StepCommitOutcome, Store, TreeError, TreeStore,
};

use crate::mount::{DirectoryMarks, DownloadRefusal, EntryKind, ItemAnnouncement, ItemDownloadStates, ItemDownloader, ItemHandles, ItemInotifyBridge, ItemLandingOutcome, ItemRangeFetcher, ItemToDownload, MountTakeDown, PlaceChange, PlaceChanges, PlaceRefusal, PlaceUndo, RangeSource, RowPathStat, TakeDownCause, UploadRules, WriteOperationThread, describe_move};
use crate::folder::disk::make_aside_entry_name;
use crate::sync_parts::step_outcome::{RowsChangedFollowUp, StepEvent, StepFailure, StepObstacle, StepOutcome, StepRefusal};

/// Why a step does not touch the folder, asked before the disk is changed; `None` when it
/// may go on. Pure. `row` is the item's row as it is now.
pub(super) fn decide_folder_step_obstacle(item: &ItemVersions, row: Option<&Item>, is_open_for_writing: bool) -> Option<StepOutcome> {
    match row {
        Some(row) if item.are_those_of(row) => is_open_for_writing.then_some(StepOutcome::NotRun(StepObstacle::OpenForWriting)),
        Some(_) | None => Some(StepOutcome::RowsChanged(RowsChangedFollowUp::Nothing)),
    }
}

/// What the plan's step for a download is answered, by how its download ended. Pure.
pub(super) fn decide_download_outcome(end: Result<ItemLandingOutcome, DownloadRefusal>) -> StepOutcome {
    match end {
        Ok(ItemLandingOutcome::Landed { path, .. }) => StepOutcome::Written { events: vec![StepEvent { kind: ActivityKind::Downloaded, path, former_path: None }] },
        Ok(ItemLandingOutcome::RowsChanged) => StepOutcome::RowsChanged(RowsChangedFollowUp::Nothing),
        Ok(ItemLandingOutcome::PlaceHeld { path }) => StepOutcome::NotRun(StepObstacle::PlaceHeldOnDisk { path }),
        Ok(ItemLandingOutcome::Failed { errno }) => StepOutcome::Failed(StepFailure::Disk { errno, path: PathBuf::new() }),
        Ok(ItemLandingOutcome::StepStopped) => StepOutcome::NotRun(StepObstacle::SendingStopped),
        Err(DownloadRefusal::SignedOut) => StepOutcome::Refused(StepRefusal::SignedOut),
        Err(DownloadRefusal::Throttled { retry_after }) => StepOutcome::Refused(StepRefusal::Throttled { retry_after }),
        Err(DownloadRefusal::ItemGone) => StepOutcome::Refused(StepRefusal::ItemGone),
        // OneDrive has another version of the item than the step was planned from.
        Err(DownloadRefusal::AnotherContent) => StepOutcome::Refused(StepRefusal::ItemChanged),
        Err(DownloadRefusal::Transient { detail }) => StepOutcome::Refused(StepRefusal::Transient { detail }),
    }
}

/// The order in which the items of several show steps are committed: a directory before
/// what OneDrive has in it, and otherwise the order given. The positions in `items`.
/// Pure.
pub(super) fn order_shown_items(items: &[ItemVersions]) -> Vec<usize> {
    let position_of: BTreeMap<LocalItemId, usize> = items.iter().enumerate().map(|(position, item)| (item.local_item_id, position)).collect();
    let directory_of = |position: usize| items[position].onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()).and_then(|place| position_of.get(&place.parent_local_item_id)).copied();
    // How many directories of the batch are above each item; bounded, should rows ever say a circle.
    let depth_of = |position: usize| std::iter::successors(directory_of(position), |above| directory_of(*above)).take(items.len()).count();
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|position| depth_of(*position));
    order
}

fn fail_store(error: &TreeError) -> StepOutcome {
    StepOutcome::Failed(StepFailure::Store { detail: error.to_string() })
}

fn rows_changed() -> StepOutcome {
    StepOutcome::RowsChanged(RowsChangedFollowUp::Nothing)
}

/// What a step read of the rows before it touched the disk: the item's row, which has
/// the versions the step was planned from, and where the folder shows it.
#[derive(Debug, Clone)]
struct StepRows {
    row: Item,
    /// The place of the item's local version.
    place: ItemPlace,
    /// Its path, relative to the folder.
    path: PathBuf,
    /// For a step that moves: the path of the place it moves to, and the marks of the
    /// directories above it.
    destination: Option<(PathBuf, DirectoryMarks)>,
}

/// A step's change of the backing directory, made and not committed.
#[derive(Debug)]
struct DiskChange {
    rows: StepRows,
    /// `None` for a step that changed no name: an item with nothing on disk.
    record: Option<PlaceChangeRecordId>,
    undo: PlaceUndo,
}

/// The commit of a step, as one job of the store: given the record of the step's change,
/// when it wrote one.
type StepCommit = Box<dyn FnOnce(&mut TreeStore, Option<PlaceChangeRecordId>) -> Result<StepCommitOutcome, TreeError> + Send>;

/// Cancels the plan's download when the step's future is dropped before its end.
struct DownloadCancel {
    downloader: Arc<ItemDownloader>,
    file: ItemToDownload,
    is_ended: bool,
}

impl Drop for DownloadCancel {
    fn drop(&mut self) {
        if !self.is_ended {
            self.downloader.cancel(self.file.item, &self.file.content);
        }
    }
}

/// The runner of the steps towards the folder, for one mount.
pub struct FolderSteps {
    place_changes: Arc<PlaceChanges>,
    /// The writer: a step's reads and its commit are jobs of the store's thread.
    store: Store,
    write_operation_thread: Arc<WriteOperationThread>,
    upload_rules: Arc<UploadRules>,
    item_handles: Arc<ItemHandles>,
    inotify_bridge: Arc<ItemInotifyBridge>,
    downloader: Arc<ItemDownloader>,
    download_states: Arc<ItemDownloadStates>,
    range_fetcher: Arc<ItemRangeFetcher>,
    mount_take_down: Arc<MountTakeDown>,
    /// The folder's path, which is the mount: a conflict's record holds full paths.
    folder: PathBuf,
}

impl FolderSteps {
    /// Made by the start of the folder's sync (`sync_tasks::start_order`), of the parts the
    /// mount hands out for it (`ItemMount::folder_step_parts`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        place_changes: Arc<PlaceChanges>,
        store: Store,
        write_operation_thread: Arc<WriteOperationThread>,
        upload_rules: Arc<UploadRules>,
        item_handles: Arc<ItemHandles>,
        inotify_bridge: Arc<ItemInotifyBridge>,
        downloader: Arc<ItemDownloader>,
        download_states: Arc<ItemDownloadStates>,
        range_fetcher: Arc<ItemRangeFetcher>,
        mount_take_down: Arc<MountTakeDown>,
        folder: PathBuf,
    ) -> Self {
        Self { place_changes, store, write_operation_thread, upload_rules, item_handles, inotify_bridge, downloader, download_states, range_fetcher, mount_take_down, folder }
    }

    /// Announces that what the mount answers as the sync mark of each of `items` changed
    /// (`ItemAnnouncement::MarksChanged` of the item at the place the folder shows it at
    /// now, read in one read of the rows; an item the folder does not show is passed
    /// over): Dolphin reads the emblem again on it. The mark itself is not kept anywhere:
    /// it is answered from `RunningSteps` and `PublishedPlan` (`item_lookup::decide_sync_mark`),
    /// so whoever changes those says so here — the plan's task, and nobody else: after it
    /// began or settled a step towards OneDrive (the items of the step), and after a
    /// published plan, for the items whose answer turned to or from what the mark shows
    /// as blocked. Never waits for the kernel: the bridge's queue takes them. The pin's
    /// change is announced by who sets it (`ItemFilesInterface::set_pin`).
    pub fn announce_marks_changed(&self, items: Vec<LocalItemId>) {
        // After a take-down was asked the folder is on its way down: nobody is told of marks.
        if items.is_empty() || self.mount_take_down.is_asked() {
            return;
        }
        let read = move |rows: &konedrive_tree::ItemReader<'_>| {
            let mut announcements = Vec::new();
            for item in items {
                // The root has no place, and an item the folder does not show no local version: neither has a name to touch.
                if let Some(place) = rows.find_item(item)?.and_then(|row| row.local).and_then(|local| local.place) {
                    announcements.push(ItemAnnouncement::MarksChanged { item, place });
                }
            }
            Ok(announcements)
        };
        // Each item is a change of its own to the bridge: none is held up by another's.
        let announce = |inotify_bridge: &ItemInotifyBridge, announcements: Result<Vec<ItemAnnouncement>, TreeError>| match announcements {
            Ok(announcements) => announcements.into_iter().for_each(|announcement| inotify_bridge.announce(vec![announcement])),
            // Not a warning: the read of a task nobody waits for fails in an ordinary stop, when the store is closed under it.
            Err(error) => tracing::debug!("cannot read the places of the items whose sync mark changed: {error}"),
        };
        // The caller, the plan's loop, waits neither for the read nor for the kernel: the read is a task of its own on the store's reading connection, kept and awaited by nobody. It may so run after the folder's sync stopped: it writes nothing, a store that is closed by then fails its read, and the bridge takes no announcement after its stop (`ItemInotifyBridge::announce`).
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                let (store, inotify_bridge) = (self.store.clone(), Arc::clone(&self.inotify_bridge));
                runtime.spawn(async move { announce(&inotify_bridge, store.read_items(read).await) });
            }
            // Called off the runtime: the read is made here.
            Err(_) => announce(&self.inotify_bridge, self.store.read_items_blocking(read)),
        }
    }

    /// Runs `step` and answers what came of it: total over the cases. A `ShowItem` goes
    /// through [`run_show_items`](Self::run_show_items) with the one item, a
    /// `DownloadContent` through [`run_download_content`](Self::run_download_content);
    /// every other case is one job of the write-operation thread. Called by the plan's
    /// task (`sync_plan_task`), which holds the folder's lease while it runs (and
    /// calls the two named functions itself for the show steps of a plan and for a
    /// download). A job dropped because the thread stopped, and any step after a
    /// take-down was asked for, is `StepOutcome::NotRun(SendingStopped)`.
    pub async fn run(self: &Arc<Self>, step: FolderStep) -> StepOutcome {
        match step {
            FolderStep::ShowItem { item } => self.run_show_items(vec![item]).await.pop().unwrap_or(StepOutcome::NotRun(StepObstacle::SendingStopped)),
            FolderStep::DownloadContent { item } => self.run_download_content(item).await,
            step => {
                let steps = Arc::clone(self);
                self.write_operation_thread.run_and_wait(move || steps.run_as_job(&step)).await.unwrap_or(StepOutcome::NotRun(StepObstacle::SendingStopped))
            }
        }
    }

    /// One step that changes a name, as the job of the write-operation thread it is.
    fn run_as_job(&self, step: &FolderStep) -> StepOutcome {
        if self.mount_take_down.is_asked() {
            return StepOutcome::NotRun(StepObstacle::SendingStopped);
        }
        match step {
            FolderStep::MoveItem { item } => self.run_move_item(item),
            FolderStep::TakeMoveBack { item } => self.run_take_move_back(item),
            FolderStep::MoveToTemporaryName { item, place } => self.run_move_to_temporary_name(item, place),
            FolderStep::RenameToConflictCopy { item, name } => self.run_rename_to_conflict_copy(item, name),
            FolderStep::RemoveItem { item } => self.run_remove_item(item),
            FolderStep::TakeOneDriveContent { item } => self.run_take_onedrive_content(item),
            // The two cases that are no such job: `run` sends them their own way.
            FolderStep::ShowItem { .. } | FolderStep::DownloadContent { .. } => StepOutcome::NotRun(StepObstacle::SendingStopped),
        }
    }

    /// `ShowItem`, for every such step of one plan: the items of OneDrive the folder does
    /// not show appear at OneDrive's places, their content not on disk. Nothing on disk:
    /// a directory of OneDrive needs none, and a file's copy is not there. One job of the
    /// write-operation thread, in it one job of the store
    /// (`ItemStore::commit_show_items`, each item with whether its name is ignored, a
    /// directory before what is in it — the items are put in that order here,
    /// [`order_shown_items`]); each answered by itself, in the order given;
    /// `Appeared` announced for those written. Design §6, "new in OneDrive". A first
    /// listing of N items is a few jobs, not N.
    pub async fn run_show_items(self: &Arc<Self>, items: Vec<ItemVersions>) -> Vec<StepOutcome> {
        let (steps, count) = (Arc::clone(self), items.len());
        let shown = self.write_operation_thread.run_and_wait(move || steps.show_items(&items)).await;
        shown.unwrap_or_else(|_| vec![StepOutcome::NotRun(StepObstacle::SendingStopped); count])
    }

    /// [`run_show_items`](Self::run_show_items), as its job.
    fn show_items(&self, items: &[ItemVersions]) -> Vec<StepOutcome> {
        if self.mount_take_down.is_asked() {
            return vec![StepOutcome::NotRun(StepObstacle::SendingStopped); items.len()];
        }
        let order = order_shown_items(items);
        let name_of = |item: &ItemVersions| item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref()).map(|place| place.name.clone());
        let ordered: Vec<(ItemVersions, bool)> = order.iter().map(|position| (items[*position].clone(), name_of(&items[*position]).is_some_and(|name| self.upload_rules.is_name_ignored(&name)))).collect();
        let committed = self.store.call_blocking(move |store| {
            let item_store = &mut store.item_store();
            // A step no plan asks for refuses the whole call: each is then asked by itself.
            let committed = match item_store.commit_show_items(&ordered) {
                Ok(committed) => committed.into_iter().map(Ok).collect(),
                Err(_) => ordered.iter().map(|(item, is_name_ignored)| item_store.commit_show_item(item, *is_name_ignored)).collect::<Vec<_>>(),
            };
            // Where each one that was written is shown now.
            let reader = item_store.reader();
            let mut shown = Vec::with_capacity(committed.len());
            for ((item, _), committed) in ordered.iter().zip(committed) {
                let place = match &committed {
                    Ok(StepCommitOutcome::Written) => reader.find_item(item.local_item_id)?.and_then(|row| row.local).and_then(|local| local.place).zip(reader.find_path(item.local_item_id)?),
                    Ok(StepCommitOutcome::RowsChanged) | Err(_) => None,
                };
                shown.push((committed.map_err(|error| error.to_string()), place));
            }
            Ok(shown)
        });
        let shown = match committed {
            Ok(shown) => shown,
            Err(error) => return vec![fail_store(&error); items.len()],
        };
        let mut outcomes = vec![rows_changed(); items.len()];
        let mut announcements = Vec::new();
        for (position, (committed, place)) in order.into_iter().zip(shown) {
            outcomes[position] = match (committed, place) {
                (Ok(StepCommitOutcome::Written), Some((place, path))) => {
                    announcements.push(ItemAnnouncement::Appeared { item: items[position].local_item_id, place });
                    StepOutcome::Written { events: vec![StepEvent { kind: ActivityKind::Added, path, former_path: None }] }
                }
                (Ok(_), _) => rows_changed(),
                (Err(detail), _) => StepOutcome::Failed(StepFailure::Store { detail }),
            };
        }
        if !announcements.is_empty() {
            self.inotify_bridge.announce(announcements);
        }
        outcomes
    }

    /// `DownloadContent`: the content of a pinned file that is not on disk is downloaded.
    /// Not a job of the write-operation thread while it fetches: the download runs on the
    /// runtime in the pool's download slot, which the downloader takes
    /// (`ItemDownloader::download_and_land` with the step's versions; a download of the
    /// same content that runs already is joined), and only its landing is such a job
    /// (`ItemDownloadLanding::land`, which calls `commit_download_content`). The source's
    /// refusal is answered as `StepRefusal::{SignedOut, Throttled, ItemGone, ItemChanged,
    /// Transient}` ([`decide_download_outcome`]). `ItemChanged` is OneDrive stating
    /// another content than the row's: the settlement asks for a cycle and plans the
    /// step again, and the next download of the content asks OneDrive anew
    /// (`ItemDownloadStates::start_or_join` forgets what the last one was answered).
    /// When this future is dropped (the step's task aborted), a guard cancels the
    /// download (`ItemDownloader::cancel`: aborted when the plan alone asked for it). The plan's task runs it as its own class of step
    /// (`StepClass::Download`), held by the pause and the sign-out, never holding back
    /// the other steps towards the folder. Replaces `pinned_file_downloader.rs`.
    pub async fn run_download_content(self: &Arc<Self>, item: ItemVersions) -> StepOutcome {
        if self.mount_take_down.is_asked() {
            return StepOutcome::NotRun(StepObstacle::SendingStopped);
        }
        // A step for what is no file of OneDrive the folder shows was planned from other rows.
        let (Some(onedrive_item), Some(content)) = (item.onedrive_item.clone(), item.local.as_ref().and_then(|local| local.content.clone())) else { return rows_changed() };
        let file = ItemToDownload { item: item.local_item_id, onedrive_item, content };
        let mut cancel = DownloadCancel { downloader: Arc::clone(&self.downloader), file: file.clone(), is_ended: false };
        let end = self.downloader.download_and_land(file, item).await;
        cancel.is_ended = true;
        decide_download_outcome(end)
    }

    /// `MoveItem`: the item is moved here to OneDrive's place. `move_on_disk`, then
    /// `commit_move_item(item, is_name_ignored, record)`. Announces the former name vanishing and
    /// the new one appearing. Replaces the cycle step's rename (`cycle_step.rs:210`) and
    /// the upload worker's `follow_cloud` (`upload/steps/shared.rs:547`).
    fn run_move_item(&self, item: &ItemVersions) -> StepOutcome {
        let Some(place) = item.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone()) else { return rows_changed() };
        let (versions, is_name_ignored) = (item.clone(), self.upload_rules.is_name_ignored(&place.name));
        self.run_move(item, &place, ActivityKind::Moved, Box::new(move |store, record| store.item_store().commit_move_item(&versions, is_name_ignored, record)))
    }

    /// `TakeMoveBack`: a move made here is taken back to OneDrive's place. As
    /// [`run_move_item`](Self::run_move_item), with `commit_take_move_back`. Design §6, "a
    /// cycle of moves".
    fn run_take_move_back(&self, item: &ItemVersions) -> StepOutcome {
        let Some(place) = item.onedrive.as_ref().and_then(|onedrive| onedrive.place.clone()) else { return rows_changed() };
        let (versions, is_name_ignored) = (item.clone(), self.upload_rules.is_name_ignored(&place.name));
        self.run_move(item, &place, ActivityKind::Restored, Box::new(move |store, record| store.item_store().commit_take_move_back(&versions, is_name_ignored, record)))
    }

    /// `MoveToTemporaryName`: the item is moved here to `place`, a temporary name. As
    /// [`run_move_item`](Self::run_move_item) to `place`, with
    /// `commit_move_to_temporary_name(item, place, is_name_ignored, record)`: a temporary name
    /// (`.konedrive-swap-<id>`) is the row's place, with its file under that name on
    /// disk; the mount hides it (`folder::disk::is_hidden_name`), and the check at start
    /// reads it as that row's — it is no entry set aside.
    fn run_move_to_temporary_name(&self, item: &ItemVersions, place: &ItemPlace) -> StepOutcome {
        let (versions, to, is_name_ignored) = (item.clone(), place.clone(), self.upload_rules.is_name_ignored(&place.name));
        self.run_move(item, place, ActivityKind::Moved, Box::new(move |store, record| store.item_store().commit_move_to_temporary_name(&versions, &to, is_name_ignored, record)))
    }

    /// `RenameToConflictCopy`: the user's own item is renamed here to `name` in the
    /// directory it is in. `move_on_disk`, then
    /// `commit_rename_to_conflict_copy_on_record(item, name, is_name_ignored, conflict, record)`
    /// with the two full paths. Replaces `upload/steps/shared.rs:389` (`copy`), `:454`
    /// (`move_to_copy_name`) and `conflict_copies.rs:106` (`move_aside`). The name was
    /// chosen free on both sides by the plan; one that is held on disk by what no row
    /// says is `StepObstacle::PlaceHeldOnDisk`. The conflict's time is the clock's
    /// (`clock::unix_now`): this part is given none.
    fn run_rename_to_conflict_copy(&self, item: &ItemVersions, name: &OsStr) -> StepOutcome {
        let Some(directory) = item.local.as_ref().and_then(|local| local.place.as_ref()).map(|place| place.parent_local_item_id) else { return rows_changed() };
        let place = ItemPlace { parent_local_item_id: directory, name: name.to_owned() };
        let (change, moved) = match self.prepare_move(item, &place) {
            Ok(prepared) => prepared,
            Err(outcome) => return outcome,
        };
        let full_path = |path: &PathBuf| self.folder.join(path).display().to_string();
        let to = moved.rows.destination.as_ref().map(|(to, _)| to.clone()).unwrap_or_default();
        let conflict = ConflictRow { at: crate::clock::unix_now(), original: full_path(&moved.rows.path), rescued: full_path(&to), kind: ConflictKind::Copy };
        let (versions, name, is_name_ignored) = (item.clone(), name.to_owned(), self.upload_rules.is_name_ignored(name));
        let commit: StepCommit = Box::new(move |store, record| store.item_store().commit_rename_to_conflict_copy_on_record(&versions, &name, is_name_ignored, &conflict, record));
        self.commit_move(change, moved, &place, ActivityKind::Conflict, commit)
    }

    /// The four cases that move: the item is moved on disk to `place` and its commit
    /// written; what happened is told as `kind`.
    fn run_move(&self, item: &ItemVersions, place: &ItemPlace, kind: ActivityKind, commit: StepCommit) -> StepOutcome {
        match self.prepare_move(item, place) {
            Ok((change, moved)) => self.commit_move(change, moved, place, kind, commit),
            Err(outcome) => outcome,
        }
    }

    /// The first half of a case that moves: the rows read and judged, the record
    /// (`MoveEntry`, none for an item with nothing on disk), the place change begun, the
    /// entry moved ([`move_on_disk`](Self::move_on_disk)).
    fn prepare_move(&self, item: &ItemVersions, place: &ItemPlace) -> Result<(PlaceChange<'_>, DiskChange), StepOutcome> {
        let rows = self.read_step_rows(item, Some(place))?;
        let record = match self.is_on_disk(&rows) {
            true => Some(self.record(PlaceChangeRecord::MoveEntry { item: item.local_item_id, from: rows.place.clone(), to: place.clone() })?),
            false => None,
        };
        let change = self.place_changes.begin();
        match self.move_on_disk(&change, &rows, record.is_some()) {
            Ok(undo) => Ok((change, DiskChange { rows, record, undo })),
            Err(outcome) => {
                drop(change);
                self.forget_record(record);
                Err(outcome)
            }
        }
    }

    /// The second half of a case that moves: its commit, and the two names announced.
    fn commit_move(&self, change: PlaceChange<'_>, moved: DiskChange, place: &ItemPlace, kind: ActivityKind, commit: StepCommit) -> StepOutcome {
        let rows = match self.commit_on_record(change, moved, commit) {
            Ok(rows) => rows,
            Err(outcome) => return outcome,
        };
        let (id, to) = (rows.row.local_item_id, rows.destination.map(|(to, _)| to).unwrap_or_default());
        self.inotify_bridge.announce(describe_move(id, rows.row.kind, rows.place, place.clone()));
        StepOutcome::Written { events: vec![StepEvent { kind, path: to, former_path: Some(rows.path) }] }
    }

    /// `RemoveItem`: the folder shows the item no more. On record (`SetAside { item, place,
    /// aside_name }`), then `PlaceChange::set_aside` of what it has on disk, then
    /// `commit_remove_item`, which takes the record; undo: restore (the entry, or the mark
    /// of a directory that holds what no row says and stays), and the record taken by
    /// itself; after the change ended, `PlaceChanges::discard`.
    /// Announces the name vanishing; the download states and the ranges of the item are
    /// forgotten. A directory the folder shows something in is not touched: its commit
    /// would find the rows changed.
    fn run_remove_item(&self, item: &ItemVersions) -> StepOutcome {
        let (change, set_aside) = match self.prepare_set_aside(item, true) {
            Ok(prepared) => prepared,
            Err(outcome) => return outcome,
        };
        let versions = item.clone();
        let rows = match self.commit_on_record(change, set_aside, Box::new(move |store, record| store.item_store().commit_remove_item(&versions, record))) {
            Ok(rows) => rows,
            Err(outcome) => return outcome,
        };
        self.forget_content(&rows.row);
        self.inotify_bridge.announce(vec![ItemAnnouncement::Vanished { item: item.local_item_id, former_place: rows.place, kind: rows.row.kind }]);
        StepOutcome::Written { events: vec![StepEvent { kind: ActivityKind::Removed, path: rows.path, former_path: None }] }
    }

    /// `TakeOneDriveContent`: the file shows OneDrive's content from now on. On record
    /// (`SetAside { item, place, aside_name }`), the copy of the old content set aside
    /// (`PlaceChange::set_aside`), then `commit_take_onedrive_content`, which takes the
    /// record; undo: restore, and the record taken by itself; after the change ended,
    /// `PlaceChanges::discard` (see the module's doc for why not the commit first). Announces the content changed; the ranges the
    /// content source remembers of the item (`ItemRangeFetcher::forget_ranges`; today the
    /// cycle step, for every cycle: `cycle_step.rs:126`) and its download states are
    /// forgotten.
    fn run_take_onedrive_content(&self, item: &ItemVersions) -> StepOutcome {
        let (change, set_aside) = match self.prepare_set_aside(item, false) {
            Ok(prepared) => prepared,
            Err(outcome) => return outcome,
        };
        let versions = item.clone();
        let rows = match self.commit_on_record(change, set_aside, Box::new(move |store, record| store.item_store().commit_take_onedrive_content(&versions, record))) {
            Ok(rows) => rows,
            Err(outcome) => return outcome,
        };
        self.forget_content(&rows.row);
        self.inotify_bridge.announce(vec![ItemAnnouncement::ContentChanged { item: item.local_item_id, place: rows.place }]);
        StepOutcome::Written { events: vec![StepEvent { kind: ActivityKind::Updated, path: rows.path, former_path: None }] }
    }

    /// The first half of the two cases that set aside: the rows read and judged, the
    /// record (`SetAside`, none for an item with nothing on disk), the place change
    /// begun, what the item has on disk set aside under its aside name.
    /// `is_item_leaving` says the step removes the item: a directory the folder still
    /// shows something in is then not touched.
    fn prepare_set_aside(&self, item: &ItemVersions, is_item_leaving: bool) -> Result<(PlaceChange<'_>, DiskChange), StepOutcome> {
        let rows = self.read_step_rows(item, None)?;
        if is_item_leaving && rows.row.kind == ItemKind::Directory && self.shows_something_in(item.local_item_id)? {
            return Err(rows_changed());
        }
        let record = match self.is_on_disk(&rows) {
            true => Some(self.record(PlaceChangeRecord::SetAside { item: item.local_item_id, place: rows.place.clone(), aside_name: make_aside_entry_name(item.local_item_id) })?),
            false => None,
        };
        let change = self.place_changes.begin();
        let set_aside = match record {
            Some(_) => change.set_aside(&rows.path, item.local_item_id),
            None => Ok(PlaceUndo::Nothing),
        };
        match set_aside {
            Ok(undo) => Ok((change, DiskChange { rows, record, undo })),
            Err(refusal) => {
                drop(change);
                self.forget_record(record);
                Err(self.describe_refusal(refusal))
            }
        }
    }

    /// Step 1 of every case: ONE job of the store reads the item's row, its path, and
    /// for a move the path of the place it moves to with the marks of the directories
    /// above it; the versions the step was planned from must be the row's, and no program
    /// may hold the file open for writing ([`decide_folder_step_obstacle`]). What it
    /// read; or what the step answers with nothing touched.
    fn read_step_rows(&self, item: &ItemVersions, destination: Option<&ItemPlace>) -> Result<StepRows, StepOutcome> {
        let (id, to) = (item.local_item_id, destination.cloned());
        let read = self.store.call_blocking(move |store| {
            let item_store = store.item_store();
            let reader = item_store.reader();
            let destination = match &to {
                Some(place) => reader.find_path_with_marks(place.parent_local_item_id)?.map(|(directory, marks)| (directory.join(&place.name), marks)),
                None => None,
            };
            Ok((reader.find_item(id)?, reader.find_path(id)?, destination))
        });
        let (row, path, found_destination) = read.map_err(|error| fail_store(&error))?;
        if let Some(obstacle) = decide_folder_step_obstacle(item, row.as_ref(), self.item_handles.is_open_for_writing(id)) {
            return Err(obstacle);
        }
        // A step for an item the folder does not show, or to a directory it does not
        // show, is no step of these rows: its commit would say so.
        let (Some(row), Some(path)) = (row, path) else { return Err(rows_changed()) };
        let Some(place) = row.local.as_ref().and_then(|local| local.place.clone()) else { return Err(rows_changed()) };
        if destination.is_some() && found_destination.is_none() {
            return Err(rows_changed());
        }
        Ok(StepRows { row, place, path, destination: found_destination })
    }

    /// Whether the item of `rows` has something of its own in the backing directory (the
    /// module's doc). Asked on the write-operation thread, before the record.
    fn is_on_disk(&self, rows: &StepRows) -> bool {
        let path = rows.path.clone();
        let Ok(RowPathStat::Facts { facts, .. }) = self.place_changes.stat_at_row_path(move || Ok(Some((path.clone(), ())))) else { return false };
        match rows.row.kind {
            ItemKind::Directory => facts.kind == EntryKind::Directory,
            ItemKind::File => facts.kind == EntryKind::File && rows.row.content_presence == ContentPresence::OnDisk,
        }
    }

    /// Whether the folder shows anything in the directory `directory`.
    fn shows_something_in(&self, directory: LocalItemId) -> Result<bool, StepOutcome> {
        self.store.call_blocking(move |store| Ok(!store.item_store().reader().list_children(directory)?.is_empty())).map_err(|error| fail_store(&error))
    }

    /// Writes the record of what a step is about to do to the disk's names, as a job of
    /// the store, before the place change begins.
    fn record(&self, record: PlaceChangeRecord) -> Result<PlaceChangeRecordId, StepOutcome> {
        self.store.call_blocking(move |store| store.item_store().record_place_change(&record)).map_err(|error| fail_store(&error))
    }

    /// Takes `record` by itself: its change was not made, or was undone.
    fn forget_record(&self, record: Option<PlaceChangeRecordId>) {
        let Some(record) = record else { return };
        if let Err(error) = self.store.call_blocking(move |store| store.item_store().forget_place_change_record(record)) {
            tracing::warn!("the record {record} of a step's change of the backing directory could not be taken: {error}");
        }
    }

    /// What a step answers when the backing directory refused its change.
    fn describe_refusal(&self, refusal: PlaceRefusal) -> StepOutcome {
        match refusal {
            PlaceRefusal::PlaceHeld { path } => StepOutcome::NotRun(StepObstacle::PlaceHeldOnDisk { path }),
            PlaceRefusal::Disk { errno, path } => StepOutcome::Failed(StepFailure::Disk { errno, path }),
            // No change a step makes sets an entry aside for a move (`move_entry_over` is
            // the mount's rename alone): answered as an undo that failed is.
            PlaceRefusal::NotPutBack { path, .. } => {
                self.mount_take_down.ask(TakeDownCause::UndoFailed { path: path.clone() });
                StepOutcome::Failed(StepFailure::UndoFailed { path })
            }
        }
    }

    /// Moves what the item of `rows` has on disk from the path of its local version to
    /// the path of the place it moves to (`PlaceChange::move_entry`, with the directories
    /// above made and marked by what `ItemReader::find_path_with_marks` of the directory
    /// answered). The caller wrote the record before it began `change` (`MoveEntry {
    /// item, from: the item's local place, to: place }`; none for an item with nothing on
    /// disk, which `is_on_disk` says), and gives the record's id to its commit.
    /// Answers how to take it back; `PlaceUndo::Nothing` for an item with nothing on
    /// disk; `StepObstacle::PlaceHeldOnDisk` as the outcome for a held place. Called by
    /// the four cases that move.
    fn move_on_disk(&self, change: &PlaceChange<'_>, rows: &StepRows, is_on_disk: bool) -> Result<PlaceUndo, StepOutcome> {
        let Some((to, marks)) = rows.destination.as_ref().filter(|_| is_on_disk) else { return Ok(PlaceUndo::Nothing) };
        change.move_entry(&rows.path, to, marks).map_err(|refusal| self.describe_refusal(refusal))
    }

    /// Steps 4 and 5 of every case that changes a name: `commit`, given the record; when
    /// it wrote, the change ended and what was set aside removed for good, and the rows
    /// the step read answered for its announcement; when it found the rows changed, or
    /// failed, the disk taken back ([`undo_on_disk`](Self::undo_on_disk)) and the step's
    /// outcome answered.
    fn commit_on_record(&self, change: PlaceChange<'_>, disk_change: DiskChange, commit: StepCommit) -> Result<StepRows, StepOutcome> {
        let DiskChange { rows, record, undo } = disk_change;
        let refused = match self.store.call_blocking(move |store| commit(store, record)) {
            Ok(StepCommitOutcome::Written) => {
                change.end();
                self.place_changes.discard(undo);
                return Ok(rows);
            }
            Ok(StepCommitOutcome::RowsChanged) => rows_changed(),
            Err(error) => fail_store(&error),
        };
        self.undo_on_disk(&change, undo, record)?;
        Err(refused)
    }

    /// Takes `undo` back through `change`, and then the step's record by itself
    /// (`ItemStore::forget_place_change_record`); when the undo fails, asks for the mount's
    /// take-down, leaves the record standing and answers `StepFailure::UndoFailed`. Called
    /// after a commit answered `RowsChanged` or failed.
    fn undo_on_disk(&self, change: &PlaceChange<'_>, undo: PlaceUndo, record: Option<PlaceChangeRecordId>) -> Result<(), StepOutcome> {
        let path = match &undo {
            PlaceUndo::Nothing => PathBuf::new(),
            PlaceUndo::RemoveMade { path, .. } | PlaceUndo::UnmarkTaken { path } | PlaceUndo::Restore { path, .. } | PlaceUndo::RestoreMark { path, .. } => path.clone(),
            PlaceUndo::MoveBack { to, .. } | PlaceUndo::MoveBackAndRestore { to, .. } => to.clone(),
        };
        match change.undo(undo) {
            Ok(()) => {
                self.forget_record(record);
                Ok(())
            }
            Err(error) => {
                tracing::error!("a step's change of the backing directory at {} could not be taken back: {error}", path.display());
                self.mount_take_down.ask(TakeDownCause::UndoFailed { path: path.clone() });
                Err(StepOutcome::Failed(StepFailure::UndoFailed { path }))
            }
        }
    }

    /// What the download states and the range fetcher keep of the item of `row`, which
    /// left or shows another content, is forgotten.
    fn forget_content(&self, row: &Item) {
        self.download_states.forget_item(row.local_item_id);
        if let (Some(onedrive_item), Some(content)) = (row.onedrive_item.clone(), row.local.as_ref().and_then(|local| local.content.clone())) {
            self.range_fetcher.forget_ranges(&RangeSource { onedrive_item, content });
        }
    }
}

#[cfg(test)]
mod tests;
