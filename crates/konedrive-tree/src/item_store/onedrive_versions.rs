//! What a listing of OneDrive says of an item, written into its row: its OneDrive version
//! (place, and for a file content), the eTag and the placement. Nothing else of a row is
//! written here: not the local version, not what the two sides agreed on, not whether the
//! content is on disk. What then differs between the sides is the plan's to act on.
//!
//! **One entry**, [`ItemStore::write_listed_changes`]: the changes of one page of a
//! listing, or of one whole answer, in one transaction. The changes come in any order, and
//! are written in an order of the entry's own, in which no statement is refused by the
//! schema whatever the page holds:
//!
//! 1. Of several changes of one item, the last counts.
//! 2. The root. Row 1 is bound to it; a row that had its reference first, as a parent named
//!    before the root came, goes, and what is in it is in row 1.
//! 3. Every item the page says OneDrive has, each by itself: the row of its parent is found
//!    by the parent's reference, and made with the binding alone when there is none (the
//!    first mention of a parent); then the item's own row, found or made the same way; then
//!    one statement that writes the OneDrive columns of that row. The parent has a row and
//!    is a directory when that statement runs, which is all the schema asks of it. No index
//!    asks for one item per name in OneDrive, so two rows may say one place between two
//!    statements and between two pages, and a directory may be below what is below it.
//! 4. Every item the page says is gone, after all of the above: its row has no OneDrive
//!    version from then on, and neither has what the rows say OneDrive has below it, which
//!    a listing does not always name. What the same page moved out of a directory is not
//!    below it any more when this runs, and what it moved into it is.
//! 5. A row left with its binding and nothing else, that no row names as its parent, goes.
//!    Such a row names no parent itself, so no row that goes holds another.
//!
//! **What is refused for one item**, with the rest of the page written: a kind that is not
//! the row's (the kind of a bound item never changes); a parent whose row is a file; an
//! item that is its own parent, has no parent or no id; the root given a place or taken
//! away.
//!
//! **The guard against an outdated listing.** The store counts confirmations
//! ([`ItemStore::confirmation_count`]); a step that OneDrive confirmed something to writes
//! what was confirmed, with its eTag, and the row is stamped with the next count, in one
//! statement ([`confirm_onedrive_version`]). A cycle reads the count when its fetch begins and gives it with every page.
//! What a page says of a row stamped later may be older than what was confirmed: it is
//! taken only when it is what was confirmed ([`is_listed_version_outdated`],
//! [`is_listed_removal_outdated`]), and otherwise the row is left and the item's id is
//! answered, for the caller to read the item again and give what it reads, with the count
//! that reading began at. A row whose removal was confirmed has no version on either side;
//! it stays until a listing ends whose fetch began at its stamp or after, so that no page
//! fetched before the removal finds the binding free and makes the item again.
//!
//! **A full listing** (the first one, and after OneDrive asks to start over) is begun with
//! [`ItemStore::begin_full_listing`]: until it ends, every row a page writes is marked with
//! the listing's count. It ends, as every listing does, with the page that carries the link
//! to ask from next time; what OneDrive had by the rows and the listing did not see then
//! loses its OneDrive version, as if listed gone. That a full listing is under way is kept
//! in the store, so one that was stopped goes on after a start.

use std::collections::HashMap;
use std::ffi::OsString;

use rusqlite::{Connection, Transaction};

use super::bindings::{bind_root, make_onedrive_item};
use super::item_row::{clear_onedrive_version, clear_onedrive_versions_below, clear_onedrive_versions_unseen, delete_rows_of_binding_alone, find_bound_item, find_item, set_onedrive_etag, set_onedrive_version, write_item};
use super::{refuse, ItemStore};
use crate::model::{Change, ContentVersion, DriveId, Item, ItemKind, Kind, ItemPlace, ItemVersion, LocalItemId, OneDriveItemId, OneDriveItemReference, Placement};
use crate::{meta, TreeError};

