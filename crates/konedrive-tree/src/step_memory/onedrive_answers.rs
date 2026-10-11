//! What becomes of the facts of what OneDrive has when OneDrive answers again about an
//! item: a listing that names it, changed or removed, and the answer to a request that
//! says where it is. Each fact names what voids it ([`item_memory`](super::item_memory));
//! here are the voiders that are not the fact's own step: called by the writer of a
//! listing's page and by the commits of the steps, in their transaction, or held by the
//! schema whoever writes the row.
//!
//! **State owned:** none of its own: the rows of `step_memory` and of
//! `mismatched_item_removals`, through their modules.

use rusqlite::Connection;

use super::memory_row::{self, read_memory};
use super::mismatched_item_removals::{find_removal, list_files_of};
use super::{change_kept_memory, forget_mismatched_item};
use crate::model::{ItemPlace, LocalItemId, OneDriveItemReference};
use crate::TreeError;

/// The two triggers that hold "a name found taken is void once OneDrive answered again
/// about that name", whoever writes the row — a listing's page, a full listing's sweep, a
/// removal that takes what is below a directory, the commit of a step OneDrive confirmed a
/// move or a removal to — and in that writer's transaction:
///
/// - `taken_name_holder_left`: the place in OneDrive of a row that keeps its binding is no
///   more the one it was. Every taken name at the place it left, the holder's name
///   compared as OneDrive compares names, is forgotten: the item that held the name was
///   moved, renamed or removed.
/// - `taken_name_other_item_came`: a row is given a place in OneDrive that another row has
///   already. OneDrive said that another item has the name now; every taken name there is
///   forgotten, and the plan decides from the rows.
///
/// A row that is given the place first — the holder itself, listed after the answer —
/// forgets nothing: the rows then say what the taken name says. A binding that moves to
/// another row takes OneDrive's place with it, and that is no answer of OneDrive: a row
/// that gives its binding up loses the place with the binding, which the first trigger
/// does not take for a holder that left; a row that hands its binding over goes; and the
/// row that takes the binding finds no other row at the place.
pub(super) fn taken_name_triggers() -> String {
    let is_another_place = "(NEW.onedrive_parent_id IS NOT OLD.onedrive_parent_id OR NEW.onedrive_folded_name IS NOT OLD.onedrive_folded_name)";
    let cleared = memory_row::TAKEN_NAME_CLEARED;
    format!(
        "
         CREATE TRIGGER taken_name_holder_left AFTER UPDATE OF onedrive_parent_id, onedrive_folded_name ON items
             WHEN OLD.onedrive_parent_id IS NOT NULL AND NEW.onedrive_item_id IS NOT NULL AND {is_another_place} BEGIN
             UPDATE step_memory SET {cleared} WHERE taken_parent_id = OLD.onedrive_parent_id AND taken_holder_folded_name = OLD.onedrive_folded_name;
         END;
         CREATE TRIGGER taken_name_other_item_came AFTER UPDATE OF onedrive_parent_id, onedrive_folded_name ON items
             WHEN NEW.onedrive_parent_id IS NOT NULL AND {is_another_place} AND EXISTS (SELECT 1 FROM items
                  WHERE onedrive_parent_id = NEW.onedrive_parent_id AND onedrive_folded_name = NEW.onedrive_folded_name AND local_item_id != NEW.local_item_id) BEGIN
             UPDATE step_memory SET {cleared} WHERE taken_parent_id = NEW.onedrive_parent_id AND taken_holder_folded_name = NEW.onedrive_folded_name;
         END;"
    )
}

/// A listing ended: in its transaction. A taken name is forgotten when no row says that
/// OneDrive has an item under the holder's name, as OneDrive compares names, in that
/// directory: the listing is OneDrive's answer about the directory, and the item that held
/// the name was never listed there, or left before any listing named it. While a listing
/// is under way nothing is asked: its later pages may still name the holder.
pub(crate) fn forget_taken_names_onedrive_lacks(conn: &Connection) -> Result<(), TreeError> {
    let sql = format!(
        "UPDATE step_memory SET {} WHERE taken_parent_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM items
          WHERE onedrive_parent_id = step_memory.taken_parent_id AND onedrive_folded_name = step_memory.taken_holder_folded_name AND items.local_item_id != step_memory.local_item_id)",
        memory_row::TAKEN_NAME_CLEARED
    );
    conn.prepare_cached(&sql)?.execute([])?;
    Ok(())
}

/// A listing wrote that OneDrive has the item `onedrive_item` with these tags: in the
/// listing's transaction. When an upload of this folder left that item and the tag of the
/// upload's answer is another one now, another client wrote the item since: it is not what
/// the upload left any more, and is forgotten wherever it is kept.
pub(crate) fn forget_mismatched_item_listed_changed(conn: &Connection, onedrive_item: &OneDriveItemReference, ctag: Option<&str>, etag: Option<&str>) -> Result<(), TreeError> {
    let kept = match list_files_of(conn, onedrive_item)?.into_iter().next() {
        Some(file) => read_memory(conn, file)?.mismatched_item,
        None => find_removal(conn, onedrive_item)?,
    };
    if kept.is_some_and(|kept| kept.is_listed_changed(ctag, etag)) {
        forget_mismatched_item(conn, onedrive_item)?;
    }
    Ok(())
}

/// OneDrive said that it has `item` at `place` — a listing, where the row said another
/// place before, or the answer to the request that made the item: in the transaction that
/// writes it. A temporary name that is not that place's is forgotten.
pub(crate) fn forget_temporary_name_unless_at(conn: &Connection, item: LocalItemId, place: &ItemPlace) -> Result<(), TreeError> {
    change_kept_memory(conn, item, |memory| memory.forget_temporary_name_unless_at(place))
}

/// The temporary name of every item the rows say OneDrive does not have is forgotten: after
/// a listing, or a step, wrote that OneDrive removed items, however many, in its
/// transaction. One statement, by the index of the temporary names.
pub(crate) fn forget_temporary_names_of_items_onedrive_lacks(conn: &Connection) -> Result<(), TreeError> {
    conn.prepare_cached(
        "UPDATE step_memory INDEXED BY step_memory_temporary_name SET temporary_name = NULL, temporary_toward_parent_id = NULL, temporary_toward_name = NULL
          WHERE temporary_name IS NOT NULL
            AND EXISTS (SELECT 1 FROM items WHERE items.local_item_id = step_memory.local_item_id AND onedrive_item_id IS NOT NULL AND onedrive_parent_id IS NULL AND items.local_item_id != 1)",
    )?
    .execute([])?;
    Ok(())
}
