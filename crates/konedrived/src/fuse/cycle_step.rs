//! The step of a cycle of the delta listing over the backing directory: what changed in
//! OneDrive, and what of it the backing directory holds.
//!
//! [`CycleStep`] owns the parts the step uses and one piece of state, what the cycle now
//! being committed leaves to be done once it is ([`UncommittedCycle`]). A cycle calls it
//! twice:
//!
//! 1. [`CycleStep::apply_staged_cycle`], inside the transaction the cycle has staged and not
//!    yet committed: each change is read as an [`OneDriveChange`], what the backing directory
//!    holds at its paths is read ([`BackingDirectoryFacts`]), [`list_cycle_actions`] decides
//!    what the change does, and the step performs that in order. A downloaded copy that is
//!    to be removed and that a program holds open for writing is not removed: it is moved
//!    aside as the user's own, a conflict copy ([`CycleStep::keep_written_copy`]) — in
//!    either access mode the cycle holds the folder's tree lock, so no file is opened for
//!    writing or closed while the step runs. The directories gone from
//!    OneDrive are removed at the end, deepest first. What was kept of the user's own is
//!    recorded through the [`OutboxRecorder`], in the cycle's own transaction, and with it
//!    what the step did in the folder is written to the rows of `items`, the disk first: an
//!    item of the user's own moved aside has its new name, a written copy and a kept
//!    directory give their bindings up, and what is gone from OneDrive and from the
//!    backing directory is removed from the folder. Each is written only where the rows
//!    still say what was there before, so a step that runs again after a commit that
//!    failed writes nothing twice. What the cycle's commit cannot know of the disk, which
//!    files have their whole copy there, is kept for it ([`CycleStep::list_copies_on_disk`]).
//! 2. [`CycleStep::announce_committed_cycle`], once the cycle is committed: the pins of what
//!    is gone are taken off, the changes are announced through the mount, what is pinned and
//!    not downloaded is downloaded, and the outbox's worker is woken.
//!
//! What an earlier cycle left uncommitted was never committed, and is replaced.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use konedrive_tree::{DriveId, Item, ItemPlace, Kind, LocalItemId, LocalVersionChange, OneDriveItemId, OneDriveItemReference, Placement, Row, Table, TreeError, TreeStore};

use super::access_mode_switch::{AccessMode, AccessModeSwitch};
use super::backing_directory::BackingDirectory;
use super::conflict_copies::{ConflictCopies, CycleConflicts, MoveAsideOutcome, RemovedDirectoryOutcome};
use super::detections::{is_directory_of, rebase, OutboxBatch, WriteState};
use super::directory_contents::DirectoryContents;
use super::inotify_bridge::{Announcement, InotifyBridge};
use super::node_resolver::NodeResolver;
use super::open_handles::OpenHandles;
use super::outbox_recorder::OutboxRecorder;
use super::pinned_file_downloader::PinnedFileDownloader;
use super::pins::Pins;
use super::range_fetcher::RangeFetcher;
use crate::local::held::Rw;

mod cycle_actions;
mod onedrive_change;
mod uncommitted_cycle;

use cycle_actions::{list_cycle_actions, order_deepest_first, BackingDirectoryFacts, CycleAction, NewPathContents, RemovedDirectory};
pub use onedrive_change::OneDriveChange;
use uncommitted_cycle::{list_removed_ids, UncommittedCycle};

/// What one cycle did in the backing directory.
#[derive(Default)]
pub struct CycleOutcome {
    /// What the backing directory followed. What a change of the user's holds is not among
    /// them: its change from OneDrive waits.
    pub changes: Vec<OneDriveChange>,
    /// What the changes found of the user's own in their way.
    pub conflicts: CycleConflicts,
}

