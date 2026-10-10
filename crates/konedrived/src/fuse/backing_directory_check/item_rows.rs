//! All the rows of `items`, read once ([`ItemRows`]): each by its local item id, the rows
//! the folder shows by their place and by their binding, and the rows it does not show by
//! their binding and by the place a file was removed from.
//!
//! A binding is a drive and an item id, and OneDrive may give one item id in two drives: a
//! row is asked for by the two. The drive tells two rows of one item id apart and does no
//! more: an item is not always of the drive of the directory it is in (a shared folder is
//! another drive's), so where no row of the drive asked for has the id, the row that has
//! it is the answer — of several, the one made first.
//!
//! They are the rows as they were before the check wrote anything; what the check has
//! written since is [`LocalPlaces`](super::local_places::LocalPlaces)'s to know.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};

use konedrive_tree::{DriveId, Item, ItemKind, LocalItemId};

pub(super) struct ItemRows {
    items: HashMap<LocalItemId, Item>,
    /// What the folder shows in each directory, by name.
    shown_inside: HashMap<LocalItemId, HashMap<OsString, LocalItemId>>,
    /// The bound rows the folder shows, by the item id of their binding: each with its
    /// drive. One, but where an item id is in two drives.
    shown_bound: HashMap<String, Vec<(DriveId, LocalItemId)>>,
    /// The same of the bound rows the folder does not show.
    unshown_bound: HashMap<String, Vec<(DriveId, LocalItemId)>>,
    /// The files of OneDrive the folder does not show, by the place they were agreed to be
    /// at: removed here, their removal not sent.
    files_removed_here: HashMap<LocalItemId, HashMap<OsString, LocalItemId>>,
}

impl ItemRows {
    pub(super) fn new(items: Vec<Item>) -> Self {
        let mut rows = ItemRows {
            items: HashMap::with_capacity(items.len()),
            shown_inside: HashMap::new(),
            shown_bound: HashMap::new(),
            unshown_bound: HashMap::new(),
            files_removed_here: HashMap::new(),
        };
        for item in items {
            let id = item.local_item_id;
            let binding = item.onedrive_item.as_ref().map(|reference| (reference.item_id.as_str().to_owned(), reference.drive_id.clone()));
            match (&item.local, binding) {
                (Some(local), binding) => {
                    if let Some(place) = &local.place {
                        rows.shown_inside.entry(place.parent_local_item_id).or_default().insert(place.name.clone(), id);
                    }
                    if let Some((onedrive_id, drive)) = binding {
                        rows.shown_bound.entry(onedrive_id).or_default().push((drive, id));
                    }
                }
                (None, Some((onedrive_id, drive))) => {
                    rows.unshown_bound.entry(onedrive_id).or_default().push((drive, id));
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

    /// The item the folder shows that is the item `onedrive_id` of OneDrive: of the drive
    /// `drive`, or, when no row of that drive has the id, of whichever drive has it.
    pub(super) fn find_shown_bound(&self, drive: &DriveId, onedrive_id: &str) -> Option<&Item> {
        Self::find_bound(&self.shown_bound, drive, onedrive_id).and_then(|id| self.items.get(&id))
    }

    /// The same of the items the folder does not show.
    pub(super) fn find_unshown_bound(&self, drive: &DriveId, onedrive_id: &str) -> Option<&Item> {
        Self::find_bound(&self.unshown_bound, drive, onedrive_id).and_then(|id| self.items.get(&id))
    }

    fn find_bound(bound: &HashMap<String, Vec<(DriveId, LocalItemId)>>, drive: &DriveId, onedrive_id: &str) -> Option<LocalItemId> {
        let of_the_id = bound.get(onedrive_id)?;
        of_the_id.iter().find(|(of, _)| of == drive).or_else(|| of_the_id.iter().min_by_key(|(_, id)| *id)).map(|(_, id)| *id)
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
