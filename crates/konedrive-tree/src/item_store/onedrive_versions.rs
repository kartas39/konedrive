//! What a listing of OneDrive says of an item, written into its row: its OneDrive version
//! (place, and for a file content), the eTag and the placement. Nothing else of a row is
//! written here: not the local version, not what the two sides agreed on, not whether the
//! content is on disk. What then differs between the sides is the plan's to act on.
//!
//! This module answers for the writing of one page. Beside it, each with its own state:
//! what a listing says of one item and what becomes of it ([`listed_change`]); the listing
//! under way, from its beginning to its end ([`listing_under_way`]); and what OneDrive
//! confirmed to a step, with the count of confirmations ([`confirmed_onedrive_version`]).
//!
//! **One entry**, [`ItemStore::write_listed_changes`]: the changes of one page of a
//! listing, with the link that follows the page, in one transaction. The changes come in
//! any order, and are written in an order of the entry's own, in which no statement is
//! refused by the schema whatever the page holds:
//!
//! 1. Of several changes of one item, the last counts.
//! 2. The root. Row 1 is bound to it; a row that had its reference first, as a parent named
//!    before the root came, goes, and what is in it is in row 1. A root no row can be is
//!    refused like any other item: one with no id, or with the id of a file or of an item
//!    the folder shows.
//! 3. Every item the page says OneDrive has, each by itself: the row that has its item id
//!    and the row that has its parent's are found; what becomes of the change is decided
//!    from them ([`listed_change`]); and only for a change that is written a row is made
//!    for the parent when it has none (its first mention, with the binding alone) and for
//!    the item when it has none, and one statement writes the OneDrive columns of the
//!    item's row. The parent has a row and is a directory when that statement runs, which
//!    is all the schema asks of it. No index asks for one item per name in OneDrive, so
//!    two rows may say one place between two statements and between two pages, and a
//!    directory may be below what is below it.
//! 4. Every item the page says is gone, after all of the above: its row has no OneDrive
//!    version from then on, and neither has what the rows say OneDrive has below it, which
//!    a listing does not always name. What the same page moved out of a directory is not
//!    below it any more when this runs, and what it moved into it is; a change that was
//!    left ([`ListedChangeOutcome::Outdated`]) moved nothing, so its item is below still,
//!    and the removal is then not written (below).
//! 5. A row left with its binding and nothing else, that no row names as its parent, goes.
//!    Such a row names no parent itself, so no row that goes holds another.
//!
//! **What becomes of each change is answered** ([`ListedChangeOutcome`]): written; left,
//! because it may be older than what OneDrive confirmed; or refused, for the one item and
//! with its reason ([`ListedChangeRefusal`]), the rest of the page being written.
//!
//! **The guard against an outdated listing.** The store counts confirmations
//! ([`ItemStore::confirmation_count`]); a step that OneDrive confirmed something to writes
//! what was confirmed, with its eTag, and the row is stamped with the next count, in one
//! statement ([`confirmed_onedrive_version`]). A cycle reads the count when a fetch begins
//! and gives it with every page of that fetch. What a page says of a row stamped later may
//! be older than what was confirmed: it is taken only when it is what was confirmed, and
//! otherwise the row is left and the item is answered, to be read again from OneDrive. A
//! row whose removal was confirmed has no version on either side; it stays until a listing
//! ends that began at its stamp or after, so that no page fetched before the removal finds
//! the binding free and makes the item again.
//!
//! **A call takes nothing through a row it leaves to be read again.** What a page says of
//! the items below a directory stops at a row stamped later, at any depth.
//!
//! - *A removal is written whole or not at all.* When the walk down from an item listed
//!   gone meets such a row, nothing of the removal is written — not the item's own
//!   version, nothing below it — the change is [`ListedChangeOutcome::Outdated`], and the
//!   item and the rows that were met are answered. So every row is as it was, and the
//!   listing asked again after a stop names the removal again and writes it. Read again,
//!   the removed item is given back as gone, with what was read of the rows met.
//! - *A full listing's end* leaves an unseen row stamped after the listing began, answers
//!   it, and leaves what the rows say is below it with its OneDrive version until that row
//!   was read: gone, and its removal takes what is below; or somewhere, and the listing's
//!   end then takes what it did not see below it.
//!
//! **A listing, from its beginning to its end** is the store's to keep
//! ([`listing_under_way`]): a full listing (the first one, and after OneDrive asks to start
//! over) and a listing of changes are each begun by a call, which gives the listing's id;
//! every page is given with that id, and a page of another listing than the one under way
//! is refused whole. The store keeps, of the listing under way: its id; the count it began
//! at; the link to its next page, from which a stopped listing goes on; and that it is a
//! full one, each row whose version a page writes being marked as seen by it.
//!
//! **The link is written last.** A call that leaves something to read again writes no
//! link and ends no listing: the caller reads those items again and gives what it read
//! with the same link, and only a call that leaves nothing writes the link. A stop in
//! between loses nothing: the store has the link it had, and what is asked from it again
//! names the items again. With a [`ListingLink::DeltaLink`] the listing then ends: a full
//! listing takes what it did not see, as if listed gone, and the rows of removals
//! confirmed before the listing began go. Both are bounded by the count the listing began
//! at, not by the count of the call that ends it: a listing that was resumed, or whose
//! last call brings what was read again, has pages older than that call.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;