impl ItemStore<'_> {
    /// Writes what one page of a listing of the store's drive says, or one whole answer,
    /// into the rows, in one transaction: all of it or nothing. See the module for the
    /// order it writes in and for what it refuses for one item.
    ///
    /// `fetch_began_at_count` is the count of confirmations ([`confirmation_count`](Self::confirmation_count))
    /// the caller read when the fetch that brought the page began. The answer is the items
    /// the page was not taken for, because something was confirmed of them since: the
    /// caller reads each again from OneDrive and gives what it reads to this entry, with
    /// the count that reading began at.
    ///
    /// `delta_link` is given with the last page of a listing: the link the next listing is
    /// asked from, written with the page. The listing ends there: a full listing takes what
    /// it did not see, and the rows of confirmed removals that it has passed go.
    ///
    /// Refused, with nothing written, in a store that does not know its drive.
    pub fn write_listed_changes(&mut self, changes: &[Change], fetch_began_at_count: u64, delta_link: Option<&str>) -> Result<Vec<OneDriveItemId>, TreeError> {
        self.write(|tx| write_listed_changes(tx, changes, fetch_began_at_count, delta_link))
    }

    /// A full listing begins: from now until a page carries the link that ends it, what the
    /// pages say is all OneDrive has. One that was under way is given up for this one. The
    /// delta link is forgotten with it: while a full listing is under way the store has
    /// none.
    pub fn begin_full_listing(&mut self) -> Result<(), TreeError> {
        self.write(|tx| {
            let count = read_count(tx, meta::FULL_LISTING_COUNT)? + 1;
            meta::set(tx, meta::FULL_LISTING_COUNT, Some(&count.to_string()))?;
            meta::set(tx, meta::DELTA_LINK, None)?;
            meta::set(tx, meta::FULL_LISTING_UNDER_WAY, Some("1"))
        })
    }

    /// How many times OneDrive confirmed something to a step of the client so far: what a
    /// cycle reads when its fetch begins, and gives with every page of it.
    pub fn confirmation_count(&self) -> Result<u64, TreeError> {
        read_count(&self.store.conn, meta::CONFIRMATION_COUNT)
    }

    /// [`confirm_onedrive_version`] in a transaction of its own.
    pub fn confirm_onedrive_version(&mut self, item: LocalItemId, confirmed: &ConfirmedOneDriveVersion) -> Result<u64, TreeError> {
        self.write(|tx| confirm_onedrive_version(tx, item, confirmed))
    }
}

/// What OneDrive confirmed of an item to a step of the client, as Graph answered the step's
/// request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmedOneDriveVersion {
    /// OneDrive has the item, as this version, under this eTag: the answer to an upload, a
    /// move or a rename.
    Present { version: ItemVersion, etag: String },
    /// OneDrive does not have the item any more: the answer to a removal.
    Removed,
}

/// A count the store keeps one of; 0 before it was first raised.
fn read_count(conn: &Connection, key: &str) -> Result<u64, TreeError> {
    Ok(meta::get(conn, key)?.and_then(|count| count.parse().ok()).unwrap_or(0))
}

/// OneDrive confirmed something of `item` to a step of the client: the row has what was
/// confirmed as its OneDrive version, with the eTag of that answer, and is stamped with the
/// count of confirmations, which is raised. The count. One statement writes the three, so
/// that no row is stamped that does not hold what was confirmed: a page fetched before the
/// confirmation is then told from one that says it ([`is_listed_version_outdated`]).
///
/// What the two sides agree on from then is the step's to write, in the same transaction,
/// in the part that was confirmed (the place of a move, the content of an upload). A
/// confirmed removal is whole: the row is left with its binding alone, stamped, and stays
/// until a listing that began after it has ended, so that a page fetched before the removal
/// finds the binding taken and does not make the item again.
///
/// Inside the caller's transaction; on any error the caller drops it. Refused, with nothing
/// written and no count raised, for what OneDrive cannot have confirmed: of an id with no
/// row or an item with no binding; a removal of the root or of an item the folder shows; a
/// version that is not one of the item's kind, has a content OneDrive did not give, or a
/// place that is not in the row of a directory.
pub(crate) fn confirm_onedrive_version(tx: &Transaction<'_>, item: LocalItemId, confirmed: &ConfirmedOneDriveVersion) -> Result<u64, TreeError> {
    let mut row = find_item(tx, item)?.ok_or_else(|| refuse(format!("item {item} has no row: nothing is confirmed of it")))?;
    if row.onedrive_item.is_none() {
        return Err(refuse(format!("item {item} has no binding: OneDrive confirms nothing of it")));
    }
    match confirmed {
        ConfirmedOneDriveVersion::Present { version, etag } => {
            check_confirmed_version(tx, &row, version)?;
            row.onedrive = Some(version.clone());
            row.onedrive_etag = Some(etag.clone());
        }
        ConfirmedOneDriveVersion::Removed => {
            if item == LocalItemId::ROOT || row.local.is_some() {
                return Err(refuse(format!("item {item} is the root or an item the folder shows: its removal is not confirmed")));
            }
            row.onedrive = None;
            row.onedrive_etag = None;
            row.placement = Placement::Placed;
            row.synced_place = None;
            row.synced_content = None;
        }
    }
    let count = read_count(tx, meta::CONFIRMATION_COUNT)? + 1;
    row.confirmed_at_count = count;
    write_item(tx, &row)?;
    meta::set(tx, meta::CONFIRMATION_COUNT, Some(&count.to_string()))?;
    Ok(count)
}

