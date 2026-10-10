//! What a listing says of one item, and what becomes of it: written into the item's row,
//! left because it may be older than what OneDrive confirmed, or refused because no row can
//! say it. The decision is made here from what the listing says and from the rows it is
//! about, which the entry finds first; nothing here reads or writes the store.

use std::ffi::OsString;

use super::listing_under_way::LeftToReadAgain;
use crate::model::{ContentVersion, Item, ItemKind, LocalItemId, Placement};

/// What became of one change of a listing given to the store
/// ([`ItemStore::write_listed_changes`](crate::ItemStore::write_listed_changes)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListedChangeOutcome {
    /// The row says what the listing said: a version written, or taken away. Also for an
    /// item listed gone that the store never knew.
    Written,
    /// Something was confirmed of the item after the fetch that brought the change began,
    /// and the change does not say what was confirmed: the row is left, and the item is to
    /// be read again from OneDrive. Also when the removal of the directory the change says
    /// the item is in was confirmed after that fetch began: nothing is written of the item,
    /// and it is read again with the directory.
    Outdated,
    /// No row can say what the listing said of the item: nothing was written of it, and
    /// nothing is to be read again.
    Refused(ListedChangeRefusal),
}

/// Why a change of a listing was refused: what the listing said that no row can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListedChangeRefusal {
    /// The item has no id, or the empty text for one.
    NoItemId,
    /// An item that is not the root is given no parent, or the empty text for one.
    NoParent,
    /// The item is placed in itself.
    PlacedInItself,
    /// The item's parent is a file, by its row.
    ParentIsFile,
    /// The item is of another kind than its row: the kind of a bound item never changes.
    KindChanged,
    /// The root of the drive is listed as an item with a place.
    RootGivenPlace,
    /// The root of the drive is listed gone.
    RootListedGone,
    /// The id the listing gives as the root's is that of a file, or of an item the folder
    /// shows: such a row is not the root.
    RootIdOfAnotherItem,
}

/// What a listing says OneDrive has of one item that is not the root, in the store's own
/// types: made at the entry's door from what the listing hands over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ListedItem {
    pub(super) item_id: String,
    /// The item id of the directory it is in; the empty text when the listing gives none.
    pub(super) parent_id: String,
    pub(super) kind: ItemKind,
    pub(super) name: OsString,
    /// Of a file, and of nothing else.
    pub(super) content: Option<ContentVersion>,
    pub(super) etag: Option<String>,
    /// The MIME type of a file's content, when the listing gives one.
    pub(super) mime: Option<String>,
    pub(super) placement: Placement,
}

/// Whether what a page says `item` is — a version with the eTag `listed_etag` — may be
/// older than what OneDrive confirmed of it: the row was stamped after the page's fetch
/// began, and the page does not say what was confirmed. It says that when OneDrive was
/// confirmed to have the item and the page's eTag is the confirmed one.
pub(super) fn is_listed_version_outdated(item: &Item, fetch_began_at_count: u64, listed_etag: Option<&str>) -> bool {
    let is_what_was_confirmed = item.onedrive.is_some() && listed_etag.is_some() && item.onedrive_etag.as_deref() == listed_etag;
    item.confirmed_at_count > fetch_began_at_count && !is_what_was_confirmed
}

/// Whether the removal a page says of `item` may be older than what OneDrive confirmed of
/// it: the row was stamped after the page's fetch began, and OneDrive was confirmed to
/// have the item. What a removal means beyond the item it names — what is below a
/// directory — is asked of the rows below by the entry, and decided in
/// [`decide_listed_removal`].
pub(super) fn is_listed_removal_outdated(item: &Item, fetch_began_at_count: u64) -> bool {
    item.confirmed_at_count > fetch_began_at_count && item.onedrive.is_some()
}

/// Whether a page that says an item is in the directory of `parent` may be older than what
/// OneDrive confirmed of that directory: its row was stamped after the page's fetch began,
/// and OneDrive was confirmed not to have it — its removal was confirmed to a step, by
/// itself or with a directory above it, which stamps every row it takes the OneDrive
/// version from. A page fetched before that names what the removal took.
pub(super) fn is_listed_parent_outdated(parent: &Item, fetch_began_at_count: u64) -> bool {
    parent.confirmed_at_count > fetch_began_at_count && parent.onedrive.is_none()
}

