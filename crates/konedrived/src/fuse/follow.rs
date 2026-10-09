//! The folder under the mount made to follow a cycle of the listing: what changed, and what
//! of it the folder holds.

use std::path::{Path, PathBuf};

use konedrive_tree::{Kind, Placement, Row, Table, TreeStore};

use super::inotify_bridge::RemoteChange;
use super::conflict_copies::CycleConflicts;
use super::detections::rebase;
use super::mount::Mount;
use super::pins::download_pinned_files;
use super::backing_directory::BackingDirectory;
use crate::local::held::Rw;

/// One item a cycle changed: where the store had it and where the new tree has it.
pub struct Step {
    pub id: String,
    pub old: Option<(Row, PathBuf)>,
    pub new: Option<(Row, PathBuf)>,
}

impl Step {
    /// The item is a file whose content is another now.
    pub fn content_changed(&self) -> bool {
        matches!((&self.old, &self.new), (Some((o, _)), Some((n, _)))
            if n.kind == Kind::File && (o.ctag != n.ctag || o.quickxor != n.quickxor || o.size != n.size))
    }
}

/// What following a cycle did.
#[derive(Default)]
pub struct Followed {
    /// The items the folder followed. What a change of the user's holds is not among them:
    /// its change from OneDrive waits.
    pub steps: Vec<Step>,
    /// The user's own files and directories moved out of an item's way: where each stood,
    /// and where it is now.
    pub copied: Vec<(PathBuf, PathBuf)>,
    /// The folders gone from OneDrive that stay here as the user's own, by item.
    pub kept: Vec<String>,
}

/// The directories of the folder under the mount down to `rel`, each marked as its item's,
/// inside a cycle of the listing, for a place of the tree it has staged: on the store's own
/// thread, where the mount's reader would not see it.
fn backing_staged(backing_directory: &BackingDirectory, store: &TreeStore, rel: &Path) -> std::io::Result<()> {
    let staged = |above: Option<&str>, _: &Path, name: &str| {
        let row = store.child(Table::Staging, above?, name).ok().flatten()?;
        (row.placement == Placement::Placed).then_some(row.id)
    };
    backing_directory.make_marked_directories(rel, store.root_item_id().ok().flatten(), staged)
}

