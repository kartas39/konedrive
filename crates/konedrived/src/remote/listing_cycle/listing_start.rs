//! How a listing cycle begins, and what a page OneDrive refuses leads to: the two
//! decisions of a cycle, as functions of what the store and Graph said.
//!
//! **State owned:** none.
//!
//! **Tested without the rest**, a test per case:
// test: no_delta_link_and_nothing_under_way_begins_a_full_listing
// test: a_full_listing_under_way_goes_on_from_its_next_link_or_from_the_start
// test: a_delta_link_begins_a_listing_of_changes_whatever_listing_of_changes_was_under_way
// test: a_full_listing_asked_for_wins_over_a_delta_link_and_not_over_one_under_way
// test: a_410_begins_a_full_listing
// test: a_refused_next_link_the_store_kept_begins_a_full_listing
// test: a_refused_delta_link_fails_the_cycle
// test: a_refused_link_of_this_cycle_fails_the_cycle
// test: any_other_trouble_with_graph_fails_the_cycle

use konedrive_graph::drive::DriveError;
use konedrive_tree::{ListingKind, ListingUnderWay};

/// How a cycle begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListingStart {
    /// A full listing was stopped: it goes on with its id, from the link the store kept,
    /// or from the start when no page of it was written.
    GoOnWithFullListing { listing: ListingUnderWay },
    /// A full listing begins: the first one, or one that was asked for.
    BeginFullListing,
    /// The changes since the store's delta link. A listing of changes that was stopped is
    /// given up for it: what was stopped is asked again from the delta link.
    BeginDeltaListing { delta_link: String },
}

/// How the cycle begins, from the listing under way
/// (`ItemReader::find_listing_under_way`), the delta link and whether a full listing was
/// asked for (`ItemReader::is_full_listing_requested`). Pure. A full listing under way
/// goes on whatever else is said: the store has no delta link while one is.
pub fn decide_listing_start(under_way: Option<&ListingUnderWay>, delta_link: Option<&str>, is_full_listing_requested: bool) -> ListingStart {
    match (under_way, delta_link) {
        (Some(listing), _) if listing.kind == ListingKind::Full => ListingStart::GoOnWithFullListing { listing: listing.clone() },
        _ if is_full_listing_requested => ListingStart::BeginFullListing,
        (_, Some(delta_link)) => ListingStart::BeginDeltaListing { delta_link: delta_link.to_owned() },
        (_, None) => ListingStart::BeginFullListing,
    }
}

/// Where the link of a refused request came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageLinkOrigin {
    /// The delta link the store kept: where a listing of changes begins.
    KeptDeltaLink,
    /// The next link the store kept of a full listing that was stopped: where it goes on.
    KeptNextLink,
    /// A page of this cycle gave it.
    GivenInThisCycle,
}

/// What a refused page leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterPageRefusal {
    /// A full listing begins in this cycle (`ItemStore::begin_full_listing`, which gives
    /// up the listing under way: a page of it that comes later is refused).
    BeginFullListing,
    /// The cycle fails; the next one goes on from what the store kept.
    FailCycle,
}

/// What `error` leads to. Pure: 410 (`ResyncRequired`, `ResyncUpload`) begins a full
/// listing, whatever link it answers; so does the next link the store kept of a stopped
/// full listing that OneDrive will not go on with (today `fetch.rs:118-123`). A delta
/// link refused otherwise fails the cycle, as today (`fetch.rs:190-201`): the link is
/// kept, and a whole drive is not listed again for one refused request. A link this
/// cycle was given and anything else fail the cycle too.
pub fn decide_page_refusal(error: &DriveError, origin: PageLinkOrigin) -> AfterPageRefusal {
    match (error, origin) {
        (DriveError::ResyncRequired | DriveError::ResyncUpload, _) => AfterPageRefusal::BeginFullListing,
        (DriveError::NotFound | DriveError::Failed(_), PageLinkOrigin::KeptNextLink) => AfterPageRefusal::BeginFullListing,
        _ => AfterPageRefusal::FailCycle,
    }
}

#[cfg(test)]
mod tests;