/// What becomes of a listing's statement that OneDrive has `listed`, given the row that has
/// its item id and the row that has its parent's, each when there is one. The first that
/// holds: an item with no id, with no parent, or in itself; the root given a place; a
/// parent that is a file; a kind that is not the row's; a statement that may be older than
/// what was confirmed, of the item or of the directory it is said to be in
/// ([`is_listed_parent_outdated`]). `Written` otherwise: the rows it needs are then made
/// and written.
pub(super) fn decide_listed_item(listed: &ListedItem, row: Option<&Item>, parent_row: Option<&Item>, fetch_began_at_count: u64) -> ListedChangeOutcome {
    use ListedChangeOutcome::{Outdated, Refused, Written};
    if listed.item_id.is_empty() {
        return Refused(ListedChangeRefusal::NoItemId);
    }
    if listed.parent_id.is_empty() {
        return Refused(ListedChangeRefusal::NoParent);
    }
    if listed.parent_id == listed.item_id {
        return Refused(ListedChangeRefusal::PlacedInItself);
    }
    if row.is_some_and(|row| row.local_item_id == LocalItemId::ROOT) {
        return Refused(ListedChangeRefusal::RootGivenPlace);
    }
    if parent_row.is_some_and(|parent| parent.kind != ItemKind::Directory) {
        return Refused(ListedChangeRefusal::ParentIsFile);
    }
    if row.is_some_and(|row| row.kind != listed.kind) {
        return Refused(ListedChangeRefusal::KindChanged);
    }
    if row.is_some_and(|row| is_listed_version_outdated(row, fetch_began_at_count, listed.etag.as_deref())) {
        return Outdated;
    }
    if parent_row.is_some_and(|parent| is_listed_parent_outdated(parent, fetch_began_at_count)) {
        return Outdated;
    }
    Written
}

/// What becomes of a listing's statement that `root_item_id` is the root of the drive,
/// given the row that has that id when there is one: refused for no id, and for the id of
/// a file or of an item the folder shows. `Written` otherwise: row 1 is bound to it, in
/// place of a row that had the id as a parent named before the root came.
pub(super) fn decide_listed_root(root_item_id: &str, holder: Option<&Item>) -> ListedChangeOutcome {
    if root_item_id.is_empty() {
        return ListedChangeOutcome::Refused(ListedChangeRefusal::NoItemId);
    }
    match holder {
        Some(holder) if holder.local_item_id == LocalItemId::ROOT => ListedChangeOutcome::Written,
        Some(holder) if holder.local.is_some() || holder.kind != ItemKind::Directory => ListedChangeOutcome::Refused(ListedChangeRefusal::RootIdOfAnotherItem),
        _ => ListedChangeOutcome::Written,
    }
}

/// What becomes of a listing's statement that the item `item_id` is gone, given the row
/// that has the id when there is one, and whether the walk down from that row, by what the
/// rows say OneDrive has below it, met a row left to be read again (`left_below`). The
/// first that holds: refused for no id and for the root; outdated when the row may hold
/// something newer; outdated when a row below is left, as a removal is written whole or
/// not at all; `Written` otherwise, also when there is no row.
pub(super) fn decide_listed_removal(item_id: &str, row: Option<&Item>, fetch_began_at_count: u64, left_below: LeftToReadAgain) -> ListedChangeOutcome {
    if item_id.is_empty() {
        return ListedChangeOutcome::Refused(ListedChangeRefusal::NoItemId);
    }
    match (row, left_below) {
        (Some(row), _) if row.local_item_id == LocalItemId::ROOT => ListedChangeOutcome::Refused(ListedChangeRefusal::RootListedGone),
        (Some(row), _) if is_listed_removal_outdated(row, fetch_began_at_count) => ListedChangeOutcome::Outdated,
        (Some(_), LeftToReadAgain::Something) => ListedChangeOutcome::Outdated,
        _ => ListedChangeOutcome::Written,
    }
}

#[cfg(test)]
mod tests;
