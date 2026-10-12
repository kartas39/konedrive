//! The rules that hold of the rows at all times (`docs/tasks/item-versions-design.md`, §3),
//! asked of rows read whole: what a test of the plan and of the commits of its steps checks
//! after every change.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{ContentPresence, Item, ItemKind, ItemPlace, ItemVersion, LocalItemId};

fn place_of(version: &Option<ItemVersion>) -> Option<&ItemPlace> {
    version.as_ref().and_then(|version| version.place.as_ref())
}

/// The first rule `items`, every row of a store, break; `None` when they keep them all.
/// While a listing is under way the rows of OneDrive's side say what was listed so far, and
/// only what holds of them then is asked.
pub(crate) fn find_broken_rule(items: &[Item], is_listing_under_way: bool) -> Option<String> {
    let by_id: BTreeMap<LocalItemId, &Item> = items.iter().map(|item| (item.local_item_id, item)).collect();
    let mut local_names = BTreeSet::new();
    let mut onedrive_names = BTreeSet::new();
    for item in items {
        let id = item.local_item_id;
        if item.onedrive_item.is_none() && (item.onedrive.is_some() || item.onedrive_etag.is_some() || item.synced_place.is_some() || item.synced_content.is_some()) {
            return Some(format!("item {id} has no binding, and something of OneDrive's or something agreed on"));
        }
        if item.onedrive_item.is_none() && item.local.is_none() {
            return Some(format!("item {id} has no binding and the folder does not show it: a row of nothing"));
        }
        for (side, version) in [("the folder", &item.local), ("OneDrive", &item.onedrive)] {
            let Some(version) = version else { continue };
            if (item.kind == ItemKind::File) != version.content.is_some() || (id == LocalItemId::ROOT) != version.place.is_none() {
                return Some(format!("item {id} in {side}: a file has a content and a directory none, and all but the root have a place"));
            }
        }
        if item.kind == ItemKind::Directory && (item.synced_content.is_some() || item.content_presence == ContentPresence::OnDisk) {
            return Some(format!("item {id} is a directory with a content"));
        }
        let shows_content_not_on_disk = item.kind == ItemKind::File && item.local.is_some() && item.content_presence == ContentPresence::NotOnDisk;
        if shows_content_not_on_disk && item.local.as_ref().and_then(|local| local.content.as_ref()) != item.synced_content.as_ref() {
            return Some(format!("item {id}: the content the folder shows is not on disk, and is not the content agreed on"));
        }
        if let Some(place) = place_of(&item.local) {
            let parent = by_id.get(&place.parent_local_item_id);
            if !parent.is_some_and(|parent| parent.kind == ItemKind::Directory && parent.local.is_some()) {
                return Some(format!("item {id} is shown in item {}, which is no directory the folder shows", place.parent_local_item_id));
            }
            if !local_names.insert((place.parent_local_item_id, place.name.clone())) {
                return Some(format!("the folder shows two items under {:?} in item {}", place.name, place.parent_local_item_id));
            }
            if !ends_at_root(&by_id, id, |item| place_of(&item.local)) {
                return Some(format!("item {id} is above itself in the folder"));
            }
        }
        if let Some(place) = &item.synced_place {
            if !by_id.get(&place.parent_local_item_id).is_some_and(|parent| parent.kind == ItemKind::Directory) {
                return Some(format!("item {id} was agreed to be in item {}, which is no directory", place.parent_local_item_id));
            }
        }
        if let Some(place) = place_of(&item.onedrive).filter(|_| !is_listing_under_way) {
            let parent = by_id.get(&place.parent_local_item_id);
            if !parent.is_some_and(|parent| parent.kind == ItemKind::Directory && parent.onedrive.is_some()) {
                return Some(format!("OneDrive has item {id} in item {}, which is no directory OneDrive has", place.parent_local_item_id));
            }
            // The very name: of two names that fold to one, OneDrive is the judge.
            if !onedrive_names.insert((place.parent_local_item_id, place.name.clone())) {
                return Some(format!("OneDrive has two items under {:?} in item {}", place.name, place.parent_local_item_id));
            }
            if !ends_at_root(&by_id, id, |item| place_of(&item.onedrive)) {
                return Some(format!("item {id} is above itself in OneDrive"));
            }
        }
    }
    None
}

/// Whether the way up from `item`, by the place `place_of` gives for each row, ends at the
/// root within as many rows as there are.
fn ends_at_root(by_id: &BTreeMap<LocalItemId, &Item>, item: LocalItemId, place_of: fn(&Item) -> Option<&ItemPlace>) -> bool {
    let mut at = item;
    for _ in 0..=by_id.len() {
        if at == LocalItemId::ROOT {
            return true;
        }
        match by_id.get(&at).and_then(|row| place_of(row)) {
            Some(place) => at = place.parent_local_item_id,
            None => return false,
        }
    }
    false
}
