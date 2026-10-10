//! A row of `step_memory` in the database: its columns, the one place it is read from SQLite
//! and written to it, and what the schema holds of the three tables of what the steps
//! remember ([`schema`]). What is here writes a row as it is told and asks no rule.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use konedrive_reason::Reason;
use rusqlite::types::{FromSql, Value};
use rusqlite::{Connection, OptionalExtension};

use super::item_memory::{FailedTries, ItemMemory, MismatchedUpload, Refusal, TakenName, TemporaryName, TooBigForSpace, UploadSession, UploadSessionAddress, UploadTag};
use crate::model::{ContentVersion, ItemPlace, ItemVersion, LocalItemId, OneDriveItemId};
use crate::outbox::{path_from, path_value};
use crate::{FoldedName, TreeError};

/// The columns of a content, after the word that says whose it is, as `items` has them.
const CONTENT_COLUMNS: [&str; 6] = ["size", "mtime", "mtime_nsec", "hash", "ctag", "change_count"];

/// The columns of `step_memory` after its key, in the order [`memory_from`] reads them and
/// [`values_of`] gives them, each with its type in the schema.
fn columns() -> Vec<(String, &'static str)> {
    fn named(names: &[&str], kind: &'static str) -> Vec<(String, &'static str)> {
        names.iter().map(|name| ((*name).to_owned(), kind)).collect()
    }
    fn content(of: &str) -> Vec<(String, &'static str)> {
        CONTENT_COLUMNS.iter().map(|column| (format!("{of}_{column}"), if matches!(*column, "hash" | "ctag") { "TEXT" } else { "INTEGER" })).collect()
    }
    [
        named(&["session_address"], "TEXT"),
        named(&["session_expires_at", "session_next_offset"], "INTEGER"),
        content("session"),
        named(&["session_placeholder_parent_id"], "INTEGER"),
        named(&["session_placeholder_name", "session_placeholder_folded_name"], ""),
        named(&["mismatched_item_id", "mismatched_ctag", "mismatched_etag"], "TEXT"),
        named(&["temporary_name"], ""),
        named(&["temporary_toward_parent_id"], "INTEGER"),
        named(&["temporary_toward_name"], ""),
        named(&["tries_count", "next_try_at"], "INTEGER"),
        named(&["last_failure"], "TEXT"),
        content("too_big"),
        named(&["too_big_free_space"], "INTEGER"),
        named(&["is_removal_confirmed"], "INTEGER NOT NULL DEFAULT 0"),
        named(&["refusal_reason"], "TEXT"),
        named(&["refused_parent_id"], "INTEGER"),
        named(&["refused_name"], ""),
        content("refused"),
        named(&["taken_parent_id"], "INTEGER"),
        named(&["taken_name", "taken_holder_name"], ""),
    ]
    .concat()
}

/// The rows that hold a reason for the user: a refusal for good, or a file too big for the
/// space. What the index `step_memory_user_reason` holds.
pub(super) const HAS_USER_REASON: &str = "(refusal_reason IS NOT NULL OR too_big_size IS NOT NULL)";

