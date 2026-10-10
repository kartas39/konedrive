//! A row of `step_memory` in the database: its columns, the one place it is read from SQLite
//! and written to it, and the table as the schema has it ([`schema`], [`ITEM_GONE_STATEMENTS`]).
//! What is here writes a row as it is told and asks no rule.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use konedrive_reason::Reason;
use rusqlite::types::{FromSql, Value};
use rusqlite::{Connection, OptionalExtension};

use super::item_memory::{FailedTries, TriedChange, ItemMemory, MismatchedItem, Refusal, RefusedVersion, RemovalConfirmation, SpaceRefusal, TakenName, TemporaryName, UploadSession, UploadSessionAddress, UploadTag};
use crate::model::{ContentVersion, DriveId, ItemPlace, ItemVersion, LocalItemId, OneDriveItemId, OneDriveItemReference};
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
        named(&["unsent_since"], "INTEGER"),
        named(&["session_address"], "TEXT"),
        named(&["session_expires_at", "session_next_offset"], "INTEGER"),
        content("session"),
        named(&["session_empty_file_parent_id"], "INTEGER"),
        named(&["session_empty_file_name", "session_empty_file_folded_name"], ""),
        named(&["mismatched_drive_id", "mismatched_item_id", "mismatched_ctag", "mismatched_etag"], "TEXT"),
        named(&["temporary_name"], ""),
        named(&["temporary_toward_parent_id"], "INTEGER"),
        named(&["temporary_toward_name"], ""),
        named(&["tries_count", "next_try_at"], "INTEGER"),
        named(&["last_failure", "tries_onedrive_etag"], "TEXT"),
        content("space_refusal"),
        named(&["space_refusal_free_space"], "INTEGER"),
        named(&["is_removal_confirmed"], "INTEGER"),
        named(&["refusal_reason"], "TEXT"),
        named(&["refused_parent_id"], "INTEGER"),
        named(&["refused_name"], ""),
        content("refused"),
        named(&["taken_parent_id"], "INTEGER"),
        named(&["taken_name", "taken_holder_name"], ""),
    ]
    .concat()
}

/// The table `step_memory`: one row per item by its local item id, every fact in columns of
/// its own and null while it is not remembered. `unsent_since` is the stamp the facts of an
/// unsent change were learnt under, and none of them is kept without it. The failed tries
/// with an eTag are of that version of OneDrive, and those with none of the unsent change.
/// A refusal with a
/// place is of the version shown there, and one without is of a removal. The item an
/// upload left is kept with its drive. Each reader that is not by the item has an index:
/// the items that wait until a time, those OneDrive refused for good and those it refused
/// for lack of space, those whose removal is confirmed, and by directory the names that are
/// taken in it, the versions that were refused in it and the empty files of upload
/// sessions in it; a session is found by its address, and the item an upload left by its
/// id.
pub(super) fn schema() -> String {
    let columns: String = columns().iter().map(|(name, kind)| format!("{name} {kind}, ")).collect();
    format!(
        "CREATE TABLE step_memory (
             local_item_id INTEGER PRIMARY KEY,
             {columns}
             CHECK (unsent_since IS NOT NULL OR (session_address IS NULL AND (tries_count IS NULL OR tries_onedrive_etag IS NOT NULL) AND space_refusal_size IS NULL
                AND is_removal_confirmed IS NULL AND refusal_reason IS NULL AND taken_parent_id IS NULL)),
             CHECK ((session_address IS NULL) = (session_next_offset IS NULL) AND (session_address IS NULL) = (session_size IS NULL)),
             CHECK (session_address IS NOT NULL OR (session_expires_at IS NULL AND session_empty_file_parent_id IS NULL)),
             CHECK ((session_empty_file_parent_id IS NULL) = (session_empty_file_name IS NULL)
                AND (session_empty_file_parent_id IS NULL) = (session_empty_file_folded_name IS NULL)),
             CHECK ((mismatched_item_id IS NULL) = (mismatched_drive_id IS NULL)),
             CHECK (mismatched_item_id IS NOT NULL OR (mismatched_ctag IS NULL AND mismatched_etag IS NULL)),
             CHECK ((temporary_name IS NULL) = (temporary_toward_parent_id IS NULL) AND (temporary_name IS NULL) = (temporary_toward_name IS NULL)),
             CHECK ((tries_count IS NULL) = (last_failure IS NULL) AND (next_try_at IS NULL OR tries_count IS NOT NULL)
                AND (tries_onedrive_etag IS NULL OR tries_count IS NOT NULL)),
             CHECK ((space_refusal_size IS NULL) = (space_refusal_free_space IS NULL)),
             CHECK ((refused_parent_id IS NULL) = (refused_name IS NULL)),
             CHECK (refusal_reason IS NOT NULL OR (refused_parent_id IS NULL AND refused_size IS NULL)),
             CHECK ((taken_parent_id IS NULL) = (taken_name IS NULL) AND (taken_parent_id IS NULL) = (taken_holder_name IS NULL)));
         CREATE INDEX step_memory_next_try ON step_memory(next_try_at, local_item_id) WHERE next_try_at IS NOT NULL;
         CREATE INDEX step_memory_refusal ON step_memory(local_item_id) WHERE refusal_reason IS NOT NULL;
         CREATE INDEX step_memory_space_refusal ON step_memory(local_item_id) WHERE space_refusal_size IS NOT NULL;
         CREATE INDEX step_memory_removal_confirmation ON step_memory(local_item_id) WHERE is_removal_confirmed IS NOT NULL;
         CREATE INDEX step_memory_taken_name ON step_memory(taken_parent_id) WHERE taken_parent_id IS NOT NULL;
         CREATE INDEX step_memory_refused_place ON step_memory(refused_parent_id) WHERE refused_parent_id IS NOT NULL;
         CREATE INDEX step_memory_session_empty_file ON step_memory(session_empty_file_parent_id, session_empty_file_folded_name)
             WHERE session_empty_file_parent_id IS NOT NULL;
         CREATE INDEX step_memory_session_address ON step_memory(session_address) WHERE session_address IS NOT NULL;
         CREATE INDEX step_memory_mismatched_item ON step_memory(mismatched_item_id) WHERE mismatched_item_id IS NOT NULL;
         CREATE INDEX step_memory_temporary_name ON step_memory(local_item_id) WHERE temporary_name IS NOT NULL;"
    )
}

