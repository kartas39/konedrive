//! All the rows of `items`, read once ([`ItemRows`]): each by its local item id, the rows
//! the folder shows by their place, and the OneDrive item ids that rows have.
//!
//! A row is found by its id or by its place. Of an item id of OneDrive only one thing is
//! answered: whether any row has it, shown by the folder or not, of whichever drive
//! ([`ItemRows::has_onedrive_id`]). A mark on disk holds the item id alone, and nothing
//! here leads from an id to a row or to a place.
//!
//! They are the rows as they were before the check wrote anything. The check moves no row,
//! so the places stay as they were read for every row it has not taken from the folder.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};

use konedrive_tree::{Item, LocalItemId};

pub(super) struct ItemRows {
    items: HashMap<LocalItemId, Item>,
    /// What the folder shows in each directory, by name.
    shown_inside: HashMap<LocalItemId, HashMap<OsString, LocalItemId>>,
    /// The OneDrive item ids of the rows that have a binding.
    onedrive_ids: HashSet<String>,
}

impl ItemRows {
    pub(super) fn new(items: Vec<Item>) -> Self {
        let mut rows = ItemRows { items: HashMap::with_capacity(items.len()), shown_inside: HashMap::new(), onedrive_ids: HashSet::new() };
        for item in items {
            let id = item.local_item_id;
            if let Some(reference) = &item.onedrive_item {
                rows.onedrive_ids.insert(reference.item_id.as_str().to_owned());
            }
            if let Some(place) = item.local.as_ref().and_then(|local| local.place.as_ref()) {
                rows.shown_inside.entry(place.parent_local_item_id).or_default().insert(place.name.clone(), id);
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

    /// Whether any row has the item id `onedrive_id` in its binding: shown by the folder or
    /// not, of whichever drive.
    pub(super) fn has_onedrive_id(&self, onedrive_id: &str) -> bool {
        self.onedrive_ids.contains(onedrive_id)
    }
}

#[cfg(test)]
mod tests;
