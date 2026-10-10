//! Which item has each place in the folder, as the rows say it now ([`LocalPlaces`]): the
//! places the rows had when they were read, kept in step with every write the check makes
//! since, so that the order of the writes is found in memory and not asked of the store.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};

use konedrive_tree::{Item, LocalItemId};

#[derive(Default)]
pub(super) struct LocalPlaces {
    place_of: HashMap<LocalItemId, (LocalItemId, OsString)>,
    inside: HashMap<LocalItemId, HashMap<OsString, LocalItemId>>,
}

impl LocalPlaces {
    /// The places of the items the folder shows.
    pub(super) fn new<'a>(items: impl Iterator<Item = &'a Item>) -> Self {
        let mut places = LocalPlaces::default();
        for item in items {
            if let Some(place) = item.local.as_ref().and_then(|local| local.place.as_ref()) {
                places.set(item.local_item_id, place.parent_local_item_id, &place.name);
            }
        }
        places
    }

    pub(super) fn find_at(&self, directory: LocalItemId, name: &OsStr) -> Option<LocalItemId> {
        self.inside.get(&directory)?.get(name).copied()
    }

    /// The directory `item` is in and its name there; `None` when the folder does not show
    /// it.
    pub(super) fn find_place(&self, item: LocalItemId) -> Option<(LocalItemId, &OsStr)> {
        self.place_of.get(&item).map(|(directory, name)| (*directory, name.as_os_str()))
    }

    pub(super) fn is_at(&self, item: LocalItemId, directory: LocalItemId, name: &OsStr) -> bool {
        self.find_place(item) == Some((directory, name))
    }

    /// The items in `directory`, by name.
    pub(super) fn list_inside(&self, directory: LocalItemId) -> Vec<LocalItemId> {
        let mut inside: Vec<(&OsString, LocalItemId)> = self.inside.get(&directory).into_iter().flatten().map(|(name, id)| (name, *id)).collect();
        inside.sort();
        inside.into_iter().map(|(_, id)| id).collect()
    }

    /// `item` is at `name` in `directory` from now on, wherever it was.
    pub(super) fn set(&mut self, item: LocalItemId, directory: LocalItemId, name: &OsStr) {
        self.remove(item);
        self.place_of.insert(item, (directory, name.to_owned()));
        self.inside.entry(directory).or_default().insert(name.to_owned(), item);
    }

    /// The folder does not show `item` any more.
    pub(super) fn remove(&mut self, item: LocalItemId) {
        if let Some((directory, name)) = self.place_of.remove(&item) {
            if let Some(inside) = self.inside.get_mut(&directory) {
                inside.remove(&name);
            }
        }
    }
}
