//! The items a plan is made over, found by what the plan asks of one item about another: the
//! row of a directory, the item the folder shows at a place, and what the folder shows in a
//! directory. It knows the rows the plan was given and no others: an item that was not
//! given is not found.

use std::collections::HashMap;
use std::ffi::OsStr;

use crate::model::{Item, ItemPlace, LocalItemId};

/// The rows a plan is made over, by local item id, by local place, and by the directory the
/// folder shows them in.
pub(crate) struct PlanItems<'a> {
    by_id: HashMap<LocalItemId, &'a Item>,
    by_local_place: HashMap<(LocalItemId, &'a OsStr), LocalItemId>,
    first_shown_in: HashMap<LocalItemId, LocalItemId>,
}

impl<'a> PlanItems<'a> {
    pub(crate) fn of(items: &'a [Item]) -> Self {
        let mut items_of_plan = Self { by_id: HashMap::new(), by_local_place: HashMap::new(), first_shown_in: HashMap::new() };
        for item in items {
            items_of_plan.by_id.insert(item.local_item_id, item);
            if let Some(place) = item.local.as_ref().and_then(|local| local.place.as_ref()) {
                items_of_plan.by_local_place.insert((place.parent_local_item_id, place.name.as_os_str()), item.local_item_id);
                let first = items_of_plan.first_shown_in.entry(place.parent_local_item_id).or_insert(item.local_item_id);
                *first = (*first).min(item.local_item_id);
            }
        }
        items_of_plan
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
        self.first_shown_in.get(&directory).copied()
    }
}

#[cfg(test)]
mod tests;