/// What a store is created with for what the steps remember: `step_memory`, one row per item
/// by its local item id, every fact in columns of its own and null while it is not
/// remembered; `upload_session_openings`, the upload sessions the folder was about to open,
/// by the place their empty file takes in OneDrive, and the item that asked;
/// `abandoned_upload_sessions`, the addresses of sessions nothing goes on with, kept until
/// each is cancelled. The trigger `step_memory_item_gone` holds "a fact does not outlive its
/// item" whoever deletes a row of `items`: the row of what was remembered of the item goes,
/// the address of its upload session is kept among the abandoned ones, the openings in a
/// directory go with the directory, and an opening whose file's row went names no file.
pub(crate) fn schema() -> String {
    let columns: String = columns().iter().map(|(name, kind)| format!("{name} {kind}, ")).collect();
    format!(
        "CREATE TABLE step_memory (
             local_item_id INTEGER PRIMARY KEY,
             {columns}
             CHECK ((session_address IS NULL) = (session_next_offset IS NULL) AND (session_address IS NULL) = (session_size IS NULL)),
             CHECK (session_address IS NOT NULL OR (session_expires_at IS NULL AND session_placeholder_parent_id IS NULL)),
             CHECK ((session_placeholder_parent_id IS NULL) = (session_placeholder_name IS NULL)
                AND (session_placeholder_parent_id IS NULL) = (session_placeholder_folded_name IS NULL)),
             CHECK (mismatched_item_id IS NOT NULL OR (mismatched_ctag IS NULL AND mismatched_etag IS NULL)),
             CHECK ((temporary_name IS NULL) = (temporary_toward_parent_id IS NULL) AND (temporary_name IS NULL) = (temporary_toward_name IS NULL)),
             CHECK ((tries_count IS NULL) = (last_failure IS NULL) AND (next_try_at IS NULL OR tries_count IS NOT NULL)),
             CHECK ((too_big_size IS NULL) = (too_big_free_space IS NULL)),
             CHECK ((refused_parent_id IS NULL) = (refused_name IS NULL)),
             CHECK (refusal_reason IS NOT NULL OR (refused_parent_id IS NULL AND refused_size IS NULL)),
             CHECK ((taken_parent_id IS NULL) = (taken_name IS NULL) AND (taken_parent_id IS NULL) = (taken_holder_name IS NULL)));
         CREATE INDEX step_memory_next_try ON step_memory(next_try_at, local_item_id) WHERE next_try_at IS NOT NULL;
         CREATE INDEX step_memory_user_reason ON step_memory(local_item_id) WHERE {HAS_USER_REASON};
         CREATE INDEX step_memory_taken_name ON step_memory(taken_parent_id) WHERE taken_parent_id IS NOT NULL;
         CREATE INDEX step_memory_session_placeholder ON step_memory(session_placeholder_parent_id, session_placeholder_folded_name)
             WHERE session_placeholder_parent_id IS NOT NULL;
         CREATE TABLE upload_session_openings (
             directory_local_item_id INTEGER NOT NULL, name NOT NULL, folded_name NOT NULL,
             file_local_item_id INTEGER, first_at INTEGER NOT NULL, last_unknown_at INTEGER NOT NULL);
         CREATE INDEX upload_session_openings_place ON upload_session_openings(directory_local_item_id, folded_name);
         CREATE UNIQUE INDEX upload_session_openings_file ON upload_session_openings(file_local_item_id) WHERE file_local_item_id IS NOT NULL;
         CREATE TABLE abandoned_upload_sessions (
             abandoned_order INTEGER PRIMARY KEY AUTOINCREMENT, address TEXT NOT NULL UNIQUE,
             placeholder_parent_id INTEGER, placeholder_folded_name,
             CHECK ((placeholder_parent_id IS NULL) = (placeholder_folded_name IS NULL)));
         CREATE INDEX abandoned_upload_sessions_placeholder ON abandoned_upload_sessions(placeholder_parent_id, placeholder_folded_name)
             WHERE placeholder_parent_id IS NOT NULL;
         CREATE TRIGGER step_memory_item_gone AFTER DELETE ON items BEGIN
             INSERT OR IGNORE INTO abandoned_upload_sessions (address, placeholder_parent_id, placeholder_folded_name)
                 SELECT session_address, session_placeholder_parent_id, session_placeholder_folded_name FROM step_memory
                  WHERE local_item_id = OLD.local_item_id AND session_address IS NOT NULL;
             DELETE FROM step_memory WHERE local_item_id = OLD.local_item_id;
             DELETE FROM upload_session_openings WHERE directory_local_item_id = OLD.local_item_id;
             UPDATE upload_session_openings SET file_local_item_id = NULL WHERE file_local_item_id = OLD.local_item_id;
             UPDATE step_memory SET session_placeholder_parent_id = NULL, session_placeholder_name = NULL, session_placeholder_folded_name = NULL
              WHERE session_placeholder_parent_id = OLD.local_item_id;
             UPDATE abandoned_upload_sessions SET placeholder_parent_id = NULL, placeholder_folded_name = NULL
              WHERE placeholder_parent_id = OLD.local_item_id;
         END;"
    )
}

/// A name as the tables keep it, as `items` keeps one: text when it is UTF-8 and its bytes
/// otherwise.
pub(super) fn name_value(name: &OsStr) -> Value {
    path_value(Path::new(name))
}

/// A name folded as OneDrive compares names, as the tables keep it.
pub(super) fn folded_name_value(name: &OsStr) -> Value {
    name_value(FoldedName::of(name).as_os_str())
}

/// The columns of a row, read one after the other in the order of [`columns`].
struct RowReader<'a, 'r> {
    row: &'a rusqlite::Row<'r>,
    at: usize,
}

impl RowReader<'_, '_> {
    fn next<T: FromSql>(&mut self) -> rusqlite::Result<T> {
        self.at += 1;
        self.row.get(self.at - 1)
    }

    fn name(&mut self) -> rusqlite::Result<Option<OsString>> {
        self.at += 1;
        let value = self.row.get_ref(self.at - 1)?;
        Ok((value != rusqlite::types::ValueRef::Null).then(|| path_from(value).into_os_string()))
    }

