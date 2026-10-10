//! The upload sessions the folder was about to open for new files, by place and time, until
//! their address is known: the table `upload_session_openings`. A request that opens a
//! session makes an empty file at the place in OneDrive at once; when the daemon stops
//! before the address is kept, the record of the opening is all that says that an empty
//! file found there later, made within the record's times, is this folder's own.
//!
//! A record is of a place: a directory, named by its local item id, and a name in it,
//! compared folded as OneDrive compares names. It names the item whose step asked while
//! that item has no other record; an item has one record at most. A record outlives the
//! file that asked — its row gone, or its step asking at another place since — and then
//! names no file: the empty file may still hold the name for the next one. It does not
//! outlive its directory: the schema takes it with the directory's row.

use rusqlite::{params, Connection, OptionalExtension};

use super::memory_row::{folded_name_value, name_value};
use crate::model::{ItemPlace, LocalItemId};
use crate::TreeError;

/// How long a record that names no file is kept after its latest try whose outcome is not
/// known, in seconds: longer than an abandoned empty file of an upload session was seen to
/// live in OneDrive (a day).
pub const UPLOAD_SESSION_OPENING_KEPT: i64 = 7 * 24 * 3600;

/// What became of the record when an item's step said it is about to open a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpeningRecord {
    /// This call made the record: no earlier try of the item at this place is unanswered.
    Made,
    /// The item had a record at this place already, from a try whose outcome is not known:
    /// it keeps its first time. `last_unknown_at` is the time of that earlier try, to put
    /// back when this try is answered for certain.
    Carried { last_unknown_at: i64 },
}

/// The times between which an empty file at a place may be this folder's own: a record's
/// first try and its latest try whose outcome is not known. Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpeningWindow {
    pub first_at: i64,
    pub last_unknown_at: i64,
}

/// The step of `item` is about to open the session of a new file at `place`: written before
/// the request. A record of the item at the same place, as OneDrive compares names, keeps
/// its first time; one at another place names no file from now on, and a new one is made.
pub(super) fn remember_opening(conn: &Connection, item: LocalItemId, place: &ItemPlace, now: i64) -> Result<OpeningRecord, TreeError> {
    let (directory, folded_name) = (place.parent_local_item_id.as_i64(), folded_name_value(&place.name));
    let carried: Option<i64> = conn
        .prepare_cached("SELECT last_unknown_at FROM upload_session_openings WHERE file_local_item_id = ?1 AND directory_local_item_id = ?2 AND folded_name = ?3")?
        .query_row(params![item.as_i64(), directory, folded_name], |row| row.get(0))
        .optional()?;
    match carried {
        Some(last_unknown_at) => {
            conn.prepare_cached("UPDATE upload_session_openings SET name = ?2, last_unknown_at = ?3 WHERE file_local_item_id = ?1")?.execute(params![item.as_i64(), name_value(&place.name), now])?;
            Ok(OpeningRecord::Carried { last_unknown_at })
        }
        None => {
            conn.prepare_cached("UPDATE upload_session_openings SET file_local_item_id = NULL WHERE file_local_item_id = ?1")?.execute([item.as_i64()])?;
            conn.prepare_cached(
                "INSERT INTO upload_session_openings (directory_local_item_id, name, folded_name, file_local_item_id, first_at, last_unknown_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            )?
            .execute(params![directory, name_value(&place.name), folded_name, item.as_i64(), now])?;
            Ok(OpeningRecord::Made)
        }
    }
}

/// The try of `item` that `record` was answered about got a certain answer: it left no
/// empty file. A record that try made goes; a carried one is as it was before the try.
pub(super) fn settle_answered_opening(conn: &Connection, item: LocalItemId, record: OpeningRecord) -> Result<(), TreeError> {
    match record {
        OpeningRecord::Made => conn.prepare_cached("DELETE FROM upload_session_openings WHERE file_local_item_id = ?1")?.execute([item.as_i64()])?,
        OpeningRecord::Carried { last_unknown_at } => {
            conn.prepare_cached("UPDATE upload_session_openings SET last_unknown_at = ?2 WHERE file_local_item_id = ?1")?.execute(params![item.as_i64(), last_unknown_at])?
        }
    };
    Ok(())
}

/// The record of `item` goes: the address of its session is known and kept.
pub(super) fn forget_opening_of(conn: &Connection, item: LocalItemId) -> Result<(), TreeError> {
    conn.prepare_cached("DELETE FROM upload_session_openings WHERE file_local_item_id = ?1")?.execute([item.as_i64()])?;
    Ok(())
}

/// Every record at `place` goes, with a file or without: its empty file was removed, the
/// name was found free, or what holds the name is not this folder's.
pub(super) fn forget_openings_at(conn: &Connection, place: &ItemPlace) -> Result<(), TreeError> {
    conn.prepare_cached("DELETE FROM upload_session_openings WHERE directory_local_item_id = ?1 AND folded_name = ?2")?
        .execute(params![place.parent_local_item_id.as_i64(), folded_name_value(&place.name)])?;
    Ok(())
}

/// The records that name no file and whose latest try was before `time` go. How many.
pub(super) fn forget_openings_before(conn: &Connection, time: i64) -> Result<usize, TreeError> {
    Ok(conn.prepare_cached("DELETE FROM upload_session_openings WHERE file_local_item_id IS NULL AND last_unknown_at < ?1")?.execute([time])?)
}

/// The windows of the records at `place`, with a file or without, oldest first.
pub(super) fn list_opening_windows(conn: &Connection, place: &ItemPlace) -> Result<Vec<OpeningWindow>, TreeError> {
    let mut statement = conn.prepare_cached(
        "SELECT first_at, last_unknown_at FROM upload_session_openings WHERE directory_local_item_id = ?1 AND folded_name = ?2 ORDER BY first_at, rowid",
    )?;
    let windows = statement.query_map(params![place.parent_local_item_id.as_i64(), folded_name_value(&place.name)], |row| Ok(OpeningWindow { first_at: row.get(0)?, last_unknown_at: row.get(1)? }))?;
    Ok(windows.collect::<Result<Vec<_>, _>>()?)
}

/// The records in the directory `from` are in `to`: the binding of `from` is `to`'s now, and
/// the empty files are where OneDrive has that directory.
pub(super) fn move_openings(conn: &Connection, from: LocalItemId, to: LocalItemId) -> Result<(), TreeError> {
    conn.prepare_cached("UPDATE upload_session_openings SET directory_local_item_id = ?2 WHERE directory_local_item_id = ?1")?.execute(params![from.as_i64(), to.as_i64()])?;
    Ok(())
}
