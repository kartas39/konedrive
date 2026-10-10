//! The listing under way: what the store keeps of a listing from its beginning to its end,
//! in `meta`, and the one place those keys are read and written. A listing is begun here,
//! goes on here from page to page, and ends here; what ends a page is decided here
//! ([`decide_page_end`]), from typed cases and with nothing read.
//!
//! **A page says which listing it is a page of.** A listing gets an id when it begins
//! ([`ListingId`]): a full one and a listing of changes alike, each begun by its own call.
//! The caller gives the id back with every page, and a page with another id than that of
//! the listing under way is refused whole: a page of a listing the store gave up for a
//! later one, or of one that ended. No page begins a listing by itself, so the pages of a
//! full listing that was not begun cannot be taken as a listing of changes.
//!
//! One listing is under way at a time: one that begins gives up the one before. While a
//! full listing is under way the store has no delta link, so no listing of changes can be
//! begun beside it.

use rusqlite::{Connection, Transaction};

use super::confirmed_onedrive_version::read_confirmation_count;
use crate::item_store::{refuse, ItemStore};
use crate::{meta, TreeError};

impl ItemStore<'_> {
    /// A full listing begins: from now until a page with a
    /// [`DeltaLink`](ListingLink::DeltaLink) ends it, what the pages say is all OneDrive
    /// has. The listing, with the id its pages are given with and the count of
    /// confirmations it begins at, which the caller gives with its first pages. A listing
    /// that was under way is given up for this one, with the link to its next page: a page
    /// of it that comes later is refused. The delta link is forgotten in the same
    /// transaction: while a full listing is under way the store has none.
    pub fn begin_full_listing(&mut self) -> Result<ListingUnderWay, TreeError> {
        self.write(|tx| ListingUnderWay::begin(tx, ListingKind::Full))
    }

    /// A listing of changes begins: what is asked from the delta link the store has. The
    /// listing, with the id its pages are given with and the count of confirmations it
    /// begins at. A listing of changes that was under way is given up for this one, with
    /// the link to its next page: what was stopped is asked again from the delta link.
    /// Refused when the store has no delta link, as it has none before the first full
    /// listing ended and while one is under way: a full listing is begun then.
    pub fn begin_delta_listing(&mut self) -> Result<ListingUnderWay, TreeError> {
        self.write(|tx| ListingUnderWay::begin(tx, ListingKind::Delta))
    }

    /// The listing the store was given the beginning of and not the end, each until a page
    /// with a [`DeltaLink`](ListingLink::DeltaLink) leaves nothing to read again. `None`
    /// when none is under way. While one is, the rows may say OneDrive does not have an
    /// item, or has it where the folder cannot show it, that a page still to come says
    /// otherwise of. A listing that was stopped goes on with the id found here.
    pub fn find_listing_under_way(&self) -> Result<Option<ListingUnderWay>, TreeError> {
        ListingUnderWay::find(&self.store.conn)
    }
}

/// What tells one listing from another: the number the store gave it when it began, which
/// no other listing of the store has. Given back with every page of the listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ListingId(u64);

impl ListingId {
    /// The number, as a full listing marks the rows it sees with.
    pub(crate) fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for ListingId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// What follows a page of a listing, in Graph's own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListingLink {
    /// `@odata.nextLink`: the listing goes on, and its next page is asked from this link.
    NextLink(String),
    /// `@odata.deltaLink`: the listing ended, and the next listing of changes is asked from
    /// this link.
    DeltaLink(String),
}

/// Whether a listing says all OneDrive has, or what changed since a delta link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListingKind {
    /// All OneDrive has: begun with [`ItemStore::begin_full_listing`]. What it does not see
    /// is gone.
    Full,
    /// What changed since the delta link the store had: begun with
    /// [`ItemStore::begin_delta_listing`].
    Delta,
}

