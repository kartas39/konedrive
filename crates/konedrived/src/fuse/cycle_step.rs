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
//!    what the change does, and the step performs that in order. The directories gone from
//!    OneDrive are removed at the end, deepest first. What was kept of the user's own is
//!    recorded through the [`OutboxRecorder`], in the cycle's own transaction.
//! 2. [`CycleStep::announce_committed_cycle`], once the cycle is committed: the pins of what
//!    is gone are taken off, the changes are announced through the mount, what is pinned and
//!    not downloaded is downloaded, and the outbox's worker is woken.
//!
//! What an earlier cycle left uncommitted was never committed, and is replaced.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use konedrive_tree::{Kind, Placement, Row, Table, TreeError, TreeStore};

use super::access_mode_switch::{AccessMode, AccessModeSwitch};
use super::backing_directory::BackingDirectory;
use super::conflict_copies::{ConflictCopies, CycleConflicts, MoveAsideOutcome};
use super::detections::{is_directory_of, rebase, OutboxBatch};
use super::directory_contents::DirectoryContents;
use super::inotify_bridge::{Announcement, InotifyBridge};
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
}

pub struct CycleStep {
    backing_directory: Arc<BackingDirectory>,
    directory_contents: Arc<DirectoryContents>,
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
        access_mode_switch: Arc<AccessModeSwitch>,
        range_fetcher: Arc<RangeFetcher>,
        conflict_copies: ConflictCopies,
        outbox_recorder: Arc<OutboxRecorder>,
        pins: Arc<Pins>,
        inotify_bridge: Arc<InotifyBridge>,
        pinned_file_downloader: Arc<PinnedFileDownloader>,
    ) -> Self {
        CycleStep { backing_directory, directory_contents, access_mode_switch, range_fetcher, conflict_copies, outbox_recorder, pins, inotify_bridge, pinned_file_downloader, uncommitted_cycle: Mutex::default() }
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
        }
        let StepProgress { mut conflicts, removed_directories, announcements } = progress;
        for directory in order_deepest_first(removed_directories) {
            let outcome = self.conflict_copies.remove_directory_if_empty(&directory.onedrive_id, &directory.path, access_mode, recorded_at, &mut conflicts);
            tracing::debug!("{}, which OneDrive removed: {outcome:?}", directory.path.display());
        }
        // What the cycle kept of the user's own, in the cycle's own transaction.
        let kept = OutboxBatch { detections: conflicts.detections.clone(), ops: conflicts.ops.clone() };
        self.outbox_recorder.record_in(store, kept, crate::clock::unix_now())?;
        *crate::panic::lock(&self.uncommitted_cycle) = UncommittedCycle { announcements, removed_ids: list_removed_ids(&changes) };
        Ok(CycleOutcome { changes, conflicts })
    }

    /// The cycle [`apply_staged_cycle`](Self::apply_staged_cycle) applied is committed: the
    /// pins of what is gone are taken off, its changes are announced through the mount, what
    /// is pinned and not downloaded is downloaded, and the outbox's worker is woken.
    pub fn announce_committed_cycle(&self) {
        let UncommittedCycle { announcements, removed_ids } = std::mem::take(&mut *crate::panic::lock(&self.uncommitted_cycle));
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
    fn perform(&self, store: &TreeStore, actions: Vec<CycleAction>, access_mode: AccessMode, recorded_at: i64, progress: &mut StepProgress) {
        let mut moved_aside = MoveAsideOutcome::NothingInTheWay;
        for action in actions {
            match action {
                CycleAction::MoveLocalOnlyEntryAside { path, parent_id } => moved_aside = self.conflict_copies.move_aside(&path, parent_id.as_deref(), access_mode, recorded_at, &mut progress.conflicts),
                CycleAction::RebaseOutboxRows { from, to } => progress.conflicts.ops.push(rebase(&from, &to)),
                CycleAction::RemoveDownloadedCopy { path, onedrive_id } => drop(self.backing_directory.remove_downloaded_copy(&path, &onedrive_id)),
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
    let placed = |table: Table, id: &str| -> Option<(Row, PathBuf)> {
        let row = store.get(table, id).ok().flatten()?;
        let at = store.locate(table, id).ok().flatten()?;
        at.placed.then_some((row, at.rel))
    };
    changed_ids.into_iter().filter_map(|id| OneDriveChange::of(id.clone(), placed(Table::Items, &id), placed(Table::Staging, &id))).collect()
}

#[cfg(test)]
pub(super) mod tests;
