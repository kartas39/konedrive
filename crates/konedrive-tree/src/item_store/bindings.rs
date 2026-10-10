//! When one item takes another's place: the three operations that move a binding. The local
//! item id follows the file the kernel holds; the binding follows what OneDrive must see as
//! one item. Where the two part, the binding moves, here and nowhere else.
//!
//! `give_up_binding` and `merge_items` are operations of [`ItemStore`]. `take_over_binding`
//! is done only with the change of a local version that says which item replaces which.
//!
//! A binding is given first here too: to an item of the folder that OneDrive confirmed as a
//! new item (`bind_item`), and with a row made for an item of OneDrive the client did not
//! know (`make_onedrive_item`).
//!
//! What is `pub(crate)` here runs inside its caller's transaction and may fail after it wrote
//! (SQLite, the disk, a rule of the schema): on any error the caller drops the transaction
//! and never commits it.
//!
//! Each keeps a binding one item's (the row that had it lets it go before another takes
//! it), and keeps a synced place and an OneDrive place pointing at a row: what the two sides
//! agreed is in a directory of OneDrive, and what OneDrive has in it, stay with the row that
//! has that directory's binding.
//!
//! What is OneDrive's goes with the binding, in every one of them: the OneDrive version, its
//! eTag, the placement, the stamp of the last confirmation and the full listing that saw
//! the item. An item that lets its binding go keeps none of it.

use rusqlite::{params, Connection, Transaction};

use super::item_row::{delete_item, find_bound_item, find_item, find_seen_in_full_listing, insert_item, set_binding, set_seen_in_full_listing, write_item};
use super::{check_empty, refuse, ItemStore};
use crate::model::{ContentPresence, DriveId, Item, ItemKind, LocalItemId, OneDriveItemId, OneDriveItemReference, Placement};
use crate::{meta, TreeError};

impl ItemStore<'_> {
    /// The item, which the folder shows, keeps its id, its local version and its content,
    /// and is an item OneDrive does not have; a new row of its kind takes the binding and
    /// nothing else, and is returned: what OneDrive has under that binding is that row's to
    /// be given. `is_name_ignored` is what the upload rules say of the item's name, written
    /// here as where an item without a binding gets a name: it is asked of the item from
    /// now on ([`is_ignored`](Self::is_ignored)).
    pub fn give_up_binding(&mut self, item: LocalItemId, is_name_ignored: bool) -> Result<LocalItemId, TreeError> {
        self.write(|tx| give_up_binding(tx, item, is_name_ignored))
    }

    /// The item `item` of the folder, which OneDrive did not have, is the item
    /// `onedrive_item` of OneDrive from now on: OneDrive confirmed it as a new item. Refused
    /// when the folder shows another item that has the reference. When a row the folder
    /// does not show has it (a daemon died between marking an uploaded file and its commit,
    /// and a listing brought the item since), that row is merged into this one, as
    /// [`merge_items`](Self::merge_items) does.
    pub fn bind_item(&mut self, item: LocalItemId, onedrive_item: &OneDriveItemReference) -> Result<(), TreeError> {
        self.write(|tx| bind_item(tx, item, onedrive_item))
    }

    /// A row for an item of OneDrive the client did not know, of this kind: the binding and
    /// nothing else, as at a parent's first mention in a listing. Its id. Refused when the
    /// reference is an item's already.
    pub fn make_onedrive_item(&mut self, onedrive_item: &OneDriveItemReference, kind: ItemKind) -> Result<LocalItemId, TreeError> {
        self.write(|tx| make_onedrive_item(tx, onedrive_item, kind))
    }

    /// A new item here (`local`) and a new item in OneDrive (`onedrive`, which the folder
    /// does not show) are one: the local row takes the binding and the synced place and
    /// content of the other, and the other row goes.
    pub fn merge_items(&mut self, local: LocalItemId, onedrive: LocalItemId) -> Result<(), TreeError> {
        self.write(|tx| merge_items(tx, local, onedrive))
    }
}

/// The row of `id`, which an operation on bindings changes: refused for an id with no row
/// and for the root, whose binding is the drive's root and stays.
fn read_below_root(conn: &Connection, id: LocalItemId) -> Result<Item, TreeError> {
    if id == LocalItemId::ROOT {
        return Err(refuse("the binding of the root of the folder is not moved".to_owned()));
    }
    find_item(conn, id)?.ok_or_else(|| refuse(format!("item {id} has no row")))
}

/// What the two sides agreed is in the directory `from`, and what OneDrive has in it, is in
/// `to` from now on.
fn move_children_of_binding(conn: &Connection, from: LocalItemId, to: LocalItemId) -> Result<(), TreeError> {
    conn.prepare_cached("UPDATE items SET synced_parent_id = ?2 WHERE synced_parent_id = ?1")?.execute(params![from.as_i64(), to.as_i64()])?;
    conn.prepare_cached("UPDATE items SET onedrive_parent_id = ?2 WHERE onedrive_parent_id = ?1")?.execute(params![from.as_i64(), to.as_i64()])?;
    Ok(())
}