    fn place(&mut self) -> rusqlite::Result<Option<ItemPlace>> {
        let (parent, name) = (self.next::<Option<i64>>()?, self.name()?);
        Ok(parent.zip(name).map(|(parent, name)| ItemPlace { parent_local_item_id: LocalItemId::new(parent), name }))
    }

    fn content(&mut self) -> rusqlite::Result<Option<ContentVersion>> {
        let (size, mtime, mtime_nsec): (Option<i64>, Option<i64>, Option<u32>) = (self.next()?, self.next()?, self.next()?);
        let (hash, ctag, change_count): (Option<String>, Option<String>, Option<i64>) = (self.next()?, self.next()?, self.next()?);
        let Some(size) = size else { return Ok(None) };
        let (size, mtime, mtime_nsec) = (size.max(0) as u64, mtime.unwrap_or(0), mtime_nsec.unwrap_or(0));
        Ok(Some(match change_count {
            Some(change_count) => ContentVersion::ChangedHere { size, mtime, mtime_nsec, change_count: change_count.max(0) as u64 },
            None => ContentVersion::Hashed { size, mtime, mtime_nsec, hash, ctag },
        }))
    }
}

/// An [`ItemMemory`] read from a query that selects [`columns`], from the column `first`.
fn memory_from(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<ItemMemory> {
    let mut read = RowReader { row, at: first };
    let (address, expires_at, next_offset): (Option<String>, Option<i64>, Option<i64>) = (read.next()?, read.next()?, read.next()?);
    let (content, placeholder) = (read.content()?, read.place()?);
    read.next::<Value>()?;
    let upload_session = address.zip(content).map(|(address, content)| UploadSession {
        address: UploadSessionAddress::new(address),
        expires_at,
        next_offset: next_offset.unwrap_or(0).max(0) as u64,
        content,
        placeholder,
    });
    let (item_id, ctag, etag): (Option<String>, Option<String>, Option<String>) = (read.next()?, read.next()?, read.next()?);
    let mismatched_upload = item_id.map(|item_id| MismatchedUpload::of_answer(OneDriveItemId::new(item_id), ctag.as_deref(), etag.as_deref()));
    let (name, toward) = (read.name()?, read.place()?);
    let temporary_name = name.zip(toward).map(|(name, toward)| TemporaryName { name, toward });
    let (count, next_try_at, last_failure): (Option<u32>, Option<i64>, Option<String>) = (read.next()?, read.next()?, read.next()?);
    let failed_tries = count.zip(last_failure).map(|(count, last_failure)| FailedTries { count, next_try_at, last_failure: Reason::parse(&last_failure) });
    let (content, free_space) = (read.content()?, read.next::<Option<i64>>()?);
    let too_big_for_space = content.zip(free_space).map(|(content, free_space)| TooBigForSpace { content, free_space: free_space.max(0) as u64 });
    let is_removal_confirmed = read.next::<i64>()? != 0;
    let (reason, place, content) = (read.next::<Option<String>>()?, read.place()?, read.content()?);
    // A refused version has a place; a refusal with none is that of a removal.
    let refusal = reason.map(|reason| Refusal { reason: Reason::parse(&reason), version: place.map(|place| ItemVersion { place: Some(place), content }) });
    let (place, holder_name) = (read.place()?, read.name()?);
    let taken_name = place.zip(holder_name).map(|(place, holder_name)| TakenName { place, holder_name });
    Ok(ItemMemory { upload_session, mismatched_upload, temporary_name, failed_tries, too_big_for_space, is_removal_confirmed, refusal, taken_name })
}

fn content_values(content: Option<&ContentVersion>) -> [Value; 6] {
    match content {
        None => [Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null],
        Some(ContentVersion::Hashed { size, mtime, mtime_nsec, hash, ctag }) => {
            [(*size as i64).into(), (*mtime).into(), i64::from(*mtime_nsec).into(), hash.clone().into(), ctag.clone().into(), Value::Null]
        }
        Some(ContentVersion::ChangedHere { size, mtime, mtime_nsec, change_count }) => {
            [(*size as i64).into(), (*mtime).into(), i64::from(*mtime_nsec).into(), Value::Null, Value::Null, (*change_count as i64).into()]
        }
    }
}

fn place_values(place: Option<&ItemPlace>) -> [Value; 2] {
    match place {
        Some(place) => [place.parent_local_item_id.as_i64().into(), name_value(&place.name)],
        None => [Value::Null, Value::Null],
    }
}

fn optional<T: Into<Value>>(value: Option<T>) -> Value {
    value.map_or(Value::Null, Into::into)
}

/// The values of [`columns`], in their order.
fn values_of(memory: &ItemMemory) -> Vec<Value> {
    let mut values: Vec<Value> = Vec::new();
    let session = memory.upload_session.as_ref();
    values.push(optional(session.map(|session| session.address.as_str().to_owned())));
    values.push(optional(session.and_then(|session| session.expires_at)));
    values.push(optional(session.map(|session| session.next_offset as i64)));
    values.extend(content_values(session.map(|session| &session.content)));
    let placeholder = session.and_then(|session| session.placeholder.as_ref());
    values.extend(place_values(placeholder));
    values.push(placeholder.map_or(Value::Null, |place| folded_name_value(&place.name)));
    let mismatched = memory.mismatched_upload.as_ref();
    values.push(optional(mismatched.map(|upload| upload.item_id.as_str().to_owned())));
    let (ctag, etag) = match mismatched.map(|upload| &upload.tag) {
        Some(UploadTag::CTag(ctag)) => (Some(ctag.clone()), None),
        Some(UploadTag::ETag(etag)) => (None, Some(etag.clone())),
        Some(UploadTag::NotGiven) | None => (None, None),
    };
    values.extend([optional(ctag), optional(etag)]);
    let temporary = memory.temporary_name.as_ref();
    values.push(temporary.map_or(Value::Null, |temporary| name_value(&temporary.name)));
    values.extend(place_values(temporary.map(|temporary| &temporary.toward)));
    let tries = memory.failed_tries.as_ref();
    values.push(optional(tries.map(|tries| i64::from(tries.count))));
    values.push(optional(tries.and_then(|tries| tries.next_try_at)));
    values.push(optional(tries.map(|tries| tries.last_failure.to_string())));
    let too_big = memory.too_big_for_space.as_ref();
    values.extend(content_values(too_big.map(|too_big| &too_big.content)));
    values.push(optional(too_big.map(|too_big| too_big.free_space as i64)));
    values.push(i64::from(memory.is_removal_confirmed).into());
    let refusal = memory.refusal.as_ref();
    let refused = refusal.and_then(|refusal| refusal.version.as_ref());
    values.push(optional(refusal.map(|refusal| refusal.reason.to_string())));
    values.extend(place_values(refused.and_then(|version| version.place.as_ref())));
    values.extend(content_values(refused.and_then(|version| version.content.as_ref())));
    let taken = memory.taken_name.as_ref();
    values.extend(place_values(taken.map(|taken| &taken.place)));
    values.push(taken.map_or(Value::Null, |taken| name_value(&taken.holder_name)));
    values
}

fn column_names() -> String {
    columns().iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(", ")
}

/// What is kept of `item`, whether its row still says it or not; nothing when no row is
/// kept.
pub(super) fn read_memory(conn: &Connection, item: LocalItemId) -> Result<ItemMemory, TreeError> {
    let sql = format!("SELECT {} FROM step_memory WHERE local_item_id = ?1", column_names());
    Ok(conn.prepare_cached(&sql)?.query_row([item.as_i64()], |row| memory_from(row, 0)).optional()?.unwrap_or_default())
}

/// `memory` is what is kept of `item` from now on, whole; a memory with nothing in it keeps
/// no row.
pub(super) fn write_memory(conn: &Connection, item: LocalItemId, memory: &ItemMemory) -> Result<(), TreeError> {
    if memory.is_empty() {
        conn.prepare_cached("DELETE FROM step_memory WHERE local_item_id = ?1")?.execute([item.as_i64()])?;
        return Ok(());
    }
    let mut values = vec![Value::from(item.as_i64())];
    values.extend(values_of(memory));
    let marks = (1..=values.len()).map(|n| format!("?{n}")).collect::<Vec<_>>().join(", ");
    let sql = format!("INSERT OR REPLACE INTO step_memory (local_item_id, {}) VALUES ({marks})", column_names());
    conn.prepare_cached(&sql)?.execute(rusqlite::params_from_iter(values))?;
    Ok(())
}

/// The rows a query finds, each with its item: `condition` is what follows `WHERE`, with
/// `values` for its marks, and may end with an order and a limit.
pub(super) fn list_memories(conn: &Connection, condition: &str, values: &[Value]) -> Result<Vec<(LocalItemId, ItemMemory)>, TreeError> {
    let sql = format!("SELECT local_item_id, {} FROM step_memory WHERE {condition}", column_names());
    let mut statement = conn.prepare_cached(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(values), |row| Ok((LocalItemId::new(row.get(0)?), memory_from(row, 1)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}
