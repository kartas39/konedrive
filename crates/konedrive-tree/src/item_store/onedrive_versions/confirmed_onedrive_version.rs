//! What OneDrive confirmed of an item to a step of the client, written into its row, and
//! the count of confirmations, which the guard against an outdated listing compares a
//! row's stamp with. The count is raised here and nowhere else.

use rusqlite::{Connection, Transaction};

use crate::item_store::item_row::{find_item, write_item};
use crate::item_store::{refuse, ItemStore};
use crate::model::{ContentVersion, Item, ItemKind, ItemVersion, LocalItemId, Placement};
use crate::{meta, TreeError};

impl ItemStore<'_> {
    /// How many times OneDrive confirmed something to a step of the client so far: what a
    /// cycle reads when a fetch begins, and gives with every page of it.
    pub fn confirmation_count(&self) -> Result<u64, TreeError> {
        read_confirmation_count(&self.store.conn)
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

/// The count of confirmations; 0 before the first.
pub(in crate::item_store) fn read_confirmation_count(conn: &Connection) -> Result<u64, TreeError> {
    Ok(meta::get(conn, meta::CONFIRMATION_COUNT)?.and_then(|count| count.parse().ok()).unwrap_or(0))
}

/// OneDrive confirmed something of `item` to a step of the client: the row has what was
/// confirmed as its OneDrive version, with the eTag of that answer, and is stamped with the
/// count of confirmations, which is raised. The count. One statement writes the three, so
/// that no row is stamped that does not hold what was confirmed: a page fetched before the
/// confirmation is then told from one that says it.
///
/// What the two sides agree on from then is the step's to write, in the same transaction,
/// in the part that was confirmed (the place of a move, the content of an upload). A
/// confirmed removal is whole: the row is left with its binding alone, stamped, and stays
/// until a listing that began after it has ended, so that a page fetched before the removal
/// finds the binding taken and does not make the item again.
///
/// The MIME type OneDrive gives for the content is not in the answer written here, and the
/// one the row had is taken away: OneDrive tells it from the name and the content, either
/// of which the step may have changed. The next listing names the item and writes it.
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
    row.onedrive_mime = None;
    let count = read_confirmation_count(tx)? + 1;
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
