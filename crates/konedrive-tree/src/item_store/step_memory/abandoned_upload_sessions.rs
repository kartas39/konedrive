//! The upload sessions nothing goes on with, each kept until it is cancelled: the table
//! `abandoned_upload_sessions`. A session is abandoned when the content it was sending is
//! not the file's any more, when its item's row goes, and when the item's binding moves. Its
//! address is no fact of an item: it is what is left to do in OneDrive once the item's
//! memory of the session is gone, so that the empty file of a new file's session does not
//! hold a name until OneDrive ends the session by itself.

use rusqlite::{params, Connection};

use super::item_memory::{UploadSession, UploadSessionAddress};
use super::memory_row::folded_name_value;
use crate::model::{ItemPlace, LocalItemId};
use crate::TreeError;

/// `session` is abandoned: its address is kept, with the place its empty file holds.
pub(super) fn abandon(conn: &Connection, session: &UploadSession) -> Result<(), TreeError> {
    let placeholder = session.placeholder.as_ref();
    conn.prepare_cached("INSERT OR IGNORE INTO abandoned_upload_sessions (address, placeholder_parent_id, placeholder_folded_name) VALUES (?1, ?2, ?3)")?.execute(params![
        session.address.as_str(),
        placeholder.map(|place| place.parent_local_item_id.as_i64()),
        placeholder.map(|place| folded_name_value(&place.name)),
    ])?;
    Ok(())
}

/// Up to `limit` abandoned sessions, the one abandoned first first.
pub(super) fn list_abandoned(conn: &Connection, limit: usize) -> Result<Vec<UploadSessionAddress>, TreeError> {
    let mut statement = conn.prepare_cached("SELECT address FROM abandoned_upload_sessions ORDER BY abandoned_order LIMIT ?1")?;
    let addresses = statement.query_map([limit as i64], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    Ok(addresses.into_iter().map(UploadSessionAddress::new).collect())
}

/// The abandoned sessions whose empty file holds `place`.
pub(super) fn list_abandoned_at(conn: &Connection, place: &ItemPlace) -> Result<Vec<UploadSessionAddress>, TreeError> {
    let mut statement =
        conn.prepare_cached("SELECT address FROM abandoned_upload_sessions WHERE placeholder_parent_id = ?1 AND placeholder_folded_name = ?2 ORDER BY abandoned_order")?;
    let addresses = statement.query_map(params![place.parent_local_item_id.as_i64(), folded_name_value(&place.name)], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    Ok(addresses.into_iter().map(UploadSessionAddress::new).collect())
}

/// The session at `address` was cancelled, or found gone: it is abandoned no more.
pub(super) fn forget_abandoned(conn: &Connection, address: &UploadSessionAddress) -> Result<(), TreeError> {
    conn.prepare_cached("DELETE FROM abandoned_upload_sessions WHERE address = ?1")?.execute([address.as_str()])?;
    Ok(())
}

/// The empty files in the directory `from` are in `to`: the binding of `from` is `to`'s now.
pub(super) fn move_placeholders(conn: &Connection, from: LocalItemId, to: LocalItemId) -> Result<(), TreeError> {
    conn.prepare_cached("UPDATE abandoned_upload_sessions SET placeholder_parent_id = ?2 WHERE placeholder_parent_id = ?1")?.execute(params![from.as_i64(), to.as_i64()])?;
    Ok(())
}
