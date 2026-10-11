use konedrive_graph::drive::{Detail, DriveError};
use konedrive_tree::{ItemStore, ListingKind, ListingUnderWay, TreeStore};

use super::*;

/// A listing of `kind` the store has under way, with `next_link` kept.
fn listing_under_way(kind: ListingKind, next_link: Option<&str>) -> ListingUnderWay {
    let mut store = TreeStore::in_memory().expect("a store");
    store.set_drive_id("d1").expect("the drive");
    let mut items: ItemStore<'_> = store.item_store();
    let mut listing = items.begin_full_listing().expect("a full listing");
    listing.kind = kind;
    listing.next_link = next_link.map(str::to_owned);
    listing
}

fn failed() -> DriveError {
    DriveError::Failed(Detail::from("a link Graph does not take"))
}

fn transient() -> DriveError {
    DriveError::Transient(Detail::from("no connection"))
}

#[test]
fn no_delta_link_and_nothing_under_way_begins_a_full_listing() {
    assert_eq!(decide_listing_start(None, None, false), ListingStart::BeginFullListing);
    assert_eq!(decide_listing_start(None, None, true), ListingStart::BeginFullListing);
}

#[test]
fn a_full_listing_under_way_goes_on_from_its_next_link_or_from_the_start() {
    for next_link in [Some("next"), None] {
        let listing = listing_under_way(ListingKind::Full, next_link);
        assert_eq!(decide_listing_start(Some(&listing), None, false), ListingStart::GoOnWithFullListing { listing: listing.clone() });
    }
}

#[test]
fn a_delta_link_begins_a_listing_of_changes_whatever_listing_of_changes_was_under_way() {
    let begun = ListingStart::BeginDeltaListing { delta_link: "delta".into() };
    assert_eq!(decide_listing_start(None, Some("delta"), false), begun);
    let stopped = listing_under_way(ListingKind::Delta, Some("next"));
    assert_eq!(decide_listing_start(Some(&stopped), Some("delta"), false), begun);
}

#[test]
fn a_full_listing_asked_for_wins_over_a_delta_link_and_not_over_one_under_way() {
    assert_eq!(decide_listing_start(None, Some("delta"), true), ListingStart::BeginFullListing);
    let stopped = listing_under_way(ListingKind::Delta, Some("next"));
    assert_eq!(decide_listing_start(Some(&stopped), Some("delta"), true), ListingStart::BeginFullListing);
    let full = listing_under_way(ListingKind::Full, Some("next"));
    assert_eq!(decide_listing_start(Some(&full), None, true), ListingStart::GoOnWithFullListing { listing: full.clone() });
}

#[test]
fn a_410_begins_a_full_listing() {
    for origin in [PageLinkOrigin::KeptDeltaLink, PageLinkOrigin::KeptNextLink, PageLinkOrigin::GivenInThisCycle] {
        assert_eq!(decide_page_refusal(&DriveError::ResyncRequired, origin), AfterPageRefusal::BeginFullListing);
        assert_eq!(decide_page_refusal(&DriveError::ResyncUpload, origin), AfterPageRefusal::BeginFullListing);
    }
}

#[test]
fn a_refused_next_link_the_store_kept_begins_a_full_listing() {
    assert_eq!(decide_page_refusal(&DriveError::NotFound, PageLinkOrigin::KeptNextLink), AfterPageRefusal::BeginFullListing);
    assert_eq!(decide_page_refusal(&failed(), PageLinkOrigin::KeptNextLink), AfterPageRefusal::BeginFullListing);
}

#[test]
fn a_refused_delta_link_fails_the_cycle() {
    assert_eq!(decide_page_refusal(&DriveError::NotFound, PageLinkOrigin::KeptDeltaLink), AfterPageRefusal::FailCycle);
    assert_eq!(decide_page_refusal(&failed(), PageLinkOrigin::KeptDeltaLink), AfterPageRefusal::FailCycle);
}

#[test]
fn a_refused_link_of_this_cycle_fails_the_cycle() {
    assert_eq!(decide_page_refusal(&DriveError::NotFound, PageLinkOrigin::GivenInThisCycle), AfterPageRefusal::FailCycle);
    assert_eq!(decide_page_refusal(&failed(), PageLinkOrigin::GivenInThisCycle), AfterPageRefusal::FailCycle);
}

#[test]
fn any_other_trouble_with_graph_fails_the_cycle() {
    for origin in [PageLinkOrigin::KeptDeltaLink, PageLinkOrigin::KeptNextLink, PageLinkOrigin::GivenInThisCycle] {
        assert_eq!(decide_page_refusal(&DriveError::SignedOut, origin), AfterPageRefusal::FailCycle);
        assert_eq!(decide_page_refusal(&transient(), origin), AfterPageRefusal::FailCycle);
        assert_eq!(decide_page_refusal(&DriveError::UrlExpired, origin), AfterPageRefusal::FailCycle);
    }
}