use rusqlite::Transaction;

use super::bindings::{bind_root, make_onedrive_item};
use super::item_row::{clear_onedrive_version, clear_onedrive_versions_below, clear_onedrive_versions_unseen, delete_rows_of_binding_alone, find_bound_item, list_left_below_in_onedrive, set_onedrive_etag, set_onedrive_version};
use super::{refuse, ItemStore};
use crate::model::{Change, ContentVersion, DriveId, Item, ItemKind, ItemPlace, ItemVersion, Kind, LocalItemId, OneDriveItemId, OneDriveItemReference, Row};
use crate::{meta, TreeError};

/// What OneDrive confirmed of an item to a step, written into its row; the count of
/// confirmations.
mod confirmed_onedrive_version;
/// What a listing says of one item, and what becomes of it.
mod listed_change;
/// The listing under way, from its beginning to its end; what ends a page.
mod listing_under_way;

pub use confirmed_onedrive_version::ConfirmedOneDriveVersion;
pub use listed_change::{ListedChangeOutcome, ListedChangeRefusal};
use listed_change::{decide_listed_item, decide_listed_removal, decide_listed_root, ListedItem};
pub use listing_under_way::{ListingId, ListingKind, ListingLink, ListingUnderWay};
use listing_under_way::{decide_page_end, LeftToReadAgain, PageEnd, PageProgress};

impl ItemStore<'_> {
    /// Writes what one page of the listing `listing` says into the rows, in one
    /// transaction: all of it or nothing. See the module for the order it writes in.
    ///
    /// `listing` is the id the store gave when the listing began
    /// ([`begin_full_listing`](Self::begin_full_listing),
    /// [`begin_delta_listing`](Self::begin_delta_listing)), or that of the listing found
    /// under way after a stop ([`find_listing_under_way`](Self::find_listing_under_way)).
    /// `fetch_began_at_count` is the count of confirmations
    /// ([`confirmation_count`](Self::confirmation_count)) the caller read when the fetch
    /// that brought the page began. `link` is what followed the page.
    ///
    /// The answer says what became of each change, and which items are to be read again
    /// from OneDrive because something was confirmed of them since the fetch began
    /// ([`ListedPageOutcome`]).
    ///
    /// **The order the caller keeps.** When the answer names no item to read again, the
    /// link is written with the page: a [`NextLink`](ListingLink::NextLink) as where the
    /// listing goes on, a [`DeltaLink`](ListingLink::DeltaLink) as the delta link, and
    /// with it the listing ends. When it names some, **the link is not written and the
    /// listing has not ended**: the caller reads each of those items again from OneDrive
    /// and calls again, with what it read as the changes (an item OneDrive has, or one
    /// that is gone), the count that reading began at, and the same link. An item answered
    /// may be a directory the page said is gone, whose removal was not written because a
    /// row below it is among those answered: it is read like any other, and given back as
    /// gone. Every item
    /// answered is given back so, as there or as gone: one that is not is answered again
    /// by the next call that reaches it, and a full listing does not end while it did not
    /// see such an item. The caller asks nothing from the link before a call has written
    /// it. If the reading fails or the daemon stops in between, nothing is lost: the store
    /// has the link it had before, the listing is asked from it again, and it names those
    /// items again.
    ///
    /// Refused, with nothing written: in a store that does not know its drive; when
    /// `listing` is not the listing under way — none is, or the store gave that one up for
    /// a later one; when `fetch_began_at_count` is a count the store has not reached; and
    /// when it is before the count the listing began at.
    pub fn write_listed_changes(&mut self, listing: ListingId, changes: &[Change], fetch_began_at_count: u64, link: &ListingLink) -> Result<ListedPageOutcome, TreeError> {
        self.write(|tx| write_listed_changes(tx, listing, changes, fetch_began_at_count, link))
    }
}

