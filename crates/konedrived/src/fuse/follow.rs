//! The folder under the mount made to follow a cycle of the listing: what changed, and what
//! of it the folder holds.

use std::fs;
use std::path::{Path, PathBuf};

use konedrive_tree::{Kind, Row, Table, TreeStore};

use super::announce::Announce;
use super::conflict::Met;
use super::copies::remove_copy;
use super::local::carried;
use super::mount::Mount;
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

impl Mount {
    /// Makes the folder under the mount follow the cycle that `store` has staged and not yet
    /// committed: a downloaded copy whose content is stale or whose item is gone is removed,
    /// and one whose item moved moves with it. What changed is announced by
    /// [`committed`](Self::committed).
    ///
    /// A read-write folder's cycle brings what the outbox holds (`rw`): an item a change of
    /// the user's waits on is left as it is, and what the user made where an item comes or
    /// in a folder that goes is kept ([`super::conflict`]).
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
            Kind::File => s.has_copy(rel, id),
            Kind::Folder => fs::symlink_metadata(s.under(rel)).is_ok_and(|m| m.is_dir()) && carried(&s.under(rel)).is_none_or(|carried| carried == id),
        };
        let (mut told, mut unpinned, mut met, mut gone) = (Vec::new(), Vec::new(), Met::default(), Vec::new());
        for step in &steps {
            // Whatever its source remembered of the item is of what it was.
            s.source.forget(&step.id);
            match (&step.old, &step.new) {
                (Some((old, was)), None) => {
                    match old.kind {
                        Kind::File => drop(remove_copy(&s.under(was), &step.id)),
                        Kind::Folder => gone.push((step.id.clone(), was.clone())),
                    }
                    unpinned.push(step.id.clone());
                    told.push(Announce::Gone(was.clone(), old.kind, step.id.clone(), old.parent_id.clone()));
                }
                (None, Some((new, now))) => {
                    s.out_of_the_way(now, new.parent_id.as_deref(), &mut met);
                    told.push(Announce::Came(now.clone(), new.kind, new.parent_id.clone()));
                }
                (Some((old, was)), Some((new, now))) => {
                    if was != now {
                        s.out_of_the_way(now, new.parent_id.as_deref(), &mut met);
                    }
                    if step.content_changed() {
                        remove_copy(&s.under(was), &step.id);
                    } else if was != now && ours(was, &step.id, old.kind) && fs::symlink_metadata(s.under(now)).is_err() {
                        let _ = now.parent().map(|dir| s.backing_staged(store, dir));
                        let _ = fs::rename(s.under(was), s.under(now));
                    }
                    if was != now {
                        told.push(Announce::Gone(was.clone(), old.kind, step.id.clone(), old.parent_id.clone()));
                        told.push(Announce::Came(now.clone(), new.kind, new.parent_id.clone()));
                    } else if new.kind == Kind::File {
                        told.push(Announce::Changed(now.clone(), step.id.clone()));
                    }
                }
                (None, None) => {}
            }
        }
        // The directories of the folders that went, deepest first: one goes once what was
        // below it went.
        gone.sort_by_key(|(_, rel)| std::cmp::Reverse(rel.components().count()));
        for (id, rel) in &gone {
            s.folder_gone(id, rel, rw.is_some(), &mut met);
        }
        let Met { detections, ops, copied, kept } = met;
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
        let dropped: Vec<String> = {
            let mut pins = crate::panic::lock(&self.shared.pins);
            unpinned.into_iter().filter(|id| pins.remove(id)).collect()
        };
        if !dropped.is_empty() {
            let store = self.shared.store.clone();
            self.shared.runtime.spawn(async move {
                let _ = store.call(move |s| dropped.iter().try_for_each(|id| s.set_pin(id, false))).await;
            });
        }
        self.shared.announce(told);
        self.shared.sweep();
        // What the cycle kept of the user's own is the outbox's to send.
        self.shared.wake();
    }
}