/// What the trigger `step_memory_item_gone` does to this table when the row `OLD` of `items`
/// goes, after the other tables took what they keep past it: the row of what was
/// remembered of the item goes; and what other rows hold of a place in the item, a
/// directory, is taken from them, since no place will ever be in it again: the place of an
/// empty file, a taken name, and a refusal of a version at a place in it.
pub(super) const ITEM_GONE_STATEMENTS: &str = "
    DELETE FROM step_memory WHERE local_item_id = OLD.local_item_id;
    UPDATE step_memory SET session_empty_file_parent_id = NULL, session_empty_file_name = NULL, session_empty_file_folded_name = NULL
     WHERE session_empty_file_parent_id = OLD.local_item_id;
    UPDATE step_memory SET taken_parent_id = NULL, taken_name = NULL, taken_holder_name = NULL WHERE taken_parent_id = OLD.local_item_id;
    UPDATE step_memory SET refusal_reason = NULL, refused_parent_id = NULL, refused_name = NULL, refused_size = NULL, refused_mtime = NULL,
           refused_mtime_nsec = NULL, refused_hash = NULL, refused_ctag = NULL, refused_change_count = NULL
     WHERE refused_parent_id = OLD.local_item_id;";

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
    let unsent_since = read.next::<Option<i64>>()?.map(|unsent_since| unsent_since.max(0) as u64);
    let (address, expires_at, next_offset): (Option<String>, Option<i64>, Option<i64>) = (read.next()?, read.next()?, read.next()?);
    let (content, empty_file_place) = (read.content()?, read.place()?);
    read.next::<Value>()?;
    let upload_session = address.zip(content).map(|(address, content)| UploadSession {
        address: UploadSessionAddress::new(address),
        expires_at,
        next_offset: next_offset.unwrap_or(0).max(0) as u64,
        content,
        empty_file_place,
    });
    let (drive_id, item_id, ctag, etag): (Option<String>, Option<String>, Option<String>, Option<String>) = (read.next()?, read.next()?, read.next()?, read.next()?);
    let mismatched_item = mismatched_item_from(drive_id, item_id, ctag.as_deref(), etag.as_deref());
    let (name, toward) = (read.name()?, read.place()?);
    let temporary_name = name.zip(toward).map(|(name, toward)| TemporaryName { name, toward });
    let (count, next_try_at, last_failure): (Option<u32>, Option<i64>, Option<String>) = (read.next()?, read.next()?, read.next()?);
    // Tries with an eTag are of that version of OneDrive; the others of the unsent change.
    let tried = read.next::<Option<String>>()?.map(|etag| TriedChange::OfOneDrive { etag }).or(unsent_since.map(|unsent_since| TriedChange::Unsent { unsent_since }));
    let failed_tries = count.zip(last_failure).zip(tried).map(|((count, last_failure), tried)| FailedTries { count, next_try_at, last_failure: Reason::parse(&last_failure), tried });
    let (content, free_space) = (read.content()?, read.next::<Option<i64>>()?);
    let space_refusal = content.zip(free_space).map(|(content, free_space)| SpaceRefusal { content, free_space: free_space.max(0) as u64 });
    // The confirmation is of the removal the memory's stamp is of.
    let removal_confirmation = read.next::<Option<i64>>()?.and(unsent_since).map(|unsent_since| RemovalConfirmation { unsent_since });
    let (reason, place, content) = (read.next::<Option<String>>()?, read.place()?, read.content()?);
    // A refused version has a place; a refused removal has none.
    let refused = match place {
        Some(place) => RefusedVersion::Shown(ItemVersion { place: Some(place), content }),
        None => RefusedVersion::Removal,
    };
    let refusal = reason.map(|reason| Refusal { reason: Reason::parse(&reason), version: refused });
    let (place, holder_name) = (read.place()?, read.name()?);
    let taken_name = place.zip(holder_name).map(|(place, holder_name)| TakenName { place, holder_name });
    Ok(ItemMemory { unsent_since, upload_session, mismatched_item, temporary_name, failed_tries, space_refusal, removal_confirmation, refusal, taken_name })
}

