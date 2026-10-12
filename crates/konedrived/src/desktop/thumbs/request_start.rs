//! Whether a thumbnail request may start now.
//!
//! **State owned:** none: one pure function over typed facts, read by the filler from the
//! store (the listing) and the account's transfer pool (the throttle, the files being
//! opened, what waits for a slot), with the filler's own count of requests under way.
//!
//! **Answers for:** that thumbnails stay far below what OneDrive throttles an account at,
//! and give way to everything else. A thumbnail is a request to the Graph API, like a
//! page of the listing, not a download from the content host: it takes no slot of the
//! pool and does not make the pool grow. It has a limit of its own
//! ([`REQUESTS_AT_ONCE`]), and none starts
//!
//! - before the folder's first listing ended, or while any listing is under way;
//! - during a throttle's `Retry-After`;
//! - while a file is being opened, or an open waits for its slot;
//! - while an upload, a download of a pinned file or a metadata operation waits for a
//!   slot of the pool.

use std::time::Duration;

/// Thumbnail requests under way at once, at most. A choice: a request takes about a
/// second (seen on a live account: 820 a minute through 16 slots, 1403 to 2142 a minute
/// through 32), so two at once are about two requests a second — a seventh of the rate
/// that went on for a minute before OneDrive answered `429`, which leaves the account's
/// allowance to the listing and to what is sent; a few thousand photos still get their
/// thumbnails within an hour.
pub(crate) const REQUESTS_AT_ONCE: usize = 2;

/// How long the filler waits before it asks again whether a request may start, while it
/// gives way to an open or to what waits at the pool, or is at its limit. A choice: the
/// pool tells nobody that its line emptied, a thumbnail is in no hurry, and reading three
/// counts of the pool every two seconds costs nothing.
pub(crate) const ASKED_AGAIN_AFTER: Duration = Duration::from_secs(2);

/// How far the folder's listing is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListingProgress {
    /// The folder's first listing has not ended, or a full listing asked for later has
    /// not: the store has no delta link.
    FullNotEnded,
    /// A listing of changes is under way.
    UnderWay,
    /// The last listing ended and none is under way.
    Ended,
}

/// What the decision is made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestFacts {
    pub(crate) listing: ListingProgress,
    /// What is left of OneDrive's `Retry-After`; zero when the account is not throttled.
    pub(crate) throttle_left: Duration,
    /// A file being opened holds a slot of the pool.
    pub(crate) is_file_being_opened: bool,
    /// Something waits for a slot of the pool: an open, a metadata operation, an upload
    /// or a download of a pinned file.
    pub(crate) is_transfer_waiting: bool,
    /// The filler's thumbnail requests under way.
    pub(crate) requests_under_way: usize,
}

/// Why no thumbnail request starts now; the first that holds, in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestWait {
    /// The folder's first listing has not ended, or a later full one has not. Ends with
    /// the listing, which nudges the filler.
    FullListingNotEnded,
    /// A listing is under way. Ends with the listing, which nudges the filler.
    ListingUnderWay,
    /// OneDrive asked to wait, this much longer.
    Throttled { left: Duration },
    /// A file is being opened.
    FileBeingOpened,
    /// Something waits for a slot of the pool.
    TransferWaiting,
    /// [`REQUESTS_AT_ONCE`] requests are under way.
    RequestsAtLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestStart {
    Start,
    Wait(RequestWait),
}

/// Whether a thumbnail request may start, given `facts`. Pure.
pub(crate) fn decide_request_start(facts: &RequestFacts) -> RequestStart {
    let wait = match facts.listing {
        ListingProgress::FullNotEnded => Some(RequestWait::FullListingNotEnded),
        ListingProgress::UnderWay => Some(RequestWait::ListingUnderWay),
        ListingProgress::Ended if !facts.throttle_left.is_zero() => Some(RequestWait::Throttled { left: facts.throttle_left }),
        ListingProgress::Ended if facts.is_file_being_opened => Some(RequestWait::FileBeingOpened),
        ListingProgress::Ended if facts.is_transfer_waiting => Some(RequestWait::TransferWaiting),
        ListingProgress::Ended if facts.requests_under_way >= REQUESTS_AT_ONCE => Some(RequestWait::RequestsAtLimit),
        ListingProgress::Ended => None,
    };
    wait.map_or(RequestStart::Start, RequestStart::Wait)
}

#[cfg(test)]
mod tests;
