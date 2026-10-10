//! All the rows of `items`, read once ([`ItemRows`]): each by its local item id, the rows
//! the folder shows by their place and by the OneDrive item id of their binding, and the
//! rows it does not show by that id and by the place a file was removed from.
//!
//! They are the rows as they were before the check wrote anything; what the check has
//! written since is [`LocalPlaces`](super::local_places::LocalPlaces)'s to know.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};

use konedrive_tree::{Item, ItemKind, LocalItemId};

pub(super) struct ItemRows {
    items: HashMap<LocalItemId, Item>,
    /// What the folder shows in each directory, by name.
    shown_inside: HashMap<LocalItemId, HashMap<OsString, LocalItemId>>,
    shown_by_onedrive_id: HashMap<String, LocalItemId>,
    unshown_by_onedrive_id: HashMap<String, LocalItemId>,
    /// The files of OneDrive the folder does not show, by the place they were agreed to be
    /// at: removed here, their removal not sent.
    files_removed_here: HashMap<LocalItemId, HashMap<OsString, LocalItemId>>,
}

impl ItemRows {
    pub(super) fn new(items: Vec<Item>) -> Self {
        let mut rows = ItemRows {
            items: HashMap::with_capacity(items.len()),
            shown_inside: HashMap::new(),
            shown_by_onedrive_id: HashMap::new(),
            unshown_by_onedrive_id: HashMap::new(),
            files_removed_here: HashMap::new(),
        };
        for item in items {
            let id = item.local_item_id;
            let onedrive_id = item.onedrive_item.as_ref().map(|reference| reference.item_id.as_str().to_owned());
            match (&item.local, onedrive_id) {
                (Some(local), onedrive_id) => {
                    if let Some(place) = &local.place {
                        rows.shown_inside.entry(place.parent_local_item_id).or_default().insert(place.name.clone(), id);
                    }
                    if let Some(onedrive_id) = onedrive_id {
                        // One item id in two drives: the row made first is the one a mark finds.
                        rows.shown_by_onedrive_id.entry(onedrive_id).or_insert(id);
                    }
                }
                (None, Some(onedrive_id)) => {
                    rows.unshown_by_onedrive_id.entry(onedrive_id).or_insert(id);
                    if let (ItemKind::File, Some(place)) = (item.kind, &item.synced_place) {
                        rows.files_removed_here.entry(place.parent_local_item_id).or_default().entry(place.name.clone()).or_insert(id);
                    }
                }
                (None, None) => {}
            }
            rows.items.insert(id, item);
        }
        rows
    }

    pub(super) fn find(&self, item: LocalItemId) -> Option<&Item> {
        self.items.get(&item)
    }

    /// The item the folder shows in `directory` under `name`.
    pub(super) fn find_at(&self, directory: LocalItemId, name: &OsStr) -> Option<&Item> {
        self.shown_inside.get(&directory)?.get(name).and_then(|id| self.items.get(id))
    }

    /// What the folder shows in `directory`, by name.
    pub(super) fn list_inside(&self, directory: LocalItemId) -> Vec<&Item> {
        let mut inside: Vec<(&OsString, &Item)> = self.shown_inside.get(&directory).into_iter().flatten().filter_map(|(name, id)| Some((name, self.items.get(id)?))).collect();
        inside.sort_by(|a, b| a.0.cmp(b.0));
        inside.into_iter().map(|(_, item)| item).collect()
    }

    /// The item the folder shows that is bound to the OneDrive item id `onedrive_id`.
    pub(super) fn find_shown_by_onedrive_id(&self, onedrive_id: &str) -> Option<&Item> {
        self.shown_by_onedrive_id.get(onedrive_id).and_then(|id| self.items.get(id))
    }

    /// The item the folder does not show that is bound to `onedrive_id`.
    pub(super) fn find_unshown_by_onedrive_id(&self, onedrive_id: &str) -> Option<&Item> {
        self.unshown_by_onedrive_id.get(onedrive_id).and_then(|id| self.items.get(id))
    }

    /// The file of OneDrive that was removed here from `name` in `directory`, its removal
    /// not sent: what a file made there replaces.
    pub(super) fn find_file_removed_at(&self, directory: LocalItemId, name: &OsStr) -> Option<LocalItemId> {
        self.files_removed_here.get(&directory)?.get(name).copied()
    }

    /// Every item, in no order.
    pub(super) fn list(&self) -> impl Iterator<Item = &Item> {
        self.items.values()
    }
}
