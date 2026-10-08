//! The folder under the mount made to follow a cycle of the listing: what changed, and what
//! of it the folder holds.

use std::fs;
use std::path::{Path, PathBuf};

use konedrive_tree::{Kind, Row, Table, TreeStore};

use super::announce::Announce;
use super::copies::remove_copy;
use super::mount::Mount;

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

impl Mount {
    /// Makes the folder under the mount follow the cycle that `store` has staged and not yet
    /// committed: a downloaded copy whose content is stale or whose item is gone is removed,
    /// and one whose item moved moves with it. A folder that is gone is left where it is; the
    /// mount no longer shows it. Returns what changed; it is announced by
    /// [`committed`](Self::committed).
    pub fn follow(&self, store: &TreeStore) -> Result<Vec<Step>, konedrive_tree::TreeError> {
        let placed = |table: Table, id: &str| {
            let row = store.get(table, id).ok().flatten()?;
            let at = store.locate(table, id).ok().flatten()?;
            at.placed.then_some((row, at.rel))
        };
        let steps: Vec<Step> =
            store.changed_ids()?.into_iter().map(|id| Step { old: placed(Table::Items, &id), new: placed(Table::Staging, &id), id }).collect();
        let s = &self.shared;
        // The copy of item `id` at `rel`, or nothing: a file that is not that item's download
        // is never moved or removed.
        let ours = |rel: &Path, id: &str, kind: Kind| match kind {
            Kind::File => s.has_copy(rel, id),
            Kind::Folder => fs::symlink_metadata(s.under(rel)).is_ok_and(|m| m.is_dir()),
        };
        let (mut told, mut unpinned) = (Vec::new(), Vec::new());
        for step in &steps {
            // Whatever its source remembered of the item is of what it was.
            s.source.forget(&step.id);
            match (&step.old, &step.new) {
                (Some((old, was)), None) => {
                    if old.kind == Kind::File {
                        remove_copy(&s.under(was), &step.id);
                    }
                    unpinned.push(step.id.clone());
                    told.push(Announce::Gone(was.clone(), old.kind, step.id.clone(), old.parent_id.clone()));
                }
                (None, Some((new, now))) => told.push(Announce::Came(now.clone(), new.kind, new.parent_id.clone())),
                (Some((old, was)), Some((new, now))) => {
                    if step.content_changed() {
                        remove_copy(&s.under(was), &step.id);
                    } else if was != now && ours(was, &step.id, old.kind) && fs::symlink_metadata(s.under(now)).is_err() {
                        let _ = s.under(now).parent().map(fs::create_dir_all);
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
        // Only this cycle's: what an earlier one left was never committed.
        *crate::panic::lock(&s.pending) = (told, unpinned);
        Ok(steps)
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
    }
}