/// What is OneDrive's of `from` is `to`'s: the binding, the OneDrive version, its eTag, the
/// placement and the stamp. `from` keeps none of it.
fn move_onedrive_side(from: &mut Item, to: &mut Item) {
    to.onedrive_item = from.onedrive_item.take();
    to.onedrive = from.onedrive.take();
    to.onedrive_etag = from.onedrive_etag.take();
    to.placement = std::mem::replace(&mut from.placement, Placement::Placed);
    to.confirmed_count = std::mem::take(&mut from.confirmed_count);
}

/// The row `from` goes, and `to` has its binding, what is OneDrive's of it, and its synced
/// place and content.
fn hand_over(conn: &Connection, mut from: Item, mut to: Item) -> Result<(), TreeError> {
    if from.kind != to.kind {
        return Err(refuse(format!("item {} and item {} are not of one kind: an item of OneDrive never changes its kind", from.local_item_id, to.local_item_id)));
    }
    check_empty(conn, from.local_item_id)?;
    move_children_of_binding(conn, from.local_item_id, to.local_item_id)?;
    let seen_in_full_listing = find_seen_in_full_listing(conn, from.local_item_id)?;
    // The row that has the binding goes before another takes it.
    delete_item(conn, from.local_item_id)?;
    move_onedrive_side(&mut from, &mut to);
    to.synced_place = from.synced_place;
    to.synced_content = from.synced_content;
    write_item(conn, &to)?;
    set_seen_in_full_listing(conn, to.local_item_id, seen_in_full_listing)
}

/// The file `replacing` takes the binding and the synced place and content of the file
/// `replaced`, which the folder does not show any more, and the replaced row goes: OneDrive
/// sees one item whose content changed. Inside the caller's transaction: on any error the
/// caller drops its transaction and never commits it.
///
/// It is not an operation of [`ItemStore`] by itself: which item replaces which is said by
/// a change of a local version, a file saved over another
/// ([`SavedOver`](crate::LocalVersionChange::SavedOver)) or made where one was removed
/// ([`Made`](crate::LocalVersionChange::Made)), and it is done with that change.
pub(crate) fn take_over_binding(tx: &Transaction<'_>, replacing: LocalItemId, replaced: LocalItemId) -> Result<(), TreeError> {
    if replacing == replaced {
        return Err(refuse(format!("item {replacing} does not take over its own binding")));
    }
    let (taker, giver) = (read_below_root(tx, replacing)?, read_below_root(tx, replaced)?);
    if taker.onedrive_item.is_some() || taker.local.is_none() {
        return Err(refuse(format!("item {replacing} takes no binding over: it has one already and would lose it, or the folder does not show it")));
    }
    if giver.onedrive_item.is_none() {
        return Err(refuse(format!("item {replaced} has no binding to take over")));
    }
    if giver.local.is_some() {
        return Err(refuse(format!("the folder still shows item {replaced}: its row does not go from under its file")));
    }
    if giver.kind != ItemKind::File || taker.kind != ItemKind::File {
        return Err(refuse(format!("item {replacing} and item {replaced} are not two files: only a file's binding is taken over")));
    }
    hand_over(tx, giver, taker)
}

/// [`ItemStore::give_up_binding`], inside the caller's transaction. On any error the
/// caller drops its transaction and never commits it.
pub(crate) fn give_up_binding(tx: &Transaction<'_>, item: LocalItemId, is_name_ignored: bool) -> Result<LocalItemId, TreeError> {
    let mut giver = read_below_root(tx, item)?;
    if giver.local.is_none() {
        return Err(refuse(format!("the folder does not show item {item}: with no binding its row would be of nothing")));
    }
    let Some(onedrive_item) = giver.onedrive_item.clone() else {
        return Err(refuse(format!("item {item} has no binding to give up")));
    };
    let seen_in_full_listing = find_seen_in_full_listing(tx, item)?;
    // What is OneDrive's of the giver, for the row that takes the binding.
    let mut given = giver.clone();
    move_onedrive_side(&mut giver, &mut given);
    giver.synced_place = None;
    giver.synced_content = None;
    giver.is_name_ignored = is_name_ignored;
    // The row that has the binding lets it go before another takes it.
    write_item(tx, &giver)?;
    set_seen_in_full_listing(tx, item, 0)?;
    let taker = insert_item(tx, giver.kind, Some(&onedrive_item), None, ContentPresence::NotOnDisk, false)?;
    let mut taker_row = find_item(tx, taker)?.ok_or_else(|| refuse(format!("item {taker} has no row")))?;
    move_onedrive_side(&mut given, &mut taker_row);
    write_item(tx, &taker_row)?;
    set_seen_in_full_listing(tx, taker, seen_in_full_listing)?;
    move_children_of_binding(tx, item, taker)?;
    Ok(taker)
}