/// What performing the changes of one cycle collects for the end of the step and for the
/// commit.
#[derive(Default)]
struct StepProgress {
    conflicts: CycleConflicts,
    removed_directories: Vec<RemovedDirectory>,
    announcements: Vec<Announcement>,
    /// The items that give their bindings up in the rows of `items`, each with what the
    /// upload rules say of its name: a written copy, and a directory that stays.
    given_up: Vec<(LocalItemId, bool)>,
    /// What the step did in the folder, for the rows: moved aside, and removed.
    row_changes: Vec<LocalVersionChange>,
    /// The files whose whole copy the backing directory holds at the place the cycle has
    /// them at.
    copies_on_disk: Vec<String>,
}

pub struct CycleStep {
    backing_directory: Arc<BackingDirectory>,
    directory_contents: Arc<DirectoryContents>,
    /// Which node stands for a OneDrive id, and what moves that node with a copy that is
    /// moved aside.
    node_resolver: Arc<NodeResolver>,
    /// Whether a program holds a node open for writing: a copy that has a writer is never
    /// removed.
    open_handles: Arc<OpenHandles>,
    /// Whether what is kept of the user's own is recorded, to be sent: in a read-write
    /// folder alone.
    access_mode_switch: Arc<AccessModeSwitch>,
    /// What drops the ranges the source remembered of a file that changed.
    range_fetcher: Arc<RangeFetcher>,
    /// What moves the user's own out of the way of a change.
    conflict_copies: ConflictCopies,
    /// What records what was kept of the user's own, and wakes the outbox's worker.
    outbox_recorder: Arc<OutboxRecorder>,
    pins: Arc<Pins>,
    inotify_bridge: Arc<InotifyBridge>,
    pinned_file_downloader: Arc<PinnedFileDownloader>,
    /// What the cycle now being committed leaves to be done once it is.
    uncommitted_cycle: Mutex<UncommittedCycle>,
}

