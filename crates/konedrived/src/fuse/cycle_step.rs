//! The step of a cycle of the delta listing over the backing directory: what changed in
//! OneDrive, and what of it the backing directory holds.
//!
//! [`apply_staged_cycle`] makes the backing directory follow the cycle a store has staged
//! and not yet committed, and collects what is to be done once it is committed: the changes
//! to announce ([`list_announcements`], a function of one [`OneDriveChange`]) and the
//! OneDrive ids whose pins go. That is the cycle's own state, held by [`UncommittedCycle`]
//! until [`announce_committed_cycle`] takes it. What an earlier cycle left there was never
//! committed, and is replaced.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use konedrive_tree::{Kind, Placement, Row, Table, TreeError, TreeStore};

use super::access_mode_switch::AccessMode;
use super::backing_directory::BackingDirectory;
use super::conflict_copies::{ConflictCopies, CycleConflicts};
use super::detections::rebase;
use super::directory_contents::DirectoryContents;
use super::file_downloader::FileDownloader;
use super::inotify_bridge::{InotifyBridge, Announcement};
use super::outbox_recorder::OutboxRecorder;
use super::pins::{download_pinned_files, Pins};
use super::range_readers::RangeReaders;
use crate::local::held::Rw;

/// One file or directory of OneDrive a cycle changed: where the store had it and where the
/// new tree has it.
pub struct OneDriveChange {
    pub id: String,
    pub old: Option<(Row, PathBuf)>,
    pub new: Option<(Row, PathBuf)>,
}

impl OneDriveChange {
    /// Whether it is a file whose content is another now.
    pub fn has_new_content(&self) -> bool {
        matches!((&self.old, &self.new), (Some((o, _)), Some((n, _)))
            if n.kind == Kind::File && (o.ctag != n.ctag || o.quickxor != n.quickxor || o.size != n.size))
    }
}

/// What one cycle did in the backing directory.
#[derive(Default)]
pub struct CycleOutcome {
    /// What the backing directory followed. What a change of the user's holds is not among
    /// them: its change from OneDrive waits.
    pub changes: Vec<OneDriveChange>,
    /// What the changes found of the user's own in their way.
    pub conflicts: CycleConflicts,
}

/// What a cycle that is not committed yet leaves to be done once it is.
#[derive(Default)]
struct AfterCommit {
    /// What is announced to whoever watches the folder.
    announcements: Vec<Announcement>,
    /// The OneDrive ids gone from the folder: one that is gone keeps nothing on this device,
    /// so its pin is taken off.
    removed_ids: Vec<String>,
}

/// What the cycle now being committed leaves to be done once it is committed.
#[derive(Default)]
pub(super) struct UncommittedCycle(Mutex<AfterCommit>);

impl UncommittedCycle {
    /// A cycle was applied: `announcements` and `removed_ids` are what it leaves. Only this
    /// cycle's: what an earlier one left was never committed.
    pub(super) fn keep(&self, announcements: Vec<Announcement>, removed_ids: Vec<String>) {
        *crate::panic::lock(&self.0) = AfterCommit { announcements, removed_ids };
    }

    /// The cycle is committed: what it left to announce, and the OneDrive ids it removed.
    /// Nothing is left here.
    pub(super) fn take(&self) -> (Vec<Announcement>, Vec<String>) {
        let AfterCommit { announcements, removed_ids } = std::mem::take(&mut *crate::panic::lock(&self.0));
        (announcements, removed_ids)
    }
}

/// What is announced of `change` to whoever watches the folder: a name that vanished, one
/// that appeared, both for a file or directory that moved, and for a file that stayed where
/// it was, that what is shown of it changed.
pub(super) fn list_announcements(change: &OneDriveChange) -> Vec<Announcement> {
    let vanished = |old: &Row, was: &Path| Announcement::NameVanished { rel: was.to_owned(), kind: old.kind, id: change.id.clone(), parent_id: old.parent_id.clone() };
    let appeared = |new: &Row, now: &Path| Announcement::NameAppeared { rel: now.to_owned(), kind: new.kind, parent_id: new.parent_id.clone() };
    match (&change.old, &change.new) {
        (Some((old, was)), None) => vec![vanished(old, was)],
        (None, Some((new, now))) => vec![appeared(new, now)],
        (Some((old, was)), Some((new, now))) if was != now => vec![vanished(old, was), appeared(new, now)],
        (Some(_), Some((new, now))) if new.kind == Kind::File => vec![Announcement::FileShownChanged { rel: now.clone(), id: change.id.clone() }],
        (Some(_), Some(_)) | (None, None) => Vec::new(),
    }
}

/// The directories of the backing directory down to `rel`, each marked with its OneDrive id,
/// inside a cycle of the delta listing, for a place of the tree it has staged: on the store's own
/// thread, where the mount's reader would not see it.
fn make_staged_directories(backing_directory: &BackingDirectory, store: &TreeStore, rel: &Path) -> std::io::Result<()> {
    let staged = |above: Option<&str>, _: &Path, name: &str| {
        let row = store.child(Table::Staging, above?, name).ok().flatten()?;
        (row.placement == Placement::Placed).then_some(row.id)
    };
    backing_directory.make_marked_directories(rel, store.root_item_id().ok().flatten(), staged)
}

