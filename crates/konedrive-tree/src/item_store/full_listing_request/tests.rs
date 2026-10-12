use super::*;
use crate::item_store::tests::shown_items::store_in_file_with_listed_root;
use crate::TreeStore;

fn is_requested(store: &mut TreeStore) -> bool {
    store.item_store().reader().is_full_listing_requested().unwrap()
}

#[test]
fn a_full_listing_that_was_asked_for_is_still_asked_for_after_the_store_is_opened_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tree.sqlite");
    let mut store = store_in_file_with_listed_root(&path);
    assert!(!is_requested(&mut store), "nothing was asked for");
    store.item_store().request_full_listing(FullListingCause::PlanFault(LocalItemId::new(7))).unwrap();
    store.item_store().request_full_listing(FullListingCause::BoundByMarkAtStart).unwrap();
    assert!(is_requested(&mut store));
    drop(store);

    let mut store = TreeStore::open(&path).unwrap();
    assert!(is_requested(&mut store));
    assert_eq!(store.meta(crate::meta::FULL_LISTING_REQUESTED).unwrap().as_deref(), Some("the plan cannot judge item 7"), "asked twice is asked once: the first cause is kept, for the journal");
    assert!(TreeStore::open_read_only(&path).unwrap().read_items(|reader| reader.is_full_listing_requested()).unwrap());
}

#[test]
fn beginning_a_full_listing_takes_the_request_away() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store_in_file_with_listed_root(&dir.path().join("tree.sqlite"));
    store.item_store().request_full_listing(FullListingCause::BoundByMarkAtStart).unwrap();
    store.item_store().begin_full_listing().unwrap();
    assert!(!is_requested(&mut store));
    store.item_store().request_full_listing(FullListingCause::BoundByMarkAtStart).unwrap();
    assert!(is_requested(&mut store), "asked for again while one is under way: the next one");
}

#[test]
fn beginning_a_listing_of_changes_leaves_the_request() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store_in_file_with_listed_root(&dir.path().join("tree.sqlite"));
    store.item_store().request_full_listing(FullListingCause::BoundByMarkAtStart).unwrap();
    store.item_store().begin_delta_listing().unwrap();
    assert!(is_requested(&mut store));
}