/// What became of one page of a listing given to the store
/// ([`ItemStore::write_listed_changes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedPageOutcome {
    /// What became of each change, in the order they were given. Of several changes of one
    /// item the last counts, and each of them has its outcome.
    pub changes: Vec<ListedChangeOutcome>,
    /// The items to read again from OneDrive: those of the changes that are
    /// [`Outdated`](ListedChangeOutcome::Outdated) — among them a removal that was not
    /// written because its walk met a row stamped later — and the items no change names:
    /// the rows such a walk met, and those a full listing that ends did not see though
    /// something was confirmed of them since it began.
    pub outdated_items: Vec<OneDriveItemId>,
}

impl ListedPageOutcome {
    /// Whether the page's link was written, and with a
    /// [`DeltaLink`](ListingLink::DeltaLink) the listing ended: only when nothing is left to
    /// read again.
    pub fn is_link_written(&self) -> bool {
        self.outdated_items.is_empty()
    }
}

/// A listing's folder is a directory, and its file a file.
fn item_kind_of(kind: Kind) -> ItemKind {
    match kind {
        Kind::File => ItemKind::File,
        Kind::Folder => ItemKind::Directory,
    }
}

/// What a listing's row says OneDrive has of an item, in the store's own types. A time
/// OneDrive gave is whole seconds; a file it gives no hash for is told by its cTag alone.
fn listed_item_of(row: &Row) -> ListedItem {
    let kind = item_kind_of(row.kind);
    let content = (kind == ItemKind::File).then(|| ContentVersion::Hashed { size: row.size, mtime: row.mtime, mtime_nsec: 0, hash: row.quickxor.clone(), ctag: row.ctag.clone() });
    ListedItem { item_id: row.id.clone(), parent_id: row.parent_id.clone().unwrap_or_default(), kind, name: OsString::from(&row.name), content, etag: row.etag.clone(), mime: row.mime.clone().filter(|_| kind == ItemKind::File), placement: row.placement }
}

/// The row of `item` says OneDrive has what `listed` says: a change that was decided to be
/// written. `row` and `parent_row` are the rows that have the item's id and its parent's;
/// one that is not there is made here, with the binding and the kind alone, the parent's
/// first, so that a parent has its row before its child whichever the page names first.
/// `seen_in_full_listing`: the id of the full listing under way, when one is.
fn write_listed_item(tx: &Transaction<'_>, listed: &ListedItem, row: Option<&Item>, parent_row: Option<&Item>, drive_id: &DriveId, seen_in_full_listing: Option<u64>) -> Result<(), TreeError> {
    let reference = |item_id: &str| OneDriveItemReference { drive_id: drive_id.clone(), item_id: OneDriveItemId::new(item_id) };
    let directory = match parent_row {
        Some(parent_row) => parent_row.local_item_id,
        None => make_onedrive_item(tx, &reference(&listed.parent_id), ItemKind::Directory)?,
    };
    let item = match row {
        Some(row) => row.local_item_id,
        None => make_onedrive_item(tx, &reference(&listed.item_id), listed.kind)?,
    };
    let onedrive = ItemVersion { place: Some(ItemPlace { parent_local_item_id: directory, name: listed.name.clone() }), content: listed.content.clone() };
    set_onedrive_version(tx, item, &onedrive, listed.etag.as_deref(), listed.mime.as_deref(), listed.placement, seen_in_full_listing)
}