impl CycleStep {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        backing_directory: Arc<BackingDirectory>,
        directory_contents: Arc<DirectoryContents>,
        node_resolver: Arc<NodeResolver>,
        open_handles: Arc<OpenHandles>,
        access_mode_switch: Arc<AccessModeSwitch>,
        range_fetcher: Arc<RangeFetcher>,
        conflict_copies: ConflictCopies,
        outbox_recorder: Arc<OutboxRecorder>,
        pins: Arc<Pins>,
        inotify_bridge: Arc<InotifyBridge>,
        pinned_file_downloader: Arc<PinnedFileDownloader>,
    ) -> Self {
        CycleStep { backing_directory, directory_contents, node_resolver, open_handles, access_mode_switch, range_fetcher, conflict_copies, outbox_recorder, pins, inotify_bridge, pinned_file_downloader, uncommitted_cycle: Mutex::default() }
    }

    /// Makes the backing directory follow the cycle that `store` has staged and not yet
    /// committed: a downloaded copy whose content is stale or whose file is gone from
    /// OneDrive is removed, and one whose file moved moves with it. What changed is announced
    /// by [`announce_committed_cycle`](Self::announce_committed_cycle).
    ///
    /// A read-write folder's cycle brings what the outbox holds (`rw`): a file or directory a
    /// change of the user's waits on is left as it is. What the user made where something of
    /// OneDrive comes, or in a directory that goes, is kept ([`super::conflict_copies`])
    /// whatever the access mode; it is recorded, to be sent, in a folder that is read-write
    /// alone — a read-only folder's cycle leaves no row in the outbox.
    pub fn apply_staged_cycle(&self, store: &mut TreeStore, rw: Option<&Rw>) -> Result<CycleOutcome, TreeError> {
        let changed_ids = list_changed_ids(store, rw)?;
        // Whatever the source of the content remembered of a file is of what it was.
        changed_ids.iter().for_each(|id| self.range_fetcher.drop_cached_ranges(id));
        let changes = list_changes(store, changed_ids);
        let access_mode = self.access_mode_switch.access_mode();
        // The time the rows of what is kept are dated by.
        let recorded_at = crate::clock::unix_now();
        let mut progress = StepProgress::default();
        for change in &changes {
            let facts = self.read_backing_directory_facts(change);
            self.perform(store, list_cycle_actions(change, &facts), access_mode, recorded_at, &mut progress);
            self.note_file_for_rows(store, change, &mut progress);
        }
        let StepProgress { mut conflicts, removed_directories, announcements, mut given_up, mut row_changes, copies_on_disk } = progress;
        for directory in order_deepest_first(removed_directories) {
            let outcome = self.conflict_copies.remove_directory_if_empty(&directory.onedrive_id, &directory.path, access_mode, recorded_at, &mut conflicts);
            tracing::debug!("{}, which OneDrive removed: {outcome:?}", directory.path.display());
            // The disk first, the row second: only a row that is still bound and shown.
            let row = find_shown_bound_item(store, &directory.onedrive_id);
            match (outcome, row, directory.path.file_name().and_then(|name| name.to_str())) {
                (RemovedDirectoryOutcome::Removed, Some(row), _) => row_changes.push(LocalVersionChange::Removed { item: row.local_item_id }),
                (RemovedDirectoryOutcome::KeptAsLocalOnly, Some(row), Some(name)) => given_up.push((row.local_item_id, self.conflict_copies.is_name_ignored(name))),
                (RemovedDirectoryOutcome::Removed | RemovedDirectoryOutcome::KeptAsLocalOnly | RemovedDirectoryOutcome::LeftForOtherDirectory, ..) => {}
            }
        }
        // What the cycle kept of the user's own, in the cycle's own transaction, and what the
        // step did in the folder, in the rows.
        let kept = OutboxBatch { detections: conflicts.detections.clone(), ops: conflicts.ops.clone() };
        let refused = self.outbox_recorder.record_step_in(store, kept, &given_up, &row_changes, crate::clock::unix_now())?;
        if refused > 0 {
            tracing::warn!("{refused} change(s) of the folder that a cycle made were refused by the rows of items: they did not say what the backing directory held");
        }
        *crate::panic::lock(&self.uncommitted_cycle) = UncommittedCycle { announcements, removed_ids: list_removed_ids(&changes), copies_on_disk };
        Ok(CycleOutcome { changes, conflicts })
    }

    /// The files whose whole copy the backing directory holds, as the step last applied
    /// found them: what the commit of that cycle writes as on disk in the rows of `items`.
    pub fn list_copies_on_disk(&self) -> Vec<String> {
        crate::panic::lock(&self.uncommitted_cycle).copies_on_disk.clone()
    }

    /// What the rows of `items` need of the file `change` is about, once the step did what
    /// the change asks. A file that is gone from OneDrive, and from the backing directory
    /// now, is removed from the folder in the rows, unless it gave its binding up (a
    /// written copy). A file that is in the folder is noted when its whole copy is at its
    /// place; so is every file below a directory that appears, which no change names when
    /// its listing came before the directory's.
    fn note_file_for_rows(&self, store: &mut TreeStore, change: &OneDriveChange, progress: &mut StepProgress) {
        let mut note_copy = |onedrive_id: &str, path: &Path| {
            if self.directory_contents.has_downloaded_copy_at(path, onedrive_id) {
                progress.copies_on_disk.push(onedrive_id.to_owned());
            }
        };
        match change {
            OneDriveChange::Vanished { onedrive_id, old, .. } if old.kind == Kind::File => {
                if let Some(row) = find_shown_bound_item(store, onedrive_id).filter(|row| !progress.given_up.iter().any(|(given_up, _)| *given_up == row.local_item_id)) {
                    progress.row_changes.push(LocalVersionChange::Removed { item: row.local_item_id });
                }
            }
            OneDriveChange::Vanished { .. } => {}
            OneDriveChange::Appeared { onedrive_id, new, now } | OneDriveChange::Moved { onedrive_id, new, now, .. } if new.kind == Kind::File => note_copy(onedrive_id, now),
            OneDriveChange::Stayed { onedrive_id, new, at, .. } if new.kind == Kind::File => note_copy(onedrive_id, at),
            OneDriveChange::Appeared { onedrive_id, .. } => {
                for below in store.descendants(Table::Staging, onedrive_id).unwrap_or_default() {
                    if let Some((_, path)) = find_placed(store, Table::Staging, &below).filter(|(row, _)| row.kind == Kind::File) {
                        note_copy(&below, &path);
                    }
                }
            }
            OneDriveChange::Moved { .. } | OneDriveChange::Stayed { .. } => {}
        }
    }

    /// The cycle [`apply_staged_cycle`](Self::apply_staged_cycle) applied is committed: the
    /// pins of what is gone are taken off, its changes are announced through the mount, what
    /// is pinned and not downloaded is downloaded, and the outbox's worker is woken.
    pub fn announce_committed_cycle(&self) {
        let UncommittedCycle { announcements, removed_ids, copies_on_disk: _ } = std::mem::take(&mut *crate::panic::lock(&self.uncommitted_cycle));
        // What is gone from OneDrive keeps nothing on this device.
        self.pins.unpin_removed(removed_ids);
        self.inotify_bridge.announce(announcements);
        self.pinned_file_downloader.download_pinned_files();
        // What the cycle kept of the user's own is the outbox's to send.
        self.outbox_recorder.wake_outbox();
    }

    /// What the backing directory holds at the paths of `change`, for the decision of what
    /// the change does. The old path is the one the store had the file at, which the mount
    /// need not show it at: no entry's sync state says what it holds, and the directory
    /// contents are asked about the path.
    fn read_backing_directory_facts(&self, change: &OneDriveChange) -> BackingDirectoryFacts {
        match change {
            OneDriveChange::Appeared { now, .. } => BackingDirectoryFacts { holds_at_old_path: false, new_path: self.read_new_path_contents(now) },
            OneDriveChange::Moved { onedrive_id, old, was, now, .. } => BackingDirectoryFacts { holds_at_old_path: self.holds_at(was, onedrive_id, old.kind), new_path: self.read_new_path_contents(now) },
            OneDriveChange::Vanished { .. } | OneDriveChange::Stayed { .. } => BackingDirectoryFacts::default(),
        }
    }

    /// Whether `path` holds the downloaded copy of the OneDrive file `id`, or its directory:
    /// a file that is not that file's download is never moved.
    fn holds_at(&self, path: &Path, id: &str, kind: Kind) -> bool {
        match kind {
            Kind::File => self.directory_contents.has_downloaded_copy_at(path, id),
            Kind::Folder => self.backing_directory.read_metadata(path).is_some_and(|m| m.is_dir()) && is_directory_of(self.backing_directory.read_onedrive_id(path).as_deref(), id),
        }
    }

    fn read_new_path_contents(&self, path: &Path) -> NewPathContents {
        if self.backing_directory.read_metadata(path).is_none() {
            NewPathContents::Nothing
        } else if self.backing_directory.find_unmarked_disk_entry(path).is_some() {
            NewPathContents::UnmarkedEntry
        } else {
            NewPathContents::OtherEntry
        }
    }

    /// Performs `actions`, the actions of one change, in their order; what is left for the
    /// end of the step and for the commit goes into `progress`. `store` is the cycle's own,
    /// with the new tree staged.
    fn perform(&self, store: &mut TreeStore, actions: Vec<CycleAction>, access_mode: AccessMode, recorded_at: i64, progress: &mut StepProgress) {
        let mut moved_aside = MoveAsideOutcome::NothingInTheWay;
        for action in actions {
            match action {
                CycleAction::MoveLocalOnlyEntryAside { path, parent_id } => {
                    moved_aside = self.conflict_copies.move_aside(&path, parent_id.as_deref(), WriteState::NotOpen, access_mode, recorded_at, &mut progress.conflicts);
                    // The disk first, the row second: the row of the user's own that was there.
                    let own = store.item_store().find_item_by_path(&path).ok().flatten().filter(|row| row.onedrive_item.is_none());
                    if let (MoveAsideOutcome::MovedAside, Some(own)) = (moved_aside, own) {
                        progress.row_changes.extend(self.describe_move_aside(&own, &progress.conflicts));
                    }
                }
                CycleAction::RebaseOutboxRows { from, to } => progress.conflicts.ops.push(rebase(&from, &to)),
                CycleAction::RemoveDownloadedCopy { path, onedrive_id } => match self.find_write_state(&onedrive_id) {
                    WriteState::NotOpen => drop(self.backing_directory.remove_downloaded_copy(&path, &onedrive_id)),
                    // Nobody removes a copy that has a writer.
                    WriteState::OpenForWriting => self.keep_written_copy(store, &path, &onedrive_id, access_mode, recorded_at, progress),
                },
                // The user's own is still at the new path: nothing is moved onto it.
                CycleAction::MoveInBackingDirectory { .. } if moved_aside == MoveAsideOutcome::StillInTheWay => {}
                CycleAction::MoveInBackingDirectory { from, to } => {
                    let _ = to.parent().map(|directory| self.make_staged_directories(store, directory));
                    let _ = self.backing_directory.rename(&from, &to);
                }
                CycleAction::RemoveDirectoryIfEmpty { onedrive_id, path } => progress.removed_directories.push(RemovedDirectory { onedrive_id, path }),
                CycleAction::Announce(announcement) => progress.announcements.push(announcement),
            }
        }
    }

    /// Whether a program holds the OneDrive file `onedrive_id` open for writing now: the
    /// node that stands for it, by the node table, has a writer. Read under the folder's
    /// tree lock, which the cycle holds: no file is opened for writing or closed meanwhile.
    fn find_write_state(&self, onedrive_id: &str) -> WriteState {
        match self.node_resolver.find_node_id(onedrive_id).is_some_and(|node_id| self.open_handles.is_open_for_writing_by_node_id(node_id)) {
            true => WriteState::OpenForWriting,
            false => WriteState::NotOpen,
        }
    }

    /// The downloaded copy of the OneDrive file `onedrive_id` at `path`, which a program
    /// writes, is not removed with the content, or the file, the cycle takes: it is moved
    /// aside as the user's own ([`ConflictCopies::move_written_copy_aside`]), in the
    /// directory `store` has the file in, and the node the program holds it by goes with it.
    /// A copy that could not be moved stays at its path, unmarked: the user's own, which no
    /// download replaces.
    ///
    /// In the rows of `items` the copy's item gives its binding up, whether or not the copy
    /// could be moved: it keeps its id, which the program holds the file by, and is the
    /// user's own from here on; and it has the conflict copy's name when it was moved.
    fn keep_written_copy(&self, store: &mut TreeStore, path: &Path, onedrive_id: &str, access_mode: AccessMode, recorded_at: i64, progress: &mut StepProgress) {
        let parent_id = store.get(Table::OneDriveItems, onedrive_id).ok().flatten().and_then(|row| row.parent_id);
        let handle = self.backing_directory.find_file_handle(path);
        let conflicts = &mut progress.conflicts;
        let outcome = self.conflict_copies.move_written_copy_aside(path, onedrive_id, parent_id.as_deref(), access_mode, recorded_at, conflicts);
        let written = find_shown_bound_item(store, onedrive_id);
        if let (MoveAsideOutcome::MovedAside | MoveAsideOutcome::StillInTheWay, Some(written), Some(name)) = (outcome, written, path.file_name().and_then(|name| name.to_str())) {
            progress.given_up.push((written.local_item_id, self.conflict_copies.is_name_ignored(name)));
            if outcome == MoveAsideOutcome::MovedAside {
                progress.row_changes.extend(self.describe_move_aside(&written, conflicts));
            }
        }
        let copy_name = conflicts.moved_aside.last().and_then(|moved_aside| moved_aside.copy.file_name()).and_then(|name| name.to_str());
        match (outcome, handle, copy_name) {
            (MoveAsideOutcome::MovedAside, Some(handle), Some(copy_name)) => self.node_resolver.move_node_onto_local_only_entry(onedrive_id, &handle, copy_name),
            (MoveAsideOutcome::StillInTheWay, ..) => tracing::warn!("{}, which a program writes, could not be moved aside: it stays there as the user's own", path.display()),
            (MoveAsideOutcome::MovedAside | MoveAsideOutcome::NothingInTheWay, ..) => {}
        }
    }

    /// The change of the row `own`, an item the step just moved aside: it is in the
    /// directory it was in, under the name of the conflict copy made last, with what the
    /// upload rules say of that name. `None` when the name cannot be read.
    fn describe_move_aside(&self, own: &Item, conflicts: &CycleConflicts) -> Option<LocalVersionChange> {
        let copy_name = conflicts.moved_aside.last()?.copy.file_name()?;
        let directory = own.local.as_ref()?.place.as_ref()?.parent_local_item_id;
        let is_name_ignored = self.conflict_copies.is_name_ignored(copy_name.to_str()?);
        Some(LocalVersionChange::Moved { item: own.local_item_id, place: ItemPlace { parent_local_item_id: directory, name: copy_name.to_owned() }, is_name_ignored })
    }

    /// The directories of the backing directory down to `rel`, each marked with its OneDrive
    /// id, for a place of the tree the cycle has staged: asked of the cycle's own `store`,
    /// where the mount's reader would not see it.
    fn make_staged_directories(&self, store: &TreeStore, rel: &Path) -> std::io::Result<()> {
        let staged = |above: Option<&str>, _: &Path, name: &str| {
            let row = store.child(Table::Staging, above?, name).ok().flatten()?;
            (row.placement == Placement::Placed).then_some(row.id)
        };
        self.backing_directory.make_marked_directories(rel, store.root_item_id().ok().flatten(), staged)
    }
}