/// `version` can be what OneDrive has of `row`: the root has no place and every other item
/// has one, in the row of a directory that is not its own; a file has a content, told as
/// OneDrive tells one, and a directory has none.
fn check_confirmed_version(conn: &Connection, row: &Item, version: &ItemVersion) -> Result<(), TreeError> {
    let item = row.local_item_id;
    match &version.place {
        None if item == LocalItemId::ROOT => {}
        Some(place) if item != LocalItemId::ROOT && place.parent_local_item_id != item => {
            let is_in_directory = find_item(conn, place.parent_local_item_id)?.is_some_and(|parent| parent.kind == ItemKind::Directory);
            if !is_in_directory {
                return Err(refuse(format!("item {} is no directory with a row: item {item} is not confirmed to be in it", place.parent_local_item_id)));
            }
        }
        _ => return Err(refuse(format!("the root has no place and every other item has one outside itself: not so for item {item}"))),
    }
    match (&version.content, row.kind) {
        (None, ItemKind::Directory) | (Some(ContentVersion::Hashed { .. }), ItemKind::File) => Ok(()),
        _ => Err(refuse(format!("a file has a content as OneDrive tells one and a directory has none: not so for item {item}"))),
    }
}

/// Whether what a page says `item` is — a version with the eTag `listed_etag` — may be
/// older than what OneDrive confirmed of it: the row was stamped after the page's fetch
/// began, and the page does not say what was confirmed. It says that when OneDrive was
/// confirmed to have the item and the page's eTag is the confirmed one.
fn is_listed_version_outdated(item: &Item, fetch_began_at_count: u64, listed_etag: Option<&str>) -> bool {
    let is_what_was_confirmed = item.onedrive.is_some() && listed_etag.is_some() && item.onedrive_etag.as_deref() == listed_etag;
    item.confirmed_at_count > fetch_began_at_count && !is_what_was_confirmed
}

/// Whether the removal a page says of `item`, or means by the removal of a directory above
/// it or by not listing it in a full listing, may be older than what OneDrive confirmed of
/// it: the row was stamped after the page's fetch began, and OneDrive was confirmed to
/// have the item.
fn is_listed_removal_outdated(item: &Item, fetch_began_at_count: u64) -> bool {
    item.confirmed_at_count > fetch_began_at_count && item.onedrive.is_some()
}

/// The count of the full listing under way; `None` when none is.
fn find_full_listing_under_way(conn: &Connection) -> Result<Option<u64>, TreeError> {
    match meta::get(conn, meta::FULL_LISTING_UNDER_WAY)? {
        Some(_) => Ok(Some(read_count(conn, meta::FULL_LISTING_COUNT)?)),
        None => Ok(None),
    }
}

/// The row of the item of OneDrive `onedrive_item`, which a listing says is of `kind`: the
/// one that has the reference, or a new one with the binding and the kind alone, as at a
/// parent's first mention. `None` when the row is of the other kind: the kind of a bound
/// item never changes.
fn find_or_make_listed_item(tx: &Transaction<'_>, onedrive_item: &OneDriveItemReference, kind: ItemKind) -> Result<Option<Item>, TreeError> {
    let item = match find_bound_item(tx, onedrive_item)? {
        Some(item) => item,
        None => {
            let made = make_onedrive_item(tx, onedrive_item, kind)?;
            find_item(tx, made)?.ok_or_else(|| refuse(format!("item {made} has no row")))?
        }
    };
    Ok((item.kind == kind).then_some(item))
}

/// The listing says OneDrive does not have `item`: neither it nor what the rows say
/// OneDrive has below it has an OneDrive version from now on. What was confirmed of one of
/// them after the fetch began is left, and its id is added to `read_again`.
fn clear_listed_gone(tx: &Transaction<'_>, item: &Item, fetch_began_at_count: u64, read_again: &mut Vec<OneDriveItemId>) -> Result<(), TreeError> {
    if is_listed_removal_outdated(item, fetch_began_at_count) {
        read_again.extend(item.onedrive_item.as_ref().map(|reference| reference.item_id.clone()));
        return Ok(());
    }
    read_again.extend(clear_onedrive_versions_below(tx, item.local_item_id, fetch_began_at_count)?);
    if item.onedrive.is_some() {
        clear_onedrive_version(tx, item.local_item_id)?;
    }
    Ok(())
}

/// A listing's folder is a directory, and its file a file.
fn item_kind_of(kind: Kind) -> ItemKind {
    match kind {
        Kind::File => ItemKind::File,
        Kind::Folder => ItemKind::Directory,
    }
}

