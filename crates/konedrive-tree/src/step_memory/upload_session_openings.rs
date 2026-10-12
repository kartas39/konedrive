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
//!
//! *Voided by:* the answer to the request it was kept for — a certain answer that left no
//! empty file ([`settle_answered_opening`]), or the address of the session, which tells
//! the empty file from then on ([`forget_opening_of`]); the step that read what holds the
//! place and resolved it ([`forget_openings_at`]); and its time
//! ([`forget_openings_before`], [`UPLOAD_SESSION_OPENING_KEPT`] after its latest try). A
//! record says what this folder asked, not what OneDrive has, so no listing voids it.

use rusqlite::{params, Connection, OptionalExtension};

use super::memory_row::{folded_name_value, name_value};
use crate::model::{ItemPlace, LocalItemId};
use crate::TreeError;

/// How long a record that names no file is kept after its latest try whose outcome is not
/// known, in seconds: longer than an abandoned empty file of an upload session was seen to
/// live in OneDrive (a day).
pub const UPLOAD_SESSION_OPENING_KEPT: i64 = 7 * 24 * 3600;

/// A try of an item's step to open an upload session at a place, as it stands among the
/// tries of that item there: what the step gives back when the try is answered for certain
/// ([`settle_answered_opening`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpeningTry {
    /// No earlier try of the item at this place is unanswered: the record is this try's.
    First,
    /// The item had a record at this place already, from a try whose outcome is not known:
    /// the record keeps its first time. `last_unknown_at` is the time of that earlier try,
    /// to put back when this try is answered for certain.
    Repeated { last_unknown_at: i64 },
}

/// The table: a record by the directory and the folded name of its place, with the name as
/// it was asked, the file whose step asked while that file has no other record, the time of
/// the first try and of the latest try whose outcome is not known.
pub(super) fn schema() -> &'static str {
    "CREATE TABLE upload_session_openings (
         directory_local_item_id INTEGER NOT NULL, name NOT NULL, folded_name NOT NULL,
         file_local_item_id INTEGER, first_at INTEGER NOT NULL, last_unknown_at INTEGER NOT NULL);
     CREATE INDEX upload_session_openings_place ON upload_session_openings(directory_local_item_id, folded_name);
     CREATE UNIQUE INDEX upload_session_openings_file ON upload_session_openings(file_local_item_id) WHERE file_local_item_id IS NOT NULL;"
}

/// What the trigger `step_memory_item_gone` does to this table when the row `OLD` of `items`
/// goes: the records in a directory go with the directory, and a record whose file's row
/// went names no file.
pub(super) const ITEM_GONE_STATEMENTS: &str = "
    DELETE FROM upload_session_openings WHERE directory_local_item_id = OLD.local_item_id;
    UPDATE upload_session_openings SET file_local_item_id = NULL WHERE file_local_item_id = OLD.local_item_id;";

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
pub(super) fn remember_opening(conn: &Connection, item: LocalItemId, place: &ItemPlace, now: i64) -> Result<OpeningTry, TreeError> {
    let (directory, folded_name) = (place.parent_local_item_id.as_i64(), folded_name_value(&place.name));
    let carried: Option<i64> = conn
        .prepare_cached("SELECT last_unknown_at FROM upload_session_openings WHERE file_local_item_id = ?1 AND directory_local_item_id = ?2 AND folded_name = ?3")?
        .query_row(params![item.as_i64(), directory, folded_name], |row| row.get(0))
        .optional()?;
    match carried {
        Some(last_unknown_at) => {
            conn.prepare_cached("UPDATE upload_session_openings SET name = ?2, last_unknown_at = ?3 WHERE file_local_item_id = ?1")?.execute(params![item.as_i64(), name_value(&place.name), now])?;
            Ok(OpeningTry::Repeated { last_unknown_at })
        }
        None => {
            conn.prepare_cached("UPDATE upload_session_openings SET file_local_item_id = NULL WHERE file_local_item_id = ?1")?.execute([item.as_i64()])?;
            conn.prepare_cached(
                "INSERT INTO upload_session_openings (directory_local_item_id, name, folded_name, file_local_item_id, first_at, last_unknown_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            )?
            .execute(params![directory, name_value(&place.name), folded_name, item.as_i64(), now])?;
            Ok(OpeningTry::First)
        }
    }
}

/// The try `opening_try` of `item` got a certain answer: it left no empty file. The record
/// of a first try goes; that of a repeated one is as it was before the try.
pub(crate) fn settle_answered_opening(conn: &Connection, item: LocalItemId, opening_try: OpeningTry) -> Result<(), TreeError> {
    match opening_try {
        OpeningTry::First => conn.prepare_cached("DELETE FROM upload_session_openings WHERE file_local_item_id = ?1")?.execute([item.as_i64()])?,
        OpeningTry::Repeated { last_unknown_at } => {
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
pub(crate) fn forget_openings_at(conn: &Connection, place: &ItemPlace) -> Result<(), TreeError> {
    conn.prepare_cached("DELETE FROM upload_session_openings WHERE directory_local_item_id = ?1 AND folded_name = ?2")?
        .execute(params![place.parent_local_item_id.as_i64(), folded_name_value(&place.name)])?;
    Ok(())
}

/// The records that name no file and whose latest try was before `time` go. How many.
pub(crate) fn forget_openings_before(conn: &Connection, time: i64) -> Result<usize, TreeError> {
    Ok(conn.prepare_cached("DELETE FROM upload_session_openings WHERE file_local_item_id IS NULL AND last_unknown_at < ?1")?.execute([time])?)
}

/// The windows of the records at `place`, with a file or without, oldest first.
pub(crate) fn list_opening_windows(conn: &Connection, place: &ItemPlace) -> Result<Vec<OpeningWindow>, TreeError> {
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
