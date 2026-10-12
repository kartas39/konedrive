//! A store with items of OneDrive that the folder shows, built as the daemon builds one: a
//! listing names the item, the step that shows it commits. What the tests of the readers
//! and of the mount's own writers start from.

use std::path::Path;

use super::listed_drive::{listed_directory, listed_file, listed_root, reference, write_listing_of_changes};
use super::row_rules::find_broken_rule;
use crate::model::{ContentVersion, Item, LocalItemId};
use crate::sync_plan::ItemVersions;
use crate::{ListingLink, StepCommitOutcome, TreeStore};

/// `store` knows its drive, `d1`, and a full listing told of the drive's root, `R`, and
/// ended.
pub(crate) fn write_listed_root(store: &mut TreeStore) {
    store.set_drive_id("d1").unwrap();
    let listing = store.item_store().begin_full_listing().unwrap();
    let outcome = store.item_store().write_listed_changes(listing.id, &[listed_root()], listing.began_at_count, &ListingLink::DeltaLink("link-0".into())).unwrap();
    assert!(outcome.is_link_written());
}

/// A store in a file at `path`, whose root a full listing told of: one a second connection
/// can read.
pub(crate) fn store_in_file_with_listed_root(path: &Path) -> TreeStore {
    let mut store = TreeStore::open(path).unwrap();
    write_listed_root(&mut store);
    store
}

/// The row of the item of OneDrive `item_id`.
pub(crate) fn bound(store: &mut TreeStore, item_id: &str) -> Item {
    store.item_store().find_bound_item(&reference(item_id)).unwrap().unwrap_or_else(|| panic!("the item of OneDrive {item_id} has no row"))
}

pub(crate) fn row(store: &mut TreeStore, id: LocalItemId) -> Item {
    store.item_store().find_item(id).unwrap().unwrap_or_else(|| panic!("item {id} has no row"))
}

/// The folder shows the item of OneDrive `item_id`, which a listing named: the commit of
/// the step that shows it.
pub(crate) fn show(store: &mut TreeStore, item_id: &str) -> LocalItemId {
    let listed = bound(store, item_id);
    assert_eq!(store.item_store().commit_show_item(&ItemVersions::of(&listed), false).unwrap(), StepCommitOutcome::Written);
    listed.local_item_id
}

/// A listing names the directory `item_id` in `parent` under `name`, and the folder shows it.
pub(crate) fn show_listed_directory(store: &mut TreeStore, item_id: &str, parent: &str, name: &str) -> LocalItemId {
    write_listing_of_changes(store, &[listed_directory(item_id, parent, name, &format!("e-{item_id}"))]);
    show(store, item_id)
}

/// A listing names the file `item_id` in `parent` under `name`, with the content
/// [`listed_content`] of `hash`, and the folder shows it: synced, its content not on disk.
pub(crate) fn show_listed_file(store: &mut TreeStore, item_id: &str, parent: &str, name: &str, hash: &str) -> LocalItemId {
    write_listing_of_changes(store, &[listed_file(item_id, parent, name, (3, hash), &format!("e-{item_id}"))]);
    show(store, item_id)
}

/// The content [`show_listed_file`] gives a file of `hash`.
pub(crate) fn listed_content(hash: &str) -> ContentVersion {
    ContentVersion::Hashed { size: 3, mtime: 1000, mtime_nsec: 0, hash: Some(hash.to_owned()), ctag: Some(format!("c-{hash}")) }
}

/// The rows keep their rules, with no listing under way.
pub(crate) fn assert_rules_hold(store: &mut TreeStore) {
    assert_eq!(find_broken_rule(&store.item_store().list_items().unwrap(), false), None);
}
