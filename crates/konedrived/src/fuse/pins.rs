//! "Always keep on this device": the pins the store holds, and the downloads they ask for.

use std::path::Path;
use std::sync::Arc;

use konedrive_graph::pool::Class;
use konedrive_tree::Row;

use super::announce::Announce;
use super::asked::Refused;
use super::mount::Mount;
use super::shared::Shared;

impl Shared {
    /// The item that keeps `row` on this device: itself or a folder above it, whichever
    /// carries a pin.
    pub(super) fn pinned_by(&self, row: &Row) -> Option<Row> {
        let pins = crate::panic::lock(&self.pins);
        let mut at = Some(row.clone());
        while let Some(row) = at {
            if pins.contains(&row.id) {
                return Some(row);
            }
            at = self.view.parent(&row);
        }
        None
    }

    /// Downloads every pinned file that is not downloaded: run when a pin is put on and after
    /// every cycle, so that a new file in a pinned folder and a pinned file whose content
    /// changed are downloaded by it.
    pub(super) fn sweep(self: &Arc<Self>) {
        // Off the caller's thread: a cycle calls this while it holds the folder's tree lock,
        // and a large pinned folder is a long walk.
        let s = Arc::clone(self);
        self.runtime.spawn_blocking(move || {
            let pins: Vec<String> = crate::panic::lock(&s.pins).iter().cloned().collect();
            for id in pins {
                let Some(row) = s.view.row(&id) else { continue };
                for (file, rel) in s.view.files_below(&row) {
                    if !s.under.has_copy(&rel, &file.id) {
                        s.ensure(file, rel, Class::Download, None);
                    }
                }
            }
        });
    }
}

impl Mount {
    /// Puts the pin on the item at `path`, or takes it off. A pin put on downloads what it
    /// keeps.
    pub async fn set_pin(&self, path: &Path, pinned: bool) -> Result<(), Refused> {
        let (row, rel) = self.item(path)?;
        let id = row.id.clone();
        self.shared.store.call(move |s| s.set_pin(&id, pinned)).await.map_err(|e| Refused::Io(e.to_string()))?;
        let changed = {
            let mut pins = crate::panic::lock(&self.shared.pins);
            if pinned { pins.insert(row.id.clone()) } else { pins.remove(&row.id) }
        };
        if changed {
            self.shared.announce(vec![Announce::Changed(rel, row.id)]);
        }
        if pinned {
            self.shared.sweep();
        }
        Ok(())
    }

    /// How many items carry a pin of their own.
    pub fn pinned_count(&self) -> u64 {
        crate::panic::lock(&self.shared.pins).len() as u64
    }
}
