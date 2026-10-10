//! What a listing of OneDrive says of an item, written into its row: its OneDrive version
//! (place, and for a file content), the eTag and the placement. Nothing else of a row is
//! written here: not the local version, not what the two sides agreed on, not whether the
//! content is on disk. What then differs between the sides is the plan's to act on.
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
//!    below it any more when this runs, and what it moved into it is.
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
//! statement ([`confirm_onedrive_version`]). A cycle reads the count when a fetch begins
//! and gives it with every page of that fetch. What a page says of a row stamped later may
//! be older than what was confirmed: it is taken only when it is what was confirmed, and
//! otherwise the row is left and the item is answered, to be read again from OneDrive. A
//! row whose removal was confirmed has no version on either side; it stays until a listing
//! ends that began at its stamp or after, so that no page fetched before the removal finds
//! the binding free and makes the item again.
//!
//! **A listing, from its beginning to its end.** One listing at a time is the caller's
//! rule. A full listing (the first one, and after OneDrive asks to start over) is begun
//! with [`ItemStore::begin_full_listing`], which forgets the delta link: the store has none
//! while a full listing is under way, so no listing of changes can be asked beside it. A
//! listing of changes begins with the first page given while none is under way. The store
//! keeps, of the listing under way ([`ItemStore::find_listing_under_way`]): the count it
//! began at; the link to its next page, from which a stopped listing goes on; and that it
//! is a full one, each row whose version a page writes being marked as seen by it.
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

use std::collections::HashMap;
use std::ffi::OsString;

use rusqlite::{Connection, Transaction};

use super::bindings::{bind_root, make_onedrive_item};
use super::item_row::{clear_onedrive_version, clear_onedrive_versions_below, clear_onedrive_versions_unseen, delete_rows_of_binding_alone, find_bound_item, find_item, set_onedrive_etag, set_onedrive_version, write_item};
use super::{refuse, ItemStore};
use crate::model::{Change, ContentVersion, DriveId, Item, ItemKind, ItemPlace, ItemVersion, Kind, LocalItemId, OneDriveItemId, OneDriveItemReference, Placement, Row};
use crate::{meta, TreeError};

