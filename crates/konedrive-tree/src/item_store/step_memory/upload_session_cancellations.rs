//! The cancels of upload sessions that are left to send: the table
//! `upload_session_cancellations`, one row for each session nothing goes on with, kept until
//! the session is cancelled or found gone. A session is abandoned when the bytes it was
//! sending are not the file's any more, when its item's row goes, when the item's binding
//! moves, and when its address comes for an item that cannot have it. Its address is no
//! fact of an item: it is what is left to do in OneDrive once the item's memory of the
//! session is gone, so that the empty file of a new file's session does not hold a name
//! until OneDrive ends the session by itself.

use rusqlite::{params, Connection};

use super::item_memory::{UploadSession, UploadSessionAddress};
use super::memory_row::folded_name_value;
use crate::model::{ItemPlace, LocalItemId};
use crate::TreeError;

/// The table: an address once, in the order the sessions were abandoned, each with the
/// place its empty file holds in OneDrive when it has one, found by that place.
pub(super) fn schema() -> &'static str {
    "CREATE TABLE upload_session_cancellations (
         cancellation_order INTEGER PRIMARY KEY AUTOINCREMENT, address TEXT NOT NULL UNIQUE,
         empty_file_parent_id INTEGER, empty_file_folded_name,
         CHECK ((empty_file_parent_id IS NULL) = (empty_file_folded_name IS NULL)));
     CREATE INDEX upload_session_cancellations_empty_file ON upload_session_cancellations(empty_file_parent_id, empty_file_folded_name)
         WHERE empty_file_parent_id IS NOT NULL;"
}

/// What the trigger `step_memory_item_gone` does to this table when the row `OLD` of `items`
/// goes, while the row of `step_memory` is still there: the address of the item's upload
/// session is kept to be cancelled, and no empty file is in a directory that went.
pub(super) const ITEM_GONE_STATEMENTS: &str = "
    INSERT OR IGNORE INTO upload_session_cancellations (address, empty_file_parent_id, empty_file_folded_name)
        SELECT session_address, session_empty_file_parent_id, session_empty_file_folded_name FROM step_memory
         WHERE local_item_id = OLD.local_item_id AND session_address IS NOT NULL;
    UPDATE upload_session_cancellations SET empty_file_parent_id = NULL, empty_file_folded_name = NULL
     WHERE empty_file_parent_id = OLD.local_item_id;";

/// The session at `address` is abandoned: its cancel is left to send, with the place its
/// empty file holds. One that is kept already stays as it is.
pub(super) fn abandon_address(conn: &Connection, address: &UploadSessionAddress, empty_file_place: Option<&ItemPlace>) -> Result<(), TreeError> {
    conn.prepare_cached("INSERT OR IGNORE INTO upload_session_cancellations (address, empty_file_parent_id, empty_file_folded_name) VALUES (?1, ?2, ?3)")?.execute(params![
        address.as_str(),
        empty_file_place.map(|place| place.parent_local_item_id.as_i64()),
        empty_file_place.map(|place| folded_name_value(&place.name)),
    ])?;
    Ok(())
}

/// `session` is abandoned ([`abandon_address`]).
pub(super) fn abandon(conn: &Connection, session: &UploadSession) -> Result<(), TreeError> {
    abandon_address(conn, &session.address, session.empty_file_place.as_ref())
}

/// Up to `limit` sessions whose cancel is left to send, the one abandoned first first.
pub(crate) fn list_cancellations(conn: &Connection, limit: usize) -> Result<Vec<UploadSessionAddress>, TreeError> {
    let mut statement = conn.prepare_cached("SELECT address FROM upload_session_cancellations ORDER BY cancellation_order LIMIT ?1")?;
    let addresses = statement.query_map([limit as i64], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    Ok(addresses.into_iter().map(UploadSessionAddress::new).collect())
}

/// The abandoned sessions whose empty file holds `place`.
pub(super) fn list_cancellations_at(conn: &Connection, place: &ItemPlace) -> Result<Vec<UploadSessionAddress>, TreeError> {
    let mut statement =
        conn.prepare_cached("SELECT address FROM upload_session_cancellations WHERE empty_file_parent_id = ?1 AND empty_file_folded_name = ?2 ORDER BY cancellation_order")?;
    let addresses = statement.query_map(params![place.parent_local_item_id.as_i64(), folded_name_value(&place.name)], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    Ok(addresses.into_iter().map(UploadSessionAddress::new).collect())
}

/// The session at `address` was cancelled, found gone, or ended: no cancel is left to send.
pub(super) fn forget_cancellation(conn: &Connection, address: &UploadSessionAddress) -> Result<(), TreeError> {
    conn.prepare_cached("DELETE FROM upload_session_cancellations WHERE address = ?1")?.execute([address.as_str()])?;
    Ok(())
}

/// The empty files in the directory `from` are in `to`: the binding of `from` is `to`'s now.
pub(super) fn move_empty_files(conn: &Connection, from: LocalItemId, to: LocalItemId) -> Result<(), TreeError> {
    conn.prepare_cached("UPDATE upload_session_cancellations SET empty_file_parent_id = ?2 WHERE empty_file_parent_id = ?1")?.execute(params![from.as_i64(), to.as_i64()])?;
    Ok(())
}