/// [`ItemStore::write_listed_changes`], inside the caller's transaction, which is dropped
/// on any error. The only place of the module that holds what a listing hands over
/// ([`Change`] and its row): each is taken apart here into the item's id, its kind, its
/// version, eTag and placement.
pub(crate) fn write_listed_changes(tx: &Transaction<'_>, listing: ListingId, changes: &[Change], fetch_began_at_count: u64, link: &ListingLink) -> Result<ListedPageOutcome, TreeError> {
    let Some(drive_id) = meta::get(tx, meta::DRIVE_ID)?.and_then(DriveId::new) else {
        return Err(refuse("the store does not know its drive: a listing is not written".to_owned()));
    };
    let listing = ListingUnderWay::find_of_page(tx, listing, fetch_began_at_count)?;
    let full_listing = listing.full_listing().map(ListingId::as_u64);
    // The row that has an item id of OneDrive: none has the empty text for one.
    let find_row = |item_id: &str| -> Result<Option<Item>, TreeError> {
        if item_id.is_empty() {
            return Ok(None);
        }
        find_bound_item(tx, &OneDriveItemReference { drive_id: drive_id.clone(), item_id: OneDriveItemId::new(item_id) })
    };

    let left = |items: &[OneDriveItemId]| if items.is_empty() { LeftToReadAgain::Nothing } else { LeftToReadAgain::Something };

    // Of several changes of one item, the last counts.
    let mut last_change_of = HashMap::new();
    for (at, change) in changes.iter().enumerate() {
        last_change_of.insert(change.id(), at);
    }
    let counted = || changes.iter().enumerate().filter(|(at, change)| last_change_of.get(change.id()) == Some(at));
    let mut outcomes = vec![ListedChangeOutcome::Written; changes.len()];
    let mut outdated_items = Vec::new();

    for (at, change) in counted() {
        let Change::Root(listed_root) = change else { continue };
        outcomes[at] = decide_listed_root(&listed_root.id, find_row(&listed_root.id)?.as_ref());
        match outcomes[at] {
            ListedChangeOutcome::Written => {
                bind_root(tx, &OneDriveItemReference { drive_id: drive_id.clone(), item_id: OneDriveItemId::new(&listed_root.id) })?;
                set_onedrive_etag(tx, LocalItemId::ROOT, listed_root.etag.as_deref())?;
            }
            outcome => tracing::warn!("what the listing says of the root of the drive, {:?}, is not written: {outcome:?}", listed_root.id),
        }
    }

    for (at, change) in counted() {
        let Change::Upsert(listed_row) = change else { continue };
        let listed = listed_item_of(listed_row);
        let (row, parent_row) = (find_row(&listed.item_id)?, find_row(&listed.parent_id)?);
        outcomes[at] = decide_listed_item(&listed, row.as_ref(), parent_row.as_ref(), fetch_began_at_count);
        match outcomes[at] {
            ListedChangeOutcome::Written => write_listed_item(tx, &listed, row.as_ref(), parent_row.as_ref(), &drive_id, full_listing)?,
            ListedChangeOutcome::Outdated => outdated_items.push(OneDriveItemId::new(&listed.item_id)),
            ListedChangeOutcome::Refused(refusal) => tracing::warn!("what the listing says of the item {:?} is not written: {refusal:?}", listed.item_id),
        }
    }

    for (at, change) in counted() {
        let Change::Delete(item_id) = change else { continue };
        let row = find_row(item_id)?;
        // A removal takes what the rows say OneDrive has below the item, which a listing
        // does not always name: it is written whole, or, when a row below is left to be
        // read again, not at all.
        let left_below = match &row {
            Some(row) => list_left_below_in_onedrive(tx, row.local_item_id, fetch_began_at_count)?,
            None => Vec::new(),
        };
        outcomes[at] = decide_listed_removal(item_id, row.as_ref(), fetch_began_at_count, left(&left_below));
        match (outcomes[at], row) {
            (ListedChangeOutcome::Written, Some(row)) => {
                clear_onedrive_versions_below(tx, row.local_item_id)?;
                if row.onedrive.is_some() {
                    clear_onedrive_version(tx, row.local_item_id)?;
                }
            }
            // An item the client never knew, or forgot already, has no row to write.
            (ListedChangeOutcome::Written, None) => {}
            (ListedChangeOutcome::Outdated, _) => {
                outdated_items.push(OneDriveItemId::new(item_id));
                outdated_items.extend(left_below);
            }
            (ListedChangeOutcome::Refused(refusal), _) => tracing::warn!("that the listing says the item {item_id:?} is gone is not written: {refusal:?}"),
        }
    }

    // What ends the page: the link is written last, and only when nothing is left to read
    // again. A full listing that ends takes what it did not see first; what was confirmed
    // of that since the listing began is to be read again too.
    let mut end = decide_page_end(listing.kind, link, PageProgress::Changes, left(&outdated_items));
    if let (PageEnd::FullListingSweeps, Some(full_listing)) = (end, full_listing) {
        outdated_items.extend(clear_onedrive_versions_unseen(tx, full_listing, listing.began_at_count)?);
        end = decide_page_end(listing.kind, link, PageProgress::ChangesAndSweep, left(&outdated_items));
    }
    delete_rows_of_binding_alone(tx, listing.binding_alone_left_after_count(end))?;
    listing.write_page_end(tx, end, link)?;
    let mut answered = HashSet::new();
    outdated_items.retain(|id| answered.insert(id.clone()));

    // Each change of an item has the outcome of the last one of it.
    let changes = changes.iter().enumerate().map(|(at, change)| outcomes[last_change_of.get(change.id()).copied().unwrap_or(at)]).collect();
    Ok(ListedPageOutcome { changes, outdated_items })
}

#[cfg(test)]
mod tests;