impl ItemStore<'_> {
    /// Writes what one page of a listing of the store's drive says into the rows, in one
    /// transaction: all of it or nothing. See the module for the order it writes in.
    ///
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
    /// that is gone), the count that reading began at, and the same link. It asks nothing
    /// from that link before a call has written it. If the reading fails or the daemon
    /// stops in between, nothing is lost: the store has the link it had before, the
    /// listing is asked from it again, and it names those items again.
    ///
    /// A page given while no listing is under way begins a listing of changes; the store
    /// has a delta link then. A full listing is begun with
    /// [`begin_full_listing`](Self::begin_full_listing) before its first page, and while
    /// it is under way every page is taken as its page.
    ///
    /// Refused, with nothing written: in a store that does not know its drive; when no
    /// listing is under way and the store has no delta link (what is listed then is a
    /// full listing, which was not begun); when `fetch_began_at_count` is a count the
    /// store has not reached; and when it is before the count the listing under way began
    /// at, as that of a page of an older listing is.
    pub fn write_listed_changes(&mut self, changes: &[Change], fetch_began_at_count: u64, link: &ListingLink) -> Result<ListedPageOutcome, TreeError> {
        self.write(|tx| write_listed_changes(tx, changes, fetch_began_at_count, link))
    }

    /// The listing the store was given the beginning of and not the end: a full one from
    /// [`begin_full_listing`](Self::begin_full_listing), a listing of changes from its
    /// first page, each until a page with a [`DeltaLink`](ListingLink::DeltaLink) leaves
    /// nothing to read again. `None` when none is under way. While one is, the rows may
    /// say OneDrive does not have an item, or has it where the folder cannot show it, that
    /// a page still to come says otherwise of.
    pub fn find_listing_under_way(&self) -> Result<Option<ListingUnderWay>, TreeError> {
        find_listing_under_way(&self.store.conn)
    }

    /// A full listing begins: from now until a page with a
    /// [`DeltaLink`](ListingLink::DeltaLink) ends it, what the pages say is all OneDrive
    /// has. The count of confirmations it begins at, which the store keeps for its end and
    /// the caller gives with its first pages. A listing that was under way is given up for
    /// this one, with the link to its next page. The delta link is forgotten in the same
    /// transaction: while a full listing is under way the store has none.
    pub fn begin_full_listing(&mut self) -> Result<u64, TreeError> {
        self.write(|tx| {
            let began_at_count = read_count(tx, meta::CONFIRMATION_COUNT)?;
            meta::set(tx, meta::FULL_LISTING_COUNT, Some(&(read_count(tx, meta::FULL_LISTING_COUNT)? + 1).to_string()))?;
            meta::set(tx, meta::FULL_LISTING_UNDER_WAY, Some("1"))?;
            meta::set(tx, meta::LISTING_BEGAN_AT_COUNT, Some(&began_at_count.to_string()))?;
            meta::set(tx, meta::LISTING_NEXT_LINK, None)?;
            meta::set(tx, meta::DELTA_LINK, None)?;
            Ok(began_at_count)
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

/// What follows a page of a listing, in Graph's own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListingLink {
    /// `@odata.nextLink`: the listing goes on, and its next page is asked from this link.
    NextLink(String),
    /// `@odata.deltaLink`: the listing ended, and the next listing of changes is asked from
    /// this link.
    DeltaLink(String),
}

/// What became of one page of a listing given to the store
/// ([`ItemStore::write_listed_changes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedPageOutcome {
    /// What became of each change, in the order they were given. Of several changes of one
    /// item the last counts, and each of them has its outcome.
    pub changes: Vec<ListedChangeOutcome>,
    /// The items to read again from OneDrive: those of the changes that are
    /// [`Outdated`](ListedChangeOutcome::Outdated), and the items no change names that a
    /// removal would have taken — below a directory listed gone, or not seen by a full
    /// listing that ends — though something was confirmed of them since.
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

/// Whether a listing says all OneDrive has, or what changed since a delta link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListingKind {
    /// All OneDrive has: begun with [`ItemStore::begin_full_listing`]. What it does not see
    /// is gone.
    Full,
    /// What changed since the delta link the store had.
    Delta,
}

/// A listing the store was given the beginning of and not the end
/// ([`ItemStore::find_listing_under_way`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingUnderWay {
    pub kind: ListingKind,
    /// The count of confirmations the listing began at.
    pub began_at_count: u64,
    /// The link its next page is asked from, as the last page that left nothing to read
    /// again gave it; `None` before the first such page.
    pub next_link: Option<String>,
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
            row.onedrive_mime = None;
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

/// [`ItemStore::find_listing_under_way`]: a listing is under way while the store keeps the
/// count it began at.
fn find_listing_under_way(conn: &Connection) -> Result<Option<ListingUnderWay>, TreeError> {
    let Some(began_at_count) = meta::get(conn, meta::LISTING_BEGAN_AT_COUNT)?.and_then(|count| count.parse().ok()) else { return Ok(None) };
    let kind = if meta::get(conn, meta::FULL_LISTING_UNDER_WAY)?.is_some() { ListingKind::Full } else { ListingKind::Delta };
    Ok(Some(ListingUnderWay { kind, began_at_count, next_link: meta::get(conn, meta::LISTING_NEXT_LINK)? }))
}

/// The listing a page whose fetch began at the count `fetch_began_at_count` is a page of:
/// the one under way, or a listing of changes that begins with it. Refused when the page
/// can be of neither, before anything is written.
fn find_or_begin_listing(tx: &Transaction<'_>, fetch_began_at_count: u64) -> Result<ListingUnderWay, TreeError> {
    if fetch_began_at_count > read_count(tx, meta::CONFIRMATION_COUNT)? {
        return Err(refuse(format!("a fetch cannot have begun at the count {fetch_began_at_count}: the store has not counted so many confirmations")));
    }
    match find_listing_under_way(tx)? {
        Some(listing) if fetch_began_at_count < listing.began_at_count => {
            Err(refuse(format!("the page was fetched at the count {fetch_began_at_count}, before the listing under way began ({}): it is another listing's", listing.began_at_count)))
        }
        Some(listing) => Ok(listing),
        None if meta::get(tx, meta::DELTA_LINK)?.is_none() => Err(refuse("no listing is under way and the store has no delta link: a full listing is begun before its first page".to_owned())),
        None => {
            meta::set(tx, meta::LISTING_BEGAN_AT_COUNT, Some(&fetch_began_at_count.to_string()))?;
            Ok(ListingUnderWay { kind: ListingKind::Delta, began_at_count: fetch_began_at_count, next_link: None })
        }
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
/// `seen_in_full_listing`: the count of the full listing under way, when one is.
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
pub(crate) fn write_listed_changes(tx: &Transaction<'_>, changes: &[Change], fetch_began_at_count: u64, link: &ListingLink) -> Result<ListedPageOutcome, TreeError> {
    let Some(drive_id) = meta::get(tx, meta::DRIVE_ID)?.and_then(DriveId::new) else {
        return Err(refuse("the store does not know its drive: a listing is not written".to_owned()));
    };
    let listing = find_or_begin_listing(tx, fetch_began_at_count)?;
    let full_listing_count = match listing.kind {
        ListingKind::Full => Some(read_count(tx, meta::FULL_LISTING_COUNT)?),
        ListingKind::Delta => None,
    };
    // The row that has an item id of OneDrive: none has the empty text for one.
    let find_row = |item_id: &str| -> Result<Option<Item>, TreeError> {
        if item_id.is_empty() {
            return Ok(None);
        }
        find_bound_item(tx, &OneDriveItemReference { drive_id: drive_id.clone(), item_id: OneDriveItemId::new(item_id) })
    };

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
            ListedChangeOutcome::Written => write_listed_item(tx, &listed, row.as_ref(), parent_row.as_ref(), &drive_id, full_listing_count)?,
            ListedChangeOutcome::Outdated => outdated_items.push(OneDriveItemId::new(&listed.item_id)),
            ListedChangeOutcome::Refused(refusal) => tracing::warn!("what the listing says of the item {:?} is not written: {refusal:?}", listed.item_id),
        }
    }

    for (at, change) in counted() {
        let Change::Delete(item_id) = change else { continue };
        let row = find_row(item_id)?;
        outcomes[at] = decide_listed_removal(item_id, row.as_ref(), fetch_began_at_count);
        match (outcomes[at], row) {
            // What the rows say OneDrive has below it is gone with it, which a listing does
            // not always name.
            (ListedChangeOutcome::Written, Some(row)) => {
                outdated_items.extend(clear_onedrive_versions_below(tx, row.local_item_id, fetch_began_at_count)?);
                if row.onedrive.is_some() {
                    clear_onedrive_version(tx, row.local_item_id)?;
                }
            }
            // An item the client never knew, or forgot already, has no row to write.
            (ListedChangeOutcome::Written, None) => {}
            (ListedChangeOutcome::Outdated, _) => outdated_items.push(OneDriveItemId::new(item_id)),
            (ListedChangeOutcome::Refused(refusal), _) => tracing::warn!("that the listing says the item {item_id:?} is gone is not written: {refusal:?}"),
        }
    }

    // A full listing whose last page left nothing to read again takes what it did not
    // see; what was confirmed of that since the listing began is to be read again too.
    if let (true, ListingLink::DeltaLink(_), Some(count)) = (outdated_items.is_empty(), link, full_listing_count) {
        outdated_items.extend(clear_onedrive_versions_unseen(tx, count, listing.began_at_count)?);
    }
    let mut answered = std::collections::HashSet::new();
    outdated_items.retain(|id| answered.insert(id.clone()));

    // The link is written last, and only when nothing is left to read again.
    match link {
        // The listing ends: the rows of the removals confirmed before it began were
        // passed by every page of it.
        ListingLink::DeltaLink(delta_link) if outdated_items.is_empty() => {
            delete_rows_of_binding_alone(tx, listing.began_at_count)?;
            meta::set(tx, meta::DELTA_LINK, Some(delta_link))?;
            for key in [meta::LISTING_BEGAN_AT_COUNT, meta::LISTING_NEXT_LINK, meta::FULL_LISTING_UNDER_WAY] {
                meta::set(tx, key, None)?;
            }
        }
        ListingLink::NextLink(next_link) if outdated_items.is_empty() => {
            delete_rows_of_binding_alone(tx, 0)?;
            meta::set(tx, meta::LISTING_NEXT_LINK, Some(next_link))?;
        }
        // The listing goes on: only a row that was never stamped goes.
        _ => delete_rows_of_binding_alone(tx, 0)?,
    }

    // Each change of an item has the outcome of the last one of it.
    let changes = changes.iter().enumerate().map(|(at, change)| outcomes[last_change_of.get(change.id()).copied().unwrap_or(at)]).collect();
    Ok(ListedPageOutcome { changes, outdated_items })
}

/// What a listing says of one item, and what becomes of it.
mod listed_change;

pub use listed_change::{ListedChangeOutcome, ListedChangeRefusal};
use listed_change::{decide_listed_item, decide_listed_removal, decide_listed_root, ListedItem};

#[cfg(test)]
mod tests;