/// [`ItemStore::write_listed_changes`], inside the caller's transaction, which is dropped
/// on any error. The only place of the module that holds what a listing hands over
/// ([`Change`] and its row): each is taken apart here into the item's reference, its kind,
/// its version, eTag and placement.
pub(crate) fn write_listed_changes(tx: &Transaction<'_>, changes: &[Change], fetch_began_at_count: u64, delta_link: Option<&str>) -> Result<Vec<OneDriveItemId>, TreeError> {
    let Some(drive_id) = meta::get(tx, meta::DRIVE_ID)?.and_then(DriveId::new) else {
        return Err(refuse("the store does not know its drive: a listing is not written".to_owned()));
    };
    let reference = |item_id: &str| OneDriveItemReference { drive_id: drive_id.clone(), item_id: OneDriveItemId::new(item_id) };
    let full_listing = find_full_listing_under_way(tx)?;
    let mut read_again = Vec::new();

    // Of several changes of one item, the last counts.
    let mut last_change_of = HashMap::new();
    for (at, change) in changes.iter().enumerate() {
        last_change_of.insert(change.id(), at);
    }
    let counted = || changes.iter().enumerate().filter(|(at, change)| last_change_of.get(change.id()) == Some(at)).map(|(_, change)| change);

    for change in counted() {
        let Change::Root(row) = change else { continue };
        bind_root(tx, &reference(&row.id))?;
        set_onedrive_etag(tx, LocalItemId::ROOT, row.etag.as_deref())?;
    }

    for change in counted() {
        let Change::Upsert(row) = change else { continue };
        let Some(parent_id) = row.parent_id.as_deref().filter(|parent_id| !parent_id.is_empty() && !row.id.is_empty()) else {
            tracing::warn!("the listing gives the item {:?} no parent or no id: it is not written", row.id);
            continue;
        };
        let onedrive_item = reference(&row.id);
        let kind = item_kind_of(row.kind);
        // A parent has its row before its child, whichever the page names first.
        let Some(directory) = find_or_make_listed_item(tx, &reference(parent_id), ItemKind::Directory)? else {
            tracing::warn!("the listing puts the item {:?} into {parent_id:?}, which is a file: it is not written", row.id);
            continue;
        };
        let Some(item) = find_or_make_listed_item(tx, &onedrive_item, kind)? else {
            tracing::warn!("the listing says the item {:?} is a {kind:?}, and its row is of the other kind: it is not written", row.id);
            continue;
        };
        if item.local_item_id == directory.local_item_id || item.local_item_id == LocalItemId::ROOT {
            tracing::warn!("the listing puts the item {:?} into itself, or gives the root a place: it is not written", row.id);
            continue;
        }
        if is_listed_version_outdated(&item, fetch_began_at_count, row.etag.as_deref()) {
            read_again.push(onedrive_item.item_id);
            continue;
        }
        // A time OneDrive gave is whole seconds; a file it gives no hash for is told by its
        // cTag alone.
        let content = (kind == ItemKind::File).then(|| ContentVersion::Hashed { size: row.size, mtime: row.mtime, mtime_nsec: 0, hash: row.quickxor.clone(), ctag: row.ctag.clone() });
        let onedrive = ItemVersion { place: Some(ItemPlace { parent_local_item_id: directory.local_item_id, name: OsString::from(&row.name) }), content };
        set_onedrive_version(tx, item.local_item_id, &onedrive, row.etag.as_deref(), row.placement, full_listing)?;
    }

    for change in counted() {
        let Change::Delete(id) = change else { continue };
        // An item the client never knew, or forgot already, has no row to write.
        let Some(item) = find_bound_item(tx, &reference(id))? else { continue };
        if item.local_item_id == LocalItemId::ROOT {
            tracing::warn!("the listing says the root of the drive is gone: it is not written");
            continue;
        }
        clear_listed_gone(tx, &item, fetch_began_at_count, &mut read_again)?;
    }

    match delta_link {
        // The listing ends: a full one takes what it did not see, and the rows of the
        // removals confirmed before its fetch began have been passed.
        Some(delta_link) => {
            if let Some(count) = full_listing {
                read_again.extend(clear_onedrive_versions_unseen(tx, count, fetch_began_at_count)?);
                meta::set(tx, meta::FULL_LISTING_UNDER_WAY, None)?;
            }
            delete_rows_of_binding_alone(tx, fetch_began_at_count)?;
            meta::set(tx, meta::DELTA_LINK, Some(delta_link))?;
        }
        // The listing goes on: only a row that was never stamped goes.
        None => delete_rows_of_binding_alone(tx, 0)?,
    }

    let mut answered = std::collections::HashSet::new();
    read_again.retain(|id| answered.insert(id.clone()));
    Ok(read_again)
}

#[cfg(test)]
mod tests;