/// The item an upload left, as two tables keep it: its drive, its id, and the tag of the
/// upload's answer. `None` where no id is kept, and for a drive id that is no drive's.
pub(super) fn mismatched_item_from(drive_id: Option<String>, item_id: Option<String>, ctag: Option<&str>, etag: Option<&str>) -> Option<MismatchedItem> {
    let item = OneDriveItemReference { drive_id: drive_id.and_then(DriveId::new)?, item_id: OneDriveItemId::new(item_id?) };
    Some(MismatchedItem::of_answer(item, ctag, etag))
}

/// The cTag and the eTag an upload's tag is kept as.
fn tag_values(tag: Option<&UploadTag>) -> [Value; 2] {
    let (ctag, etag) = match tag {
        Some(UploadTag::CTag(ctag)) => (Some(ctag.clone()), None),
        Some(UploadTag::ETag(etag)) => (None, Some(etag.clone())),
        Some(UploadTag::NotGiven) | None => (None, None),
    };
    [optional(ctag), optional(etag)]
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
    let mut values: Vec<Value> = vec![optional(memory.unsent_since.map(|unsent_since| unsent_since as i64))];
    let session = memory.upload_session.as_ref();
    values.push(optional(session.map(|session| session.address.as_str().to_owned())));
    values.push(optional(session.and_then(|session| session.expires_at)));
    values.push(optional(session.map(|session| session.next_offset as i64)));
    values.extend(content_values(session.map(|session| &session.content)));
    let empty_file_place = session.and_then(|session| session.empty_file_place.as_ref());
    values.extend(place_values(empty_file_place));
    values.push(empty_file_place.map_or(Value::Null, |place| folded_name_value(&place.name)));
    let mismatched = memory.mismatched_item.as_ref();
    values.push(optional(mismatched.map(|upload| upload.item.drive_id.as_str().to_owned())));
    values.push(optional(mismatched.map(|upload| upload.item.item_id.as_str().to_owned())));
    values.extend(tag_values(mismatched.map(|upload| &upload.tag)));
    let temporary = memory.temporary_name.as_ref();
    values.push(temporary.map_or(Value::Null, |temporary| name_value(&temporary.name)));
    values.extend(place_values(temporary.map(|temporary| &temporary.toward)));
    let tries = memory.failed_tries.as_ref();
    values.push(optional(tries.map(|tries| i64::from(tries.count))));
    values.push(optional(tries.and_then(|tries| tries.next_try_at)));
    values.push(optional(tries.map(|tries| tries.last_failure.to_string())));
    values.push(optional(tries.and_then(|tries| match &tries.tried {
        TriedChange::OfOneDrive { etag } => Some(etag.clone()),
        TriedChange::Unsent { .. } => None,
    })));
    let space_refusal = memory.space_refusal.as_ref();
    values.extend(content_values(space_refusal.map(|space_refusal| &space_refusal.content)));
    values.push(optional(space_refusal.map(|space_refusal| space_refusal.free_space as i64)));
    values.push(optional(memory.removal_confirmation.map(|_| 1i64)));
    let refusal = memory.refusal.as_ref();
    let refused = match refusal.map(|refusal| &refusal.version) {
        Some(RefusedVersion::Shown(version)) => Some(version),
        Some(RefusedVersion::Removal) | None => None,
    };
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
    let mut memory = memory.clone();
    memory.keep_stamp_only_with_its_facts();
    let memory = &memory;
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
