//! When one item takes another's place: the three operations that move a binding. The local
//! item id follows the file the kernel holds; the binding follows what OneDrive must see as
//! one item. Where the two part, the binding moves, here and nowhere else.
//!
//! Each keeps a binding one item's (the row that had it lets it go before another takes
//! it), and keeps a synced place pointing at a row: what the two sides agreed is in a
//! directory of OneDrive stays with the row that has that directory's binding.

use rusqlite::{params, Connection, Transaction};

use super::stored::{delete_item, find_item, insert_item, write_item};
use super::{check_empty, refuse, ItemStore};
use crate::model::{ContentPresence, Item, LocalItemId};
use crate::TreeError;

impl ItemStore<'_> {
    /// The replacing item takes the binding and the synced place and content of the
    /// replaced one, and the replaced row goes: a file saved by a rename over another, or
    /// made at the place of one removed here whose removal is not sent yet. OneDrive sees
    /// one item whose content changed.
    pub fn take_over_binding(&mut self, replacing: LocalItemId, replaced: LocalItemId) -> Result<(), TreeError> {
        self.write(|tx| take_over_binding(tx, replacing, replaced))
    }

    /// The item keeps its id, its local version and its content, and is an item OneDrive
    /// does not have; a new row takes the binding and nothing else, and is returned: what
    /// OneDrive has under that binding is that row's to be given.
    pub fn give_up_binding(&mut self, item: LocalItemId) -> Result<LocalItemId, TreeError> {
        self.write(|tx| give_up_binding(tx, item))
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

/// What the two sides agreed is in the directory `from` is in `to` from now on.
fn move_synced_children(conn: &Connection, from: LocalItemId, to: LocalItemId) -> Result<(), TreeError> {
    conn.prepare_cached("UPDATE items SET synced_parent_id = ?2 WHERE synced_parent_id = ?1")?.execute(params![from.as_i64(), to.as_i64()])?;
    Ok(())
}

/// The row `from` goes, and `to` has its binding and its synced place and content.
fn hand_over(conn: &Connection, from: Item, mut to: Item) -> Result<(), TreeError> {
    if from.kind != to.kind {
        return Err(refuse(format!("item {} and item {} are not of one kind: an item of OneDrive never changes its kind", from.local_item_id, to.local_item_id)));
    }
    check_empty(conn, from.local_item_id)?;
    move_synced_children(conn, from.local_item_id, to.local_item_id)?;
    // The row that has the binding goes before another takes it.
    delete_item(conn, from.local_item_id)?;
    to.onedrive_item = from.onedrive_item;
    to.synced_place = from.synced_place;
    to.synced_content = from.synced_content;
    write_item(conn, &to)
}

/// [`ItemStore::take_over_binding`], inside the caller's transaction.
pub(crate) fn take_over_binding(tx: &Transaction<'_>, replacing: LocalItemId, replaced: LocalItemId) -> Result<(), TreeError> {
    if replacing == replaced {
        return Err(refuse(format!("item {replacing} does not take over its own binding")));
    }
    let (taker, giver) = (read_below_root(tx, replacing)?, read_below_root(tx, replaced)?);
    if taker.onedrive_item.is_some() {
        return Err(refuse(format!("item {replacing} has a binding already and would lose it")));
    }
    if giver.onedrive_item.is_none() {
        return Err(refuse(format!("item {replaced} has no binding to take over")));
    }
    hand_over(tx, giver, taker)
}

/// [`ItemStore::give_up_binding`], inside the caller's transaction.
pub(crate) fn give_up_binding(tx: &Transaction<'_>, item: LocalItemId) -> Result<LocalItemId, TreeError> {
    let mut giver = read_below_root(tx, item)?;
    let Some(onedrive_item) = giver.onedrive_item.take() else {
        return Err(refuse(format!("item {item} has no binding to give up")));
    };
    giver.synced_place = None;
    giver.synced_content = None;
    // The row that has the binding lets it go before another takes it.
    write_item(tx, &giver)?;
    let taker = insert_item(tx, giver.kind, Some(&onedrive_item), None, ContentPresence::NotOnDisk, false)?;
    move_synced_children(tx, item, taker)?;
    Ok(taker)
}

/// [`ItemStore::merge_items`], inside the caller's transaction.
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

#[cfg(test)]
mod tests;