/// [`ItemStore::merge_items`], inside the caller's transaction. On any error the
/// caller drops its transaction and never commits it.
pub(crate) fn merge_items(tx: &Transaction<'_>, local: LocalItemId, onedrive: LocalItemId) -> Result<(), TreeError> {
    if local == onedrive {
        return Err(refuse(format!("item {local} is not merged with itself")));
    }
    let (taker, giver) = (read_below_root(tx, local)?, read_below_root(tx, onedrive)?);
    if taker.onedrive_item.is_some() || taker.local.is_none() {
        return Err(refuse(format!("item {local} is not a new item of the folder: it has a binding, or the folder does not show it")));
    }
    if giver.onedrive_item.is_none() || giver.local.is_some() {
        return Err(refuse(format!("item {onedrive} is not a new item of OneDrive: it has no binding, or the folder shows it as an item of its own")));
    }
    hand_over(tx, giver, taker)
}

/// [`ItemStore::bind_item`], inside the caller's transaction. On any error the
/// caller drops its transaction and never commits it.
pub(crate) fn bind_item(tx: &Transaction<'_>, item: LocalItemId, onedrive_item: &OneDriveItemReference) -> Result<(), TreeError> {
    let taker = read_below_root(tx, item)?;
    if taker.onedrive_item.is_some() || taker.local.is_none() {
        return Err(refuse(format!("item {item} is not a new item of the folder: it has a binding, or the folder does not show it")));
    }
    match find_bound_item(tx, onedrive_item)? {
        None => set_binding(tx, item, onedrive_item),
        Some(holder) if holder.local.is_some() => Err(refuse(format!("the folder shows item {} as the item of OneDrive {}: item {item} is not bound to it too", holder.local_item_id, onedrive_item.item_id.as_str()))),
        Some(holder) => merge_items(tx, item, holder.local_item_id),
    }
}

/// [`ItemStore::make_onedrive_item`], inside the caller's transaction. On any error the
/// caller drops its transaction and never commits it.
pub(crate) fn make_onedrive_item(tx: &Transaction<'_>, onedrive_item: &OneDriveItemReference, kind: ItemKind) -> Result<LocalItemId, TreeError> {
    if let Some(holder) = find_bound_item(tx, onedrive_item)? {
        return Err(refuse(format!("item {} is the item of OneDrive {} already", holder.local_item_id, onedrive_item.item_id.as_str())));
    }
    insert_item(tx, kind, Some(onedrive_item), None, ContentPresence::NotOnDisk, false)
}

/// The root of the folder is bound to the root of the drive, once the store knows both the
/// drive and its root's item id: called wherever it learns either. Nothing to do before,
/// and an id that is the empty text is not known.
///
/// The reference is row 1's whatever row was made first: a row that has it already (a page
/// of a listing named children before the root, and their parent got a row at that first
/// mention) goes, and what was agreed to be in it is in the root.
///
/// Inside the transaction of the store's call that learnt the drive or its root
/// ([`set_drive_id`](crate::TreeStore::set_drive_id), a staged or committed page of a
/// listing), which begins its own: on any error that transaction is dropped, never
/// committed, and none of those calls is made with a transaction open.
pub(crate) fn bind_root(conn: &Connection) -> Result<(), TreeError> {
    let (Some(drive_id), Some(root_item_id)) = (meta::get(conn, meta::DRIVE_ID)?.and_then(DriveId::new), meta::get(conn, meta::ROOT_ITEM_ID)?.filter(|id| !id.is_empty())) else {
        return Ok(());
    };
    bind_root_to(conn, &OneDriveItemReference { drive_id, item_id: OneDriveItemId::new(root_item_id) })
}

/// The root of the folder is bound to `root`, the root of the drive, whatever row had that
/// reference: see [`bind_root`]. What OneDrive has in a row that goes is in the root, as
/// what was agreed to be in it is. Inside the caller's transaction, which is dropped on any
/// error.
pub(super) fn bind_root_to(conn: &Connection, root: &OneDriveItemReference) -> Result<(), TreeError> {
    match find_bound_item(conn, root)? {
        Some(holder) if holder.local_item_id == LocalItemId::ROOT => return Ok(()),
        Some(holder) if holder.local.is_some() || holder.kind != ItemKind::Directory => {
            return Err(refuse(format!("item {} is bound to the root of the drive, and is a file or an item the folder shows: it is not the root", holder.local_item_id)));
        }
        Some(holder) => {
            move_children_of_binding(conn, holder.local_item_id, LocalItemId::ROOT)?;
            delete_item(conn, holder.local_item_id)?;
        }
        None => {}
    }
    set_binding(conn, LocalItemId::ROOT, root)
}

#[cfg(test)]
mod tests;
