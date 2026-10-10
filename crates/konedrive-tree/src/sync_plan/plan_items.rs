//! The items a plan is made over, found by what the plan asks of one item about another: the
//! row of a directory, the item the folder shows at a place, what each side has in a
//! directory, under a name as OneDrive compares names, and at any depth. It knows the rows
//! the plan was given and no others: an item that was not given is not found.

use std::collections::HashMap;
use std::ffi::OsStr;

use crate::folded_name::FoldedName;
use crate::model::{Item, ItemPlace, LocalItemId};

/// The rows a plan is made over, by local item id, by local place, and by the directory each
/// side has them in.
pub(crate) struct PlanItems<'a> {
    by_id: HashMap<LocalItemId, &'a Item>,
    by_local_place: HashMap<(LocalItemId, &'a OsStr), LocalItemId>,
    /// What the folder shows in each directory, by id.
    shown_in: HashMap<LocalItemId, Vec<LocalItemId>>,
    /// What OneDrive has in each directory, by id.
    in_onedrive_in: HashMap<LocalItemId, Vec<LocalItemId>>,
}

fn local_place(item: &Item) -> Option<&ItemPlace> {
    item.local.as_ref().and_then(|local| local.place.as_ref())
}

fn onedrive_place(item: &Item) -> Option<&ItemPlace> {
    item.onedrive.as_ref().and_then(|onedrive| onedrive.place.as_ref())
}

impl<'a> PlanItems<'a> {
    pub(crate) fn of(items: &'a [Item]) -> Self {
        let mut items_of_plan = Self { by_id: HashMap::new(), by_local_place: HashMap::new(), shown_in: HashMap::new(), in_onedrive_in: HashMap::new() };
        for item in items {
            items_of_plan.by_id.insert(item.local_item_id, item);
            if let Some(place) = local_place(item) {
                items_of_plan.by_local_place.insert((place.parent_local_item_id, place.name.as_os_str()), item.local_item_id);
                items_of_plan.shown_in.entry(place.parent_local_item_id).or_default().push(item.local_item_id);
            }
            if let Some(place) = onedrive_place(item) {
                items_of_plan.in_onedrive_in.entry(place.parent_local_item_id).or_default().push(item.local_item_id);
            }
        }
        items_of_plan.shown_in.values_mut().chain(items_of_plan.in_onedrive_in.values_mut()).for_each(|in_directory| in_directory.sort());
        items_of_plan
    }

    /// How many rows were given.
    pub(crate) fn count(&self) -> usize {
        self.by_id.len()
    }

    /// The row of `item`, when it was given.
    pub(crate) fn find(&self, item: LocalItemId) -> Option<&'a Item> {
        self.by_id.get(&item).copied()
    }

    /// The item the folder shows at `place`, its name compared as it was given.
    pub(crate) fn find_shown_at(&self, place: &ItemPlace) -> Option<LocalItemId> {
        self.by_local_place.get(&(place.parent_local_item_id, place.name.as_os_str())).copied()
    }

    /// One of the items the folder shows in `directory`, the one with the lowest id; `None`
    /// when it shows none of the given items there.
    pub(crate) fn find_first_shown_in(&self, directory: LocalItemId) -> Option<LocalItemId> {
        self.list_shown_in(directory).first().copied()
    }

    /// The items the folder shows in `directory`, by id.
    pub(crate) fn list_shown_in(&self, directory: LocalItemId) -> &[LocalItemId] {
        self.shown_in.get(&directory).map_or(&[], Vec::as_slice)
    }

    /// The items OneDrive has in `directory`, by id.
    pub(crate) fn list_in_onedrive_in(&self, directory: LocalItemId) -> &[LocalItemId] {
        self.in_onedrive_in.get(&directory).map_or(&[], Vec::as_slice)
    }

    /// The items the folder shows in `directory` under a name that folds to `name`, by id.
    pub(crate) fn list_shown_with_name(&self, directory: LocalItemId, name: &FoldedName) -> Vec<LocalItemId> {
        let has_name = |item: &LocalItemId| self.find(*item).and_then(local_place).is_some_and(|place| FoldedName::of(&place.name) == *name);
        self.list_shown_in(directory).iter().copied().filter(has_name).collect()
    }

    /// The items OneDrive has in `directory` under a name that folds to `name`, by id.
    pub(crate) fn list_in_onedrive_with_name(&self, directory: LocalItemId, name: &FoldedName) -> Vec<LocalItemId> {
        let has_name = |item: &LocalItemId| self.find(*item).and_then(onedrive_place).is_some_and(|place| FoldedName::of(&place.name) == *name);
        self.list_in_onedrive_in(directory).iter().copied().filter(has_name).collect()
    }

    /// The items OneDrive has in `directory` and in the directories in it, at any depth:
    /// each directory before what is in it.
    pub(crate) fn list_below_in_onedrive(&self, directory: LocalItemId) -> Vec<LocalItemId> {
        let mut below = Vec::new();
        let mut to_read: Vec<LocalItemId> = self.list_in_onedrive_in(directory).iter().rev().copied().collect();
        // No more items than were given: rows that put an item above itself end the walk.
        while let Some(item) = to_read.pop().filter(|_| below.len() < self.by_id.len()) {
            below.push(item);
            to_read.extend(self.list_in_onedrive_in(item).iter().rev());
        }
        below
    }

    /// Whether the folder shows `item` in `directory` or in a directory in it, at any
    /// depth; an item is not in itself.
    pub(crate) fn is_shown_in(&self, item: LocalItemId, directory: LocalItemId) -> bool {
        self.is_in(item, directory, local_place)
    }

    /// Whether OneDrive has `item` in `directory` or in a directory in it, at any depth; an
    /// item is not in itself.
    pub(crate) fn is_in_onedrive_in(&self, item: LocalItemId, directory: LocalItemId) -> bool {
        self.is_in(item, directory, onedrive_place)
    }

    /// Whether the way up from `item`, by the place `place_of` gives for each row, passes
    /// `directory`. It ends at a row that was not given, and after as many rows as were
    /// given.
    fn is_in(&self, item: LocalItemId, directory: LocalItemId, place_of: fn(&Item) -> Option<&ItemPlace>) -> bool {
        let mut at = item;
        for _ in 0..self.by_id.len() {
            match self.find(at).and_then(place_of) {
                Some(place) if place.parent_local_item_id == directory => return true,
                Some(place) => at = place.parent_local_item_id,
                None => return false,
            }
        }
        false
    }
}

#[cfg(test)]
mod tests;