/// A listing the store was given the beginning of and not the end
/// ([`ItemStore::find_listing_under_way`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingUnderWay {
    /// What its pages are given with.
    pub id: ListingId,
    pub kind: ListingKind,
    /// The count of confirmations the listing began at.
    pub began_at_count: u64,
    /// The link its next page is asked from, as the last page that left nothing to read
    /// again gave it; `None` before the first such page.
    pub next_link: Option<String>,
}

/// What becomes of the link that followed a page, and of the listing, once the page's
/// changes are written ([`decide_page_end`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum PageEnd {
    /// Something is left to be read again: no link is written and the listing has not
    /// ended. It goes on from the link the store had.
    LinkNotWritten,
    /// The listing goes on: the link to its next page is written.
    NextLinkWritten,
    /// The last page of a full listing left nothing to read again: the listing takes what
    /// it did not see, and what ends the page is decided again from what that left.
    FullListingSweeps,
    /// The listing ends: its delta link is written, and nothing is under way.
    ListingEnds,
}

/// How far the writing of a page got when its end is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum PageProgress {
    /// The page's changes are written.
    Changes,
    /// And a full listing took what it did not see.
    ChangesAndSweep,
}

/// Whether a call leaves an item to be read again from OneDrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum LeftToReadAgain {
    Nothing,
    Something,
}

/// What ends a page of a listing of `kind` that was followed by `link`. The first that
/// holds: while something is left to be read again no link is written, whatever the link;
/// a link to a next page is written; a full listing's last page sweeps before it ends; a
/// listing ends.
pub(super) fn decide_page_end(kind: ListingKind, link: &ListingLink, written: PageProgress, left: LeftToReadAgain) -> PageEnd {
    match (left, link, kind, written) {
        (LeftToReadAgain::Something, _, _, _) => PageEnd::LinkNotWritten,
        (LeftToReadAgain::Nothing, ListingLink::NextLink(_), _, _) => PageEnd::NextLinkWritten,
        (LeftToReadAgain::Nothing, ListingLink::DeltaLink(_), ListingKind::Full, PageProgress::Changes) => PageEnd::FullListingSweeps,
        (LeftToReadAgain::Nothing, ListingLink::DeltaLink(_), ListingKind::Full, PageProgress::ChangesAndSweep) => PageEnd::ListingEnds,
        (LeftToReadAgain::Nothing, ListingLink::DeltaLink(_), ListingKind::Delta, _) => PageEnd::ListingEnds,
    }
}

/// A count the store keeps one of; 0 before it was first raised.
fn read_count(conn: &Connection, key: &str) -> Result<u64, TreeError> {
    Ok(meta::get(conn, key)?.and_then(|count| count.parse().ok()).unwrap_or(0))
}

impl ListingUnderWay {
    /// [`ItemStore::find_listing_under_way`]: a listing is under way while the store keeps
    /// its id.
    pub(in crate::item_store) fn find(conn: &Connection) -> Result<Option<Self>, TreeError> {
        let Some(id) = meta::get(conn, meta::LISTING_UNDER_WAY)?.and_then(|id| id.parse().ok()) else { return Ok(None) };
        let kind = if meta::get(conn, meta::FULL_LISTING_UNDER_WAY)?.is_some() { ListingKind::Full } else { ListingKind::Delta };
        Ok(Some(Self { id: ListingId(id), kind, began_at_count: read_count(conn, meta::LISTING_BEGAN_AT_COUNT)?, next_link: meta::get(conn, meta::LISTING_NEXT_LINK)? }))
    }

