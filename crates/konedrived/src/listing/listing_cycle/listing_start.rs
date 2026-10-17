//! How a listing cycle begins, and what a page OneDrive refuses leads to: the two
//! decisions of a cycle, as functions of what the store and Graph said.
//!
//! **State owned:** none.
//!
//! **Tested without the rest**, a test per case:
// test: no_delta_link_and_nothing_under_way_begins_a_full_listing
// test: a_full_listing_under_way_goes_on_from_the_next_link_the_store_kept
// test: a_full_listing_under_way_with_no_next_link_is_given_up_for_a_full_listing_begun_anew
// test: a_listing_of_changes_under_way_with_no_delta_link_begins_a_full_listing
// test: a_delta_link_begins_a_listing_of_changes_whatever_listing_of_changes_was_under_way
// test: a_full_listing_asked_for_wins_over_a_delta_link_and_not_over_one_under_way
// test: a_410_begins_a_full_listing
// test: a_refused_next_link_the_store_kept_begins_a_full_listing
// test: a_refused_delta_link_fails_the_cycle
// test: a_refused_link_of_this_cycle_fails_the_cycle
// test: a_second_refusal_in_one_cycle_begins_no_second_full_listing
// test: any_other_trouble_with_graph_fails_the_cycle

use konedrive_graph::drive::DriveError;
use konedrive_tree::{ListingKind, ListingUnderWay};

/// How a cycle begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ListingStart {
    /// A full listing was stopped after a page of it wrote the link to the next one: it
    /// goes on with its id, from that link (`next_link`, the store's own, carried here so
    /// that no full listing goes on without one).
    GoOnWithFullListing { listing: ListingUnderWay, next_link: String },
    /// A full listing begins, with an id of its own: the first one, one that was asked
    /// for, and one in place of a full listing under way that kept no next link.
    BeginFullListing,
    /// The changes since the store's delta link. A listing of changes that was stopped is
    /// given up for it: what was stopped is asked again from the delta link.
    BeginDeltaListing { delta_link: String },
}

/// How the cycle begins, from the listing under way
/// (`ItemReader::find_listing_under_way`), the delta link and whether a full listing was
/// asked for (`ItemReader::is_full_listing_requested`). Pure.
///
/// **One listing id names one enumeration of OneDrive.** A full listing marks the rows
/// its pages name with its id, and its end takes every row that does not carry it: a mark
/// must come from the enumeration whose end sweeps. So a request from the start
/// (`DeltaFrom::Start`), which is a new enumeration, is always a new listing
/// ([`ListingStart::BeginFullListing`], `ItemStore::begin_full_listing`), and the id of a
/// listing under way is used again only to ask the next link the store kept of it.
///
/// A full listing under way with a next link goes on whatever else is said: the store has
/// no delta link while one is. One with **no** next link is given up for a new one. "No
/// link written" is not "no page written": a first page that left an item to be read again
/// wrote its other rows, marked, and no link; asked again from the start under the same
/// id, the end would spare an item that page named and OneDrive removed since.
///
/// **What going on from a kept next link relies on.** The same may be so of a later page:
/// some of its rows written and marked, its link not, and the page asked a second time
/// from the next link before it. The second answer is then taken as a page of the same
/// enumeration. This rests on Graph alone: that a next link asked again goes on with the
/// enumeration it was given in, so that a removal of an item a page of that enumeration
/// named is said by a later page of it or by the changes since the delta link it ends
/// with — as for an item removed while a listing that was never stopped goes on. It was
/// not measured; a next link Graph refuses begins a full listing anew
/// ([`decide_page_refusal`]).
pub(crate) fn decide_listing_start(under_way: Option<&ListingUnderWay>, delta_link: Option<&str>, is_full_listing_requested: bool) -> ListingStart {
    match (under_way, delta_link) {
        (Some(listing), _) if listing.kind == ListingKind::Full => match &listing.next_link {
            Some(next_link) => ListingStart::GoOnWithFullListing { listing: listing.clone(), next_link: next_link.clone() },
            None => ListingStart::BeginFullListing,
        },
        _ if is_full_listing_requested => ListingStart::BeginFullListing,
        (_, Some(delta_link)) => ListingStart::BeginDeltaListing { delta_link: delta_link.to_owned() },
        (_, None) => ListingStart::BeginFullListing,
    }
}

/// Where the link of a refused request came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageLinkOrigin {
    /// The delta link the store kept: where a listing of changes begins.
    KeptDeltaLink,
    /// The next link the store kept of a full listing that was stopped: where it goes on.
    KeptNextLink,
    /// A page of this cycle gave it.
    GivenInThisCycle,
}

/// Whether this cycle may still begin a full listing because OneDrive refused a link: it
/// does so once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FullListingAllowance {
    /// No refused link began a full listing in this cycle.
    Unused,
    /// A refused link began one already.
    Used,
}

/// What a refused page leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AfterPageRefusal {
    /// A full listing begins in this cycle (`ItemStore::begin_full_listing`, which gives
    /// up the listing under way: a page of it that comes later is refused).
    BeginFullListing,
    /// The cycle fails; the next one goes on from what the store kept.
    FailCycle,
}

/// What `error` leads to. Pure: 410 (`ResyncRequired`, `ResyncUpload`) begins a full
/// listing, whatever link it answers; so does the next link the store kept of a stopped
/// full listing that OneDrive will not go on with. A delta
/// link refused otherwise fails the cycle: the link is
/// kept, and a whole drive is not listed again for one refused request. A link this
/// cycle was given and anything else fail the cycle too. And a full listing is begun for
/// a refused link once in a cycle (`allowance`): a second refusal fails the cycle, and
/// the next cycle decides anew ([`decide_listing_start`]).
pub(crate) fn decide_page_refusal(error: &DriveError, origin: PageLinkOrigin, allowance: FullListingAllowance) -> AfterPageRefusal {
    match (allowance, error, origin) {
        (FullListingAllowance::Used, _, _) => AfterPageRefusal::FailCycle,
        (FullListingAllowance::Unused, DriveError::ResyncRequired | DriveError::ResyncUpload, _) => AfterPageRefusal::BeginFullListing,
        (FullListingAllowance::Unused, DriveError::NotFound | DriveError::Failed(_), PageLinkOrigin::KeptNextLink) => AfterPageRefusal::BeginFullListing,
        (FullListingAllowance::Unused, _, _) => AfterPageRefusal::FailCycle,
    }
}

#[cfg(test)]
mod tests;
