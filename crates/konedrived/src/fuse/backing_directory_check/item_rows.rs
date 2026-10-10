//! All the rows of `items`, read once ([`ItemRows`]): each by its local item id, the rows
//! the folder shows by their place, every row that has a binding by the item id of it, and
//! the files the folder does not show by the place they were removed from.
//!
//! A binding is a drive and an item id, and a mark on disk holds the item id alone. So a row
//! is asked for by an item id, and the answer says when rows of two drives have it
//! ([`BoundRows`]): which of them a mark means is then not known, and nothing here chooses.
//!
//! They are the rows as they were before the check wrote anything; what the check has
//! written since is [`LocalPlaces`](super::local_places::LocalPlaces)'s to know.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};

use konedrive_tree::{Item, ItemKind, LocalItemId};

/// The rows whose binding has one OneDrive item id, shown by the folder or not.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum BoundRows<'a> {
    /// No row has the id.
    None,
    /// One row has it.
    One(&'a Item),
    /// Rows of two drives have it, or of more: the id alone does not name an item.
    OfTwoDrives,
}

pub(super) struct ItemRows {
    items: HashMap<LocalItemId, Item>,
    /// What the folder shows in each directory, by name.
    shown_inside: HashMap<LocalItemId, HashMap<OsString, LocalItemId>>,
    /// The rows that have a binding, by the item id of it. One of an id, but where an item
    /// id is in two drives.
    bound: HashMap<String, Vec<LocalItemId>>,
    /// The files of OneDrive the folder does not show, by the place they were agreed to be
    /// at: removed here, their removal not sent.
    files_removed_here: HashMap<LocalItemId, HashMap<OsString, LocalItemId>>,
}

impl ItemRows {
    pub(super) fn new(items: Vec<Item>) -> Self {
        let mut rows = ItemRows { items: HashMap::with_capacity(items.len()), shown_inside: HashMap::new(), bound: HashMap::new(), files_removed_here: HashMap::new() };
        for item in items {
            let id = item.local_item_id;
            if let Some(reference) = &item.onedrive_item {
                rows.bound.entry(reference.item_id.as_str().to_owned()).or_default().push(id);
            }
            match &item.local {
                Some(local) => {
                    if let Some(place) = &local.place {
                        rows.shown_inside.entry(place.parent_local_item_id).or_default().insert(place.name.clone(), id);
                    }
                }
                None => {
                    if let (ItemKind::File, Some(_), Some(place)) = (item.kind, &item.onedrive_item, &item.synced_place) {
                        rows.files_removed_here.entry(place.parent_local_item_id).or_default().entry(place.name.clone()).or_insert(id);
                    }
                }
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

    /// The rows whose binding has the item id `onedrive_id`, whichever drive they are of.
    pub(super) fn find_bound(&self, onedrive_id: &str) -> BoundRows<'_> {
        match self.bound.get(onedrive_id).map(Vec::as_slice) {
            Some([one]) => self.items.get(one).map_or(BoundRows::None, BoundRows::One),
            Some([_, _, ..]) => BoundRows::OfTwoDrives,
            Some([]) | None => BoundRows::None,
        }
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

#[cfg(test)]
pub(super) mod tests;