    /// A listing of `kind` begins, at the count of confirmations the store has reached,
    /// with the next id; the one that was under way is given up. A full listing forgets
    /// the delta link. Refused, with nothing written, for a listing of changes in a store
    /// that has no delta link.
    pub(super) fn begin(tx: &Transaction<'_>, kind: ListingKind) -> Result<Self, TreeError> {
        match kind {
            ListingKind::Full => meta::set(tx, meta::DELTA_LINK, None)?,
            ListingKind::Delta if meta::get(tx, meta::DELTA_LINK)?.is_none() => {
                return Err(refuse("the store has no delta link to ask a listing of changes from: a full listing is begun".to_owned()));
            }
            ListingKind::Delta => {}
        }
        let listing = Self { id: ListingId(read_count(tx, meta::LISTING_COUNT)? + 1), kind, began_at_count: read_confirmation_count(tx)?, next_link: None };
        meta::set(tx, meta::LISTING_COUNT, Some(&listing.id.to_string()))?;
        meta::set(tx, meta::LISTING_UNDER_WAY, Some(&listing.id.to_string()))?;
        meta::set(tx, meta::FULL_LISTING_UNDER_WAY, (kind == ListingKind::Full).then_some("1"))?;
        meta::set(tx, meta::LISTING_BEGAN_AT_COUNT, Some(&listing.began_at_count.to_string()))?;
        meta::set(tx, meta::LISTING_NEXT_LINK, None)?;
        Ok(listing)
    }

    /// The listing under way, for a page that says it is a page of `listing` and that its
    /// fetch began at the count `fetch_began_at_count`. Refused when the page is not that
    /// listing's: none is under way; another one is; the fetch is said to have begun at a
    /// count the store has not reached, or before the listing began.
    pub(super) fn find_of_page(conn: &Connection, listing: ListingId, fetch_began_at_count: u64) -> Result<Self, TreeError> {
        let Some(under_way) = Self::find(conn)? else {
            return Err(refuse(format!("no listing is under way: the page is of the listing {listing}, which ended or was never begun")));
        };
        if under_way.id != listing {
            return Err(refuse(format!("the page is of the listing {listing}, which the store gave up: the listing under way is {}", under_way.id)));
        }
        if fetch_began_at_count > read_confirmation_count(conn)? {
            return Err(refuse(format!("a fetch cannot have begun at the count {fetch_began_at_count}: the store has not counted so many confirmations")));
        }
        if fetch_began_at_count < under_way.began_at_count {
            return Err(refuse(format!("the page was fetched at the count {fetch_began_at_count}, before its listing began ({})", under_way.began_at_count)));
        }
        Ok(under_way)
    }

    /// The id a full listing marks the rows it sees with; `None` for a listing of changes,
    /// which marks none.
    pub(super) fn full_listing(&self) -> Option<ListingId> {
        (self.kind == ListingKind::Full).then_some(self.id)
    }

    /// The count after which a row that is a binding and nothing else was stamped to be
    /// left at this end of a page: at a listing's end the count it began at, as every page
    /// of it passed the rows of the removals confirmed before; while it goes on 0, so that
    /// only a row that was never stamped goes.
    pub(super) fn binding_alone_left_after_count(&self, end: PageEnd) -> u64 {
        match end {
            PageEnd::ListingEnds => self.began_at_count,
            PageEnd::LinkNotWritten | PageEnd::NextLinkWritten | PageEnd::FullListingSweeps => 0,
        }
    }

    /// Writes what `end` says becomes of `link` and of the listing: nothing; the link to
    /// its next page; or the delta link, with which nothing is under way any more.
    pub(super) fn write_page_end(&self, tx: &Transaction<'_>, end: PageEnd, link: &ListingLink) -> Result<(), TreeError> {
        let (ListingLink::NextLink(text) | ListingLink::DeltaLink(text)) = link;
        match end {
            PageEnd::LinkNotWritten | PageEnd::FullListingSweeps => {}
            PageEnd::NextLinkWritten => meta::set(tx, meta::LISTING_NEXT_LINK, Some(text))?,
            PageEnd::ListingEnds => {
                meta::set(tx, meta::DELTA_LINK, Some(text))?;
                for key in [meta::LISTING_UNDER_WAY, meta::FULL_LISTING_UNDER_WAY, meta::LISTING_BEGAN_AT_COUNT, meta::LISTING_NEXT_LINK] {
                    meta::set(tx, key, None)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