/// The OneDrive ids the cycle `store` has staged changes, without what the outbox holds
/// (`rw`): a file or directory a change of the user's waits on is left as it is.
fn list_changed_ids(store: &TreeStore, rw: Option<&Rw>) -> Result<Vec<String>, TreeError> {
    let held = |id: &str| rw.is_some_and(|rw| rw.held.contains(id) || rw.removing.contains(id));
    Ok(store.changed_ids()?.into_iter().filter(|id| !held(id)).collect())
}

/// The change of each of `changed_ids`, from where `store` had it placed and where the new
/// tree it has staged has it placed. An id that is placed in neither is no change of the
/// folder.
fn list_changes(store: &TreeStore, changed_ids: Vec<String>) -> Vec<OneDriveChange> {
    changed_ids.into_iter().filter_map(|id| OneDriveChange::of(id.clone(), find_placed(store, Table::OneDriveItems, &id), find_placed(store, Table::Staging, &id))).collect()
}

/// The row of the OneDrive id `id` in `table` and its path in the folder, when the table
/// places it there.
fn find_placed(store: &TreeStore, table: Table, id: &str) -> Option<(Row, PathBuf)> {
    let row = store.get(table, id).ok().flatten()?;
    let at = store.locate(table, id).ok().flatten()?;
    at.placed.then_some((row, at.rel))
}

/// The row of `items` that is bound to the OneDrive id `onedrive_id`, when the folder
/// shows it: what the step changes in the rows, and only while they still say so.
fn find_shown_bound_item(store: &mut TreeStore, onedrive_id: &str) -> Option<Item> {
    let drive_id = store.drive_id().ok().flatten().and_then(DriveId::new)?;
    let onedrive_item = OneDriveItemReference { drive_id, item_id: OneDriveItemId::new(onedrive_id) };
    store.item_store().find_bound_item(&onedrive_item).ok().flatten().filter(|row| row.local.is_some())
}

#[cfg(test)]
pub(super) mod tests;
