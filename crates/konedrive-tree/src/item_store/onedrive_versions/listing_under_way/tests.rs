use super::{decide_page_end, LeftToReadAgain, ListingKind, ListingLink, ListingUnderWay, PageEnd, PageProgress};
use crate::item_store::tests::is_refused;
use crate::{meta, TreeStore};

fn next_link() -> ListingLink {
    ListingLink::NextLink("next".to_owned())
}

fn delta_link() -> ListingLink {
    ListingLink::DeltaLink("link".to_owned())
}

/// While something is left to be read again no link is written: whatever the link, the
/// listing's kind, and whether a full listing has swept.
#[test]
fn a_page_that_leaves_something_to_read_again_writes_no_link() {
    for kind in [ListingKind::Full, ListingKind::Delta] {
        for link in [next_link(), delta_link()] {
            for written in [PageProgress::Changes, PageProgress::ChangesAndSweep] {
                assert_eq!(decide_page_end(kind, &link, written, LeftToReadAgain::Something), PageEnd::LinkNotWritten, "{kind:?} {link:?} {written:?}");
            }
        }
    }
}

/// A page followed by a link to the next one, that left nothing: the link is written, in
/// a full listing and in a listing of changes.
#[test]
fn a_page_followed_by_a_link_to_the_next_one_writes_it() {
    for kind in [ListingKind::Full, ListingKind::Delta] {
        assert_eq!(decide_page_end(kind, &next_link(), PageProgress::Changes, LeftToReadAgain::Nothing), PageEnd::NextLinkWritten, "{kind:?}");
    }
}

/// The last page of a listing of changes, that left nothing: the listing ends.
#[test]
fn the_last_page_of_a_listing_of_changes_ends_it() {
    assert_eq!(decide_page_end(ListingKind::Delta, &delta_link(), PageProgress::Changes, LeftToReadAgain::Nothing), PageEnd::ListingEnds);
}

/// The last page of a full listing, that left nothing: the listing sweeps before it ends.
#[test]
fn the_last_page_of_a_full_listing_sweeps_first() {
    assert_eq!(decide_page_end(ListingKind::Full, &delta_link(), PageProgress::Changes, LeftToReadAgain::Nothing), PageEnd::FullListingSweeps);
}

/// A full listing that swept and left nothing ends; one whose sweep left something is the
/// first case above.
#[test]
fn a_full_listing_whose_sweep_left_nothing_ends() {
    assert_eq!(decide_page_end(ListingKind::Full, &delta_link(), PageProgress::ChangesAndSweep, LeftToReadAgain::Nothing), PageEnd::ListingEnds);
}

/// The rows that are a binding and nothing else stay by their stamp: at a listing's end
/// those stamped after the count it began at, and at every other end of a page all that
/// were ever stamped.
#[test]
fn the_rows_of_confirmed_removals_go_only_at_the_end_of_a_listing_that_began_after_them() {
    let mut store = TreeStore::in_memory().unwrap();
    store.set_meta(meta::CONFIRMATION_COUNT, Some("7")).unwrap();
    let tx = store.conn.transaction().unwrap();
    let listing = ListingUnderWay::begin(&tx, ListingKind::Full).unwrap();
    assert_eq!(listing.began_at_count, 7);
    assert_eq!(listing.binding_alone_left_after_count(PageEnd::ListingEnds), 7);
    for end in [PageEnd::LinkNotWritten, PageEnd::NextLinkWritten, PageEnd::FullListingSweeps] {
        assert_eq!(listing.binding_alone_left_after_count(end), 0, "{end:?}");
    }
}

/// What `meta` keeps of a listing, from its beginning to its end, each written by its
/// owner: a listing that begins has the next id, the count of confirmations, its kind and
/// no link to a next page, and a full one forgets the delta link; a page's end writes
/// nothing, the link to the next page, or the delta link, with which nothing is under way.
#[test]
fn a_listing_is_kept_from_its_beginning_to_its_end() {
    let mut store = TreeStore::in_memory().unwrap();
    store.set_meta(meta::DELTA_LINK, Some("link-0")).unwrap();
    let tx = store.conn.transaction().unwrap();
    assert_eq!(ListingUnderWay::find(&tx).unwrap(), None);

    let changes = ListingUnderWay::begin(&tx, ListingKind::Delta).unwrap();
    assert_eq!((changes.kind, changes.began_at_count, changes.next_link.clone()), (ListingKind::Delta, 0, None));
    assert_eq!(ListingUnderWay::find(&tx).unwrap(), Some(changes.clone()));
    assert_eq!(meta::get(&tx, meta::DELTA_LINK).unwrap().as_deref(), Some("link-0"), "a listing of changes keeps the link it is asked from");

    changes.write_page_end(&tx, PageEnd::LinkNotWritten, &next_link()).unwrap();
    changes.write_page_end(&tx, PageEnd::FullListingSweeps, &delta_link()).unwrap();
    assert_eq!(ListingUnderWay::find(&tx).unwrap(), Some(changes.clone()));
    changes.write_page_end(&tx, PageEnd::NextLinkWritten, &next_link()).unwrap();
    assert_eq!(ListingUnderWay::find(&tx).unwrap(), Some(ListingUnderWay { next_link: Some("next".to_owned()), ..changes.clone() }));

    let full = ListingUnderWay::begin(&tx, ListingKind::Full).unwrap();
    assert!(full.id > changes.id);
    assert_eq!(ListingUnderWay::find(&tx).unwrap(), Some(ListingUnderWay { id: full.id, kind: ListingKind::Full, began_at_count: 0, next_link: None }));
    assert_eq!(meta::get(&tx, meta::DELTA_LINK).unwrap(), None);
    assert!(is_refused(ListingUnderWay::begin(&tx, ListingKind::Delta)), "no delta link while a full listing is under way");
    assert_eq!(ListingUnderWay::find(&tx).unwrap().map(|listing| listing.id), Some(full.id));

    full.write_page_end(&tx, PageEnd::ListingEnds, &delta_link()).unwrap();
    assert_eq!((ListingUnderWay::find(&tx).unwrap(), meta::get(&tx, meta::DELTA_LINK).unwrap().as_deref()), (None, Some("link")));
    let next = ListingUnderWay::begin(&tx, ListingKind::Delta).unwrap();
    assert!(next.id > full.id, "an id is not given twice");
    assert_eq!(next.kind, ListingKind::Delta);
}

/// A page is its listing's when it comes with the id of the listing under way: refused
/// when none is under way, and when another one is.
#[test]
fn a_page_is_of_the_listing_under_way_by_its_id() {
    let mut store = TreeStore::in_memory().unwrap();
    let tx = store.conn.transaction().unwrap();
    let first = ListingUnderWay::begin(&tx, ListingKind::Full).unwrap();
    assert_eq!(ListingUnderWay::find_of_page(&tx, first.id, 0).unwrap(), first);
    let second = ListingUnderWay::begin(&tx, ListingKind::Full).unwrap();
    assert!(is_refused(ListingUnderWay::find_of_page(&tx, first.id, 0)));
    assert_eq!(ListingUnderWay::find_of_page(&tx, second.id, 0).unwrap(), second);
    assert!(is_refused(ListingUnderWay::find_of_page(&tx, second.id, 1)), "a count the store has not reached");
    second.write_page_end(&tx, PageEnd::ListingEnds, &delta_link()).unwrap();
    assert!(is_refused(ListingUnderWay::find_of_page(&tx, second.id, 0)), "none is under way");
}
