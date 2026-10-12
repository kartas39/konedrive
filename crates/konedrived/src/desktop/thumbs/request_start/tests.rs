use super::*;

/// The facts with which a request starts: every test changes one of them.
fn free() -> RequestFacts {
    RequestFacts { listing: ListingProgress::Ended, throttle_left: Duration::ZERO, is_file_being_opened: false, is_transfer_waiting: false, requests_under_way: 0 }
}

#[test]
fn a_request_starts_when_nothing_is_in_its_way() {
    assert_eq!(decide_request_start(&free()), RequestStart::Start);
    let one_under_way = RequestFacts { requests_under_way: REQUESTS_AT_ONCE - 1, ..free() };
    assert_eq!(decide_request_start(&one_under_way), RequestStart::Start, "below the limit");
}

#[test]
fn none_starts_before_a_full_listing_ended() {
    let facts = RequestFacts { listing: ListingProgress::FullNotEnded, ..free() };
    assert_eq!(decide_request_start(&facts), RequestStart::Wait(RequestWait::FullListingNotEnded));
}

#[test]
fn none_starts_while_a_listing_is_under_way() {
    let facts = RequestFacts { listing: ListingProgress::UnderWay, ..free() };
    assert_eq!(decide_request_start(&facts), RequestStart::Wait(RequestWait::ListingUnderWay));
}

#[test]
fn none_starts_during_a_throttles_wait() {
    let left = Duration::from_secs(300);
    let facts = RequestFacts { throttle_left: left, ..free() };
    assert_eq!(decide_request_start(&facts), RequestStart::Wait(RequestWait::Throttled { left }));
}

#[test]
fn none_starts_while_a_file_is_being_opened() {
    let facts = RequestFacts { is_file_being_opened: true, ..free() };
    assert_eq!(decide_request_start(&facts), RequestStart::Wait(RequestWait::FileBeingOpened));
}

#[test]
fn none_starts_while_a_transfer_waits_for_a_slot() {
    let facts = RequestFacts { is_transfer_waiting: true, ..free() };
    assert_eq!(decide_request_start(&facts), RequestStart::Wait(RequestWait::TransferWaiting));
}

#[test]
fn none_starts_at_the_limit_of_requests_at_once() {
    for requests_under_way in [REQUESTS_AT_ONCE, REQUESTS_AT_ONCE + 1] {
        let facts = RequestFacts { requests_under_way, ..free() };
        assert_eq!(decide_request_start(&facts), RequestStart::Wait(RequestWait::RequestsAtLimit));
    }
}

/// With several reasons at once the first in the order of the cases is the one named: the
/// listing's end is what nudges the filler, so it comes before what is only waited out.
#[test]
fn the_first_reason_in_the_order_of_the_cases_is_the_one_named() {
    let all = RequestFacts { listing: ListingProgress::FullNotEnded, throttle_left: Duration::from_secs(5), is_file_being_opened: true, is_transfer_waiting: true, requests_under_way: REQUESTS_AT_ONCE };
    assert_eq!(decide_request_start(&all), RequestStart::Wait(RequestWait::FullListingNotEnded));
    let listed = RequestFacts { listing: ListingProgress::Ended, ..all };
    assert_eq!(decide_request_start(&listed), RequestStart::Wait(RequestWait::Throttled { left: Duration::from_secs(5) }));
    let unthrottled = RequestFacts { throttle_left: Duration::ZERO, ..listed };
    assert_eq!(decide_request_start(&unthrottled), RequestStart::Wait(RequestWait::FileBeingOpened));
    let none_opened = RequestFacts { is_file_being_opened: false, ..unthrottled };
    assert_eq!(decide_request_start(&none_opened), RequestStart::Wait(RequestWait::TransferWaiting));
}
