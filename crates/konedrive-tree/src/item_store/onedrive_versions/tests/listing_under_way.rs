//! A page says which listing it is a page of: the id the store gave when the listing
//! began. Through the entry, [`ItemStore::write_listed_changes`](crate::ItemStore).

use super::*;

/// The rows and what `meta` keeps of listings, to tell that a refused page wrote nothing.
fn whole(store: &mut TreeStore) -> (Vec<Item>, Option<String>, Option<ListingUnderWay>) {
    (every_row(store), store.delta_link().unwrap(), under_way(store))
}

/// S1 a) of the second review. A listing of changes is fetched; before its answer is
/// given, the store is told to start over (OneDrive asked for it), and the full listing's
/// first page is written. The answer of the listing of changes comes then, with its delta
/// link: it is refused whole. Taken as the full listing's last page, it would end it and
/// take everything that listing has not seen yet.
#[test]
fn a_page_of_a_listing_the_store_gave_up_is_refused_whole() {
    let mut store = listed_store();
    let kept = make_file(&mut store, ROOT, "kept");
    synced(&mut store, kept, "K");
    write_fetched_at(&mut store, &[file("A", "R", "a", "1"), file("K", "R", "kept", "1")], 0, &delta_link("link-1"));
    let given_up = store.item_store().begin_delta_listing().unwrap();

    let full = store.item_store().begin_full_listing().unwrap();
    assert_ne!(full.id, given_up.id);
    store.item_store().write_listed_changes(full.id, &[root()], full.began_at_count, &next_link("next-1")).unwrap();
    let before = whole(&mut store);

    assert!(is_refused(store.item_store().write_listed_changes(given_up.id, &[file("B", "R", "b", "1")], given_up.began_at_count, &delta_link("link-2"))));
    assert_eq!(whole(&mut store), before);
    assert_eq!(kind_count_and_link(&mut store), Some((ListingKind::Full, 0, Some("next-1".to_owned()))));
    assert_eq!((listed(&mut store, "A").onedrive, item(&mut store, kept).onedrive, bound(&mut store, "B")), (version(ROOT, "a", Some("1")), version(ROOT, "kept", Some("1")), None));
}

/// S1 b) of the second review. No page begins a listing: in a store that has a delta
/// link and no listing under way, the pages of a full listing that was not begun are
/// refused, whatever id they come with, and are not written as a listing of changes,
/// which would take nothing of what OneDrive removed meanwhile.
#[test]
fn the_pages_of_a_listing_that_was_not_begun_are_refused() {
    let mut store = listed_store();
    write_fetched_at(&mut store, &[file("A", "R", "a", "1"), file("B", "R", "b", "1")], 0, &delta_link("link-1"));
    let ended = store.item_store().begin_delta_listing().unwrap();
    store.item_store().write_listed_changes(ended.id, &[], 0, &delta_link("link-2")).unwrap();
    let before = whole(&mut store);
    assert_eq!((before.1.as_deref(), before.2.clone()), (Some("link-2"), None));

    assert!(is_refused(store.item_store().write_listed_changes(ended.id, &[root(), file("A", "R", "a", "1")], 0, &delta_link("link-3"))));
    assert_eq!(whole(&mut store), before);
}

/// A listing of changes is begun from the delta link the store has, and from nothing
/// else: refused in a store that has none, as before its first full listing ended and
/// while a full listing is under way, which is not given up for it.
#[test]
fn a_listing_of_changes_is_begun_only_from_a_delta_link() {
    let mut store = TreeStore::in_memory().unwrap();
    store.set_drive_id("d1").unwrap();
    assert!(is_refused(store.item_store().begin_delta_listing()));
    assert_eq!(under_way(&mut store), None);

    let full = store.item_store().begin_full_listing().unwrap();
    assert!(is_refused(store.item_store().begin_delta_listing()));
    assert_eq!(under_way(&mut store), Some(full.clone()));
    store.item_store().write_listed_changes(full.id, &[root()], 0, &delta_link("link-0")).unwrap();

    let changes = store.item_store().begin_delta_listing().unwrap();
    assert_eq!((changes.kind, changes.began_at_count, changes.next_link.clone()), (ListingKind::Delta, 0, None));
    assert!(changes.id > full.id);
    assert_eq!(under_way(&mut store), Some(changes));
}

/// A listing of changes that was stopped is either gone on with, under the id found under
/// way and from the link to its next page, or begun again from the delta link: the one
/// begun again has another id and no link to a next page, and a page of the one given up
/// is refused.
#[test]
fn a_listing_of_changes_begun_again_gives_up_the_one_that_was_stopped() {
    let mut store = listed_store();
    let stopped = store.item_store().begin_delta_listing().unwrap();
    store.item_store().write_listed_changes(stopped.id, &[file("A", "R", "a", "1")], 0, &next_link("next-1")).unwrap();
    assert_eq!(under_way(&mut store), Some(ListingUnderWay { next_link: Some("next-1".to_owned()), ..stopped.clone() }));
    store.item_store().write_listed_changes(stopped.id, &[file("B", "R", "b", "1")], 0, &next_link("next-2")).unwrap();

    let again = store.item_store().begin_delta_listing().unwrap();
    assert_ne!(again.id, stopped.id);
    assert_eq!(under_way(&mut store), Some(ListingUnderWay { next_link: None, ..again.clone() }));
    assert!(is_refused(store.item_store().write_listed_changes(stopped.id, &[file("C", "R", "c", "1")], 0, &delta_link("link-1"))));
    assert_eq!((bound(&mut store, "C"), store.delta_link().unwrap().as_deref()), (None, Some("link-0")));
    store.item_store().write_listed_changes(again.id, &[file("C", "R", "c", "1")], 0, &delta_link("link-1")).unwrap();
    assert_eq!((under_way(&mut store), store.delta_link().unwrap().as_deref()), (None, Some("link-1")));
}

/// What else the entry refuses whole, with nothing written: a page in a store that does
/// not know its drive; one whose fetch is said to have begun at a count the store has not
/// reached; and one whose fetch began before its listing did.
#[test]
fn a_page_the_entry_cannot_write_rightly_is_refused_whole() {
    let page = [root(), file("A", "R", "a", "1")];
    let mut no_drive = TreeStore::in_memory().unwrap();
    let begun = no_drive.item_store().begin_full_listing().unwrap();
    assert!(is_refused(no_drive.item_store().write_listed_changes(begun.id, &page, 0, &delta_link("link-1"))));
    assert_eq!((no_drive.delta_link().unwrap(), every_row(&mut no_drive).len()), (None, 1));

    let mut store = listed_store();
    let changes = store.item_store().begin_delta_listing().unwrap();
    assert!(is_refused(store.item_store().write_listed_changes(changes.id, &page, 1, &next_link("next-1"))), "the store counted no confirmation yet");
    write(&mut store, &[file("B", "R", "b", "1")]);
    let b = listed(&mut store, "B").local_item_id;
    let older = count(&mut store);
    confirm(&mut store, b, version(ROOT, "b", Some("22")), Some("e-B-22"));
    let full = store.item_store().begin_full_listing().unwrap();
    assert!(older < full.began_at_count);
    let before = whole(&mut store);
    assert!(is_refused(store.item_store().write_listed_changes(full.id, &page, older, &delta_link("link-1"))), "a page fetched before its listing began");
    assert_eq!(whole(&mut store), before);
    assert_eq!(bound(&mut store, "A"), None);
}