impl Mount {
    /// Makes the folder under the mount follow the cycle that `store` has staged and not yet
    /// committed: a downloaded copy whose content is stale or whose item is gone is removed,
    /// and one whose item moved moves with it. What changed is announced by
    /// [`committed`](Self::committed).
    ///
    /// A read-write folder's cycle brings what the outbox holds (`rw`): an item a change of
    /// the user's waits on is left as it is, and what the user made where an item comes or
    /// in a folder that goes is kept ([`super::conflict_copies`]).
    pub fn follow(&self, store: &mut TreeStore, rw: Option<&Rw>) -> Result<Followed, konedrive_tree::TreeError> {
        let placed = |table: Table, id: &str| {
            let row = store.get(table, id).ok().flatten()?;
            let at = store.locate(table, id).ok().flatten()?;
            at.placed.then_some((row, at.rel))
        };
        let held = |id: &str| rw.is_some_and(|rw| rw.held.contains(id) || rw.removing.contains(id));
        let steps: Vec<Step> = store
            .changed_ids()?
            .into_iter()
            .filter(|id| !held(id))
            .map(|id| Step { old: placed(Table::Items, &id), new: placed(Table::Staging, &id), id })
            .collect();
        let s = &self.shared;
        // The copy of item `id` at `rel`, or its directory: a file that is not that item's
        // download is never moved or removed.
        let ours = |rel: &Path, id: &str, kind: Kind| match kind {
            Kind::File => s.backing_directory.has_downloaded_copy(rel, id),
            Kind::Folder => s.backing_directory.stat(rel).is_some_and(|m| m.is_dir()) && s.backing_directory.read_onedrive_id(rel).is_none_or(|carried| carried == id),
        };
        let (mut told, mut unpinned, mut conflicts, mut gone) = (Vec::new(), Vec::new(), CycleConflicts::default(), Vec::new());
        // The time the rows of what is kept are dated by.
        let recorded_at = s.now();
        for step in &steps {
            // Whatever its source remembered of the item is of what it was.
            s.range_readers.forget_file(&step.id);
            match (&step.old, &step.new) {
                (Some((old, was)), None) => {
                    match old.kind {
                        Kind::File => drop(s.backing_directory.remove_downloaded_copy(was, &step.id)),
                        Kind::Folder => gone.push((step.id.clone(), was.clone())),
                    }
                    unpinned.push(step.id.clone());
                    told.push(RemoteChange::NameVanished { rel: was.clone(), kind: old.kind, id: step.id.clone(), parent_id: old.parent_id.clone() });
                }
                (None, Some((new, now))) => {
                    s.conflict_copies.move_aside(now, new.parent_id.as_deref(), recorded_at, &mut conflicts);
                    told.push(RemoteChange::NameAppeared { rel: now.clone(), kind: new.kind, parent_id: new.parent_id.clone() });
                }
                (Some((old, was)), Some((new, now))) => {
                    if was != now {
                        s.conflict_copies.move_aside(now, new.parent_id.as_deref(), recorded_at, &mut conflicts);
                    }
                    if was != now && new.kind == Kind::Folder {
                        // The rows of what the user changed below it are found by place.
                        conflicts.ops.push(rebase(was, now));
                    }
                    if step.content_changed() {
                        s.backing_directory.remove_downloaded_copy(was, &step.id);
                    } else if was != now && ours(was, &step.id, old.kind) && s.backing_directory.stat(now).is_none() {
                        let _ = now.parent().map(|dir| backing_staged(&s.backing_directory, store, dir));
                        let _ = s.backing_directory.rename(was, now);
                    }
                    if was != now {
                        told.push(RemoteChange::NameVanished { rel: was.clone(), kind: old.kind, id: step.id.clone(), parent_id: old.parent_id.clone() });
                        told.push(RemoteChange::NameAppeared { rel: now.clone(), kind: new.kind, parent_id: new.parent_id.clone() });
                    } else if new.kind == Kind::File {
                        told.push(RemoteChange::ContentChanged { rel: now.clone(), id: step.id.clone() });
                    }
                }
                (None, None) => {}
            }
        }
        // The directories of the folders that went, deepest first: one goes once what was
        // below it went.
        gone.sort_by_key(|(_, rel)| std::cmp::Reverse(rel.components().count()));
        for (id, rel) in &gone {
            s.conflict_copies.keep_removed_directory(id, rel, rw.is_some(), recorded_at, &mut conflicts);
        }
        let CycleConflicts { detections, ops, moved_aside: copied, kept_directories: kept } = conflicts;
        if !detections.is_empty() || !ops.is_empty() {
            crate::local::record::record(store, detections, ops, s.now())?;
        }
        // Only this cycle's: what an earlier one left was never committed.
        *crate::panic::lock(&s.pending) = (told, unpinned);
        Ok(Followed { steps, copied, kept })
    }

    /// The cycle [`follow`](Self::follow) followed is committed: its changes are announced
    /// through the mount, and what is pinned and not downloaded is downloaded.
    pub fn committed(&self) {
        let (told, unpinned) = std::mem::take(&mut *crate::panic::lock(&self.shared.pending));
        // An item that is gone keeps nothing on this device.
        let s = &self.shared;
        s.pins.unpin_removed(unpinned);
        s.inotify_bridge.announce(told);
        download_pinned_files(&s.pins, &s.directory_contents, &s.file_downloader, &s.runtime);
        // What the cycle kept of the user's own is the outbox's to send.
        s.outbox_recorder.wake();
    }
}