/// Makes the backing directory follow the cycle that `store` has staged and not yet
/// committed: a downloaded copy whose content is stale or whose file is gone from OneDrive
/// is removed, and one whose file moved moves with it. What changed is announced by
/// [`announce_committed_cycle`], from what is left in `uncommitted_cycle`.
///
/// A read-write folder's cycle brings what the outbox holds (`rw`): a file or directory a
/// change of the user's waits on is left as it is. What the user made where something of
/// OneDrive comes, or in a directory that goes, is kept ([`super::conflict_copies`]), and
/// in a folder that is read-write by `access_mode` — the mount asked its switch for this
/// cycle — a directory that is kept is made again in OneDrive.
pub(super) fn apply_staged_cycle(
    store: &mut TreeStore,
    rw: Option<&Rw>,
    access_mode: AccessMode,
    backing_directory: &BackingDirectory,
    range_readers: &RangeReaders,
    conflict_copies: &ConflictCopies,
    uncommitted_cycle: &UncommittedCycle,
) -> Result<CycleOutcome, TreeError> {
    let placed = |table: Table, id: &str| {
        let row = store.get(table, id).ok().flatten()?;
        let at = store.locate(table, id).ok().flatten()?;
        at.placed.then_some((row, at.rel))
    };
    let held = |id: &str| rw.is_some_and(|rw| rw.held.contains(id) || rw.removing.contains(id));
    let changes: Vec<OneDriveChange> = store
        .changed_ids()?
        .into_iter()
        .filter(|id| !held(id))
        .map(|id| OneDriveChange { old: placed(Table::Items, &id), new: placed(Table::Staging, &id), id })
        .collect();
    // Whether `rel` holds the downloaded copy of the OneDrive file `id`, or its directory:
    // a file that is not that file's download is never moved or removed.
    let holds = |rel: &Path, id: &str, kind: Kind| match kind {
        Kind::File => backing_directory.has_downloaded_copy(rel, id),
        Kind::Folder => backing_directory.read_metadata(rel).is_some_and(|m| m.is_dir()) && backing_directory.read_onedrive_id(rel).is_none_or(|carried| carried == id),
    };
    let (mut announcements, mut removed_ids, mut conflicts, mut removed_directories) = (Vec::new(), Vec::new(), CycleConflicts::default(), Vec::new());
    // The time the rows of what is kept are dated by.
    let recorded_at = crate::clock::unix_now();
    for change in &changes {
        // Whatever the source of the content remembered of the file is of what it was.
        range_readers.drop_cached_ranges(&change.id);
        match (&change.old, &change.new) {
            (Some((old, was)), None) => {
                match old.kind {
                    Kind::File => drop(backing_directory.remove_downloaded_copy(was, &change.id)),
                    Kind::Folder => removed_directories.push((change.id.clone(), was.clone())),
                }
                removed_ids.push(change.id.clone());
            }
            (None, Some((new, now))) => conflict_copies.move_aside(now, new.parent_id.as_deref(), recorded_at, &mut conflicts),
            (Some((old, was)), Some((new, now))) => {
                if was != now {
                    conflict_copies.move_aside(now, new.parent_id.as_deref(), recorded_at, &mut conflicts);
                }
                if was != now && new.kind == Kind::Folder {
                    // The rows of what the user changed below it are found by place.
                    conflicts.ops.push(rebase(was, now));
                }
                if change.has_new_content() {
                    backing_directory.remove_downloaded_copy(was, &change.id);
                } else if was != now && holds(was, &change.id, old.kind) && backing_directory.read_metadata(now).is_none() {
                    let _ = now.parent().map(|dir| make_staged_directories(backing_directory, store, dir));
                    let _ = backing_directory.rename(was, now);
                }
            }
            (None, None) => {}
        }
        announcements.extend(list_announcements(change));
    }
    // The directories gone from OneDrive, deepest first: one goes once what was below it
    // went.
    removed_directories.sort_by_key(|(_, rel)| std::cmp::Reverse(rel.components().count()));
    for (id, rel) in &removed_directories {
        conflict_copies.keep_removed_directory(id, rel, access_mode, recorded_at, &mut conflicts);
    }
    if !conflicts.detections.is_empty() || !conflicts.ops.is_empty() {
        crate::local::record::record(store, conflicts.detections.clone(), conflicts.ops.clone(), crate::clock::unix_now())?;
    }
    uncommitted_cycle.keep(announcements, removed_ids);
    Ok(CycleOutcome { changes, conflicts })
}

/// The cycle [`apply_staged_cycle`] applied is committed: the pins of what is gone are taken
/// off, its changes are announced through the mount, what is pinned and not downloaded is
/// downloaded, and the outbox's worker is woken.
pub(super) fn announce_committed_cycle(
    uncommitted_cycle: &UncommittedCycle,
    pins: &Arc<Pins>,
    inotify_bridge: &InotifyBridge,
    directory_contents: &Arc<DirectoryContents>,
    file_downloader: &Arc<FileDownloader>,
    runtime: &tokio::runtime::Handle,
    outbox_recorder: &OutboxRecorder,
) {
    let (announcements, removed_ids) = uncommitted_cycle.take();
    // What is gone from OneDrive keeps nothing on this device.
    pins.unpin_removed(removed_ids);
    inotify_bridge.announce(announcements);
    download_pinned_files(pins, directory_contents, file_downloader, runtime);
    // What the cycle kept of the user's own is the outbox's to send.
    outbox_recorder.wake_outbox();
}

#[cfg(test)]
mod tests;
