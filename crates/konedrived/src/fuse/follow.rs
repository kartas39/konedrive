//! The folder under the mount made to follow a cycle of the listing: what changed, and what
//! of it the folder holds.

use std::path::{Path, PathBuf};

use konedrive_tree::{Kind, Placement, Row, Table, TreeStore};

use super::announce::Announce;
use super::conflict::{Meeting, Met};
use super::detections::rebase;
use super::mount::Mount;
use super::under::Under;
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
fn backing_staged(under: &Under, store: &TreeStore, rel: &Path) -> std::io::Result<()> {
    let staged = |above: Option<&str>, _: &Path, name: &str| {
        let row = store.child(Table::Staging, above?, name).ok().flatten()?;
        (row.placement == Placement::Placed).then_some(row.id)
    };
    under.backing(rel, store.root_item_id().ok().flatten(), staged)
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
            Kind::File => s.under.has_copy(rel, id),
            Kind::Folder => s.under.stat(rel).is_some_and(|m| m.is_dir()) && s.under.carried(rel).is_none_or(|carried| carried == id),
        };
        let (mut told, mut unpinned, mut met, mut gone) = (Vec::new(), Vec::new(), Met::default(), Vec::new());
        let meeting = Meeting { under: &s.under, view: &s.view, rules: &s.upload_rules, now: s.now() };
        for step in &steps {
            // Whatever its source remembered of the item is of what it was.
            s.source.forget(&step.id);
            match (&step.old, &step.new) {
                (Some((old, was)), None) => {
                    match old.kind {
                        Kind::File => drop(s.under.remove_copy(was, &step.id)),
                        Kind::Folder => gone.push((step.id.clone(), was.clone())),
                    }
                    unpinned.push(step.id.clone());
                    told.push(Announce::Gone(was.clone(), old.kind, step.id.clone(), old.parent_id.clone()));
                }
                (None, Some((new, now))) => {
                    meeting.keep_beside_item(now, new.parent_id.as_deref(), &mut met);
                    told.push(Announce::Came(now.clone(), new.kind, new.parent_id.clone()));
                }
                (Some((old, was)), Some((new, now))) => {
                    if was != now {
                        meeting.keep_beside_item(now, new.parent_id.as_deref(), &mut met);
                    }
                    if was != now && new.kind == Kind::Folder {
                        // The rows of what the user changed below it are found by place.
                        met.ops.push(rebase(was, now));
                    }
                    if step.content_changed() {
                        s.under.remove_copy(was, &step.id);
                    } else if was != now && ours(was, &step.id, old.kind) && s.under.stat(now).is_none() {
                        let _ = now.parent().map(|dir| backing_staged(&s.under, store, dir));
                        let _ = s.under.rename(was, now);
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
            meeting.keep_gone_folder(id, rel, rw.is_some(), &mut met);
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
        self.shared.recorder.wake();
    }
}
