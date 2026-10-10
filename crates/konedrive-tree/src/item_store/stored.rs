//! A row of `items` in the database: the one place it is read from SQLite, written to it
//! and removed from it. What is here writes a row as it is told, with no rule of the rows
//! asked: only [`ItemStore`](super::ItemStore)'s operations and the operations on bindings
//! call it, and nothing outside `item_store` can.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rusqlite::types::Value;
use rusqlite::{params, Connection, OptionalExtension};

use super::refuse;
use crate::model::{ContentPresence, ContentVersion, DriveId, Item, ItemKind, ItemPlace, ItemVersion, LocalItemId, OneDriveItemId, OneDriveItemReference};
use crate::outbox::{path_from, path_value};
use crate::TreeError;

/// The columns of a content, after the word that says whose it is.
const CONTENT_COLUMNS: [&str; 6] = ["size", "mtime", "mtime_nsec", "hash", "ctag", "change_count"];

/// The columns of an [`Item`], in the order [`item_from`] reads them and [`values_of`] gives them.
const ITEM_COLUMNS: &str = "local_item_id, onedrive_drive_id, onedrive_item_id, kind, \
     local_parent_id, local_name, local_size, local_mtime, local_mtime_nsec, local_hash, local_ctag, local_change_count, \
     synced_parent_id, synced_name, synced_size, synced_mtime, synced_mtime_nsec, synced_hash, synced_ctag, synced_change_count, \
     content_presence, \
     before_write_size, before_write_mtime, before_write_mtime_nsec, before_write_hash, before_write_ctag, before_write_change_count, \
     is_name_ignored";

/// Where each column of [`ITEM_COLUMNS`] is; a content's is where its first column is.
mod at {
    use super::ITEM_COLUMNS;
    use crate::model::column;

    pub(super) const LOCAL_ITEM_ID: usize = column(ITEM_COLUMNS, "local_item_id");
    pub(super) const ONEDRIVE_DRIVE_ID: usize = column(ITEM_COLUMNS, "onedrive_drive_id");
    pub(super) const ONEDRIVE_ITEM_ID: usize = column(ITEM_COLUMNS, "onedrive_item_id");
    pub(super) const KIND: usize = column(ITEM_COLUMNS, "kind");
    pub(super) const LOCAL_PARENT_ID: usize = column(ITEM_COLUMNS, "local_parent_id");
    pub(super) const LOCAL_NAME: usize = column(ITEM_COLUMNS, "local_name");
    pub(super) const LOCAL_CONTENT: usize = column(ITEM_COLUMNS, "local_size");
    pub(super) const SYNCED_PARENT_ID: usize = column(ITEM_COLUMNS, "synced_parent_id");
    pub(super) const SYNCED_NAME: usize = column(ITEM_COLUMNS, "synced_name");
    pub(super) const SYNCED_CONTENT: usize = column(ITEM_COLUMNS, "synced_size");
    pub(super) const CONTENT_PRESENCE: usize = column(ITEM_COLUMNS, "content_presence");
    pub(super) const CONTENT_BEFORE_WRITE: usize = column(ITEM_COLUMNS, "before_write_size");
    pub(super) const IS_NAME_IGNORED: usize = column(ITEM_COLUMNS, "is_name_ignored");
}

/// A name as `items` keeps it: text when it is UTF-8 and its bytes otherwise, as the outbox
/// keeps a path, so that one name always has one form and is found by equality.
fn name_value(name: &OsStr) -> Value {
    path_value(Path::new(name))
}

/// The one check of every name a row is written with, local or synced: the name of a
/// directory entry is not empty, not `.` or `..`, and holds no `/` and no NUL. An item's
/// path is made of the stored names, and the disk is touched by it.
fn check_name(name: &OsStr) -> Result<(), TreeError> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') || bytes.contains(&0) {
        return Err(refuse(format!("{name:?} is no name of a directory entry")));
    }
    Ok(())
}

fn check_names(item_local: Option<&ItemVersion>, synced_place: Option<&ItemPlace>) -> Result<(), TreeError> {
    item_local.and_then(|local| local.place.as_ref()).into_iter().chain(synced_place).try_for_each(|place| check_name(&place.name))
}

fn content_from(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<Option<ContentVersion>> {
    let Some(size) = row.get::<_, Option<i64>>(first)? else { return Ok(None) };
    let (size, mtime, mtime_nsec) = (size.max(0) as u64, row.get::<_, Option<i64>>(first + 1)?.unwrap_or(0), row.get::<_, Option<u32>>(first + 2)?.unwrap_or(0));
    Ok(Some(match row.get::<_, Option<i64>>(first + 5)? {
        Some(change_count) => ContentVersion::ChangedHere { size, mtime, mtime_nsec, change_count: change_count.max(0) as u64 },
        None => ContentVersion::Hashed { size, mtime, mtime_nsec, hash: row.get(first + 3)?, ctag: row.get(first + 4)? },
    }))
}

fn place_from(row: &rusqlite::Row<'_>, parent: usize, name: usize) -> rusqlite::Result<Option<ItemPlace>> {
    Ok(row
        .get::<_, Option<i64>>(parent)?
        .map(|parent| -> rusqlite::Result<ItemPlace> { Ok(ItemPlace { parent_local_item_id: LocalItemId::new(parent), name: path_from(row.get_ref(name)?).into_os_string() }) })
        .transpose()?)
}

/// An [`Item`] read from a query that selects [`ITEM_COLUMNS`].
fn item_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    let (drive_id, item_id): (Option<String>, Option<String>) = (row.get(at::ONEDRIVE_DRIVE_ID)?, row.get(at::ONEDRIVE_ITEM_ID)?);
    let onedrive_item = match (drive_id.and_then(DriveId::new), item_id) {
        (Some(drive_id), Some(item_id)) => Some(OneDriveItemReference { drive_id, item_id: OneDriveItemId::new(item_id) }),
        _ => None,
    };
    let local_item_id = LocalItemId::new(row.get(at::LOCAL_ITEM_ID)?);
    // The folder shows what has a local place, and the root, which has none.
    let local = match place_from(row, at::LOCAL_PARENT_ID, at::LOCAL_NAME)? {
        Some(place) => Some(ItemVersion { place: Some(place), content: content_from(row, at::LOCAL_CONTENT)? }),
        None if local_item_id == LocalItemId::ROOT => Some(ItemVersion { place: None, content: None }),
        None => None,
    };
    Ok(Item {
        local_item_id,
        onedrive_item,
        kind: ItemKind::decode(&row.get::<_, String>(at::KIND)?),
        local,
        synced_place: place_from(row, at::SYNCED_PARENT_ID, at::SYNCED_NAME)?,
        synced_content: content_from(row, at::SYNCED_CONTENT)?,
        content_presence: ContentPresence::decode(&row.get::<_, String>(at::CONTENT_PRESENCE)?),
        content_before_write: content_from(row, at::CONTENT_BEFORE_WRITE)?,
        is_name_ignored: row.get::<_, i64>(at::IS_NAME_IGNORED)? != 0,
    })
}

fn content_values(content: Option<&ContentVersion>) -> [Value; CONTENT_COLUMNS.len()] {
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

/// The value of every column of [`ITEM_COLUMNS`] but the first, the key, in their order.
fn values_of(item: &Item) -> Vec<Value> {
    let binding = item.onedrive_item.as_ref();
    let local = item.local.as_ref();
    let mut values: Vec<Value> = vec![binding.map(|b| b.drive_id.as_str().to_owned()).into(), binding.map(|b| b.item_id.as_str().to_owned()).into()];
    values.push(item.kind.as_str().to_owned().into());
    values.extend(place_values(local.and_then(|local| local.place.as_ref())));
    values.extend(content_values(local.and_then(|local| local.content.as_ref())));
    values.extend(place_values(item.synced_place.as_ref()));
    values.extend(content_values(item.synced_content.as_ref()));
    values.push(item.content_presence.as_str().to_owned().into());
    values.extend(content_values(item.content_before_write.as_ref()));
    values.push(i64::from(item.is_name_ignored).into());
    values
}

/// The names of [`ITEM_COLUMNS`] but the first, the key.
fn written_columns() -> impl Iterator<Item = &'static str> {
    ITEM_COLUMNS.split(',').map(str::trim).skip(1)
}

/// A new row: an item of `kind` with this binding and this local version, and nothing
/// agreed on for it yet. The id the row was given.
pub(super) fn insert_item(
    conn: &Connection,
    kind: ItemKind,
    onedrive_item: Option<&OneDriveItemReference>,
    local: Option<&ItemVersion>,
    content_presence: ContentPresence,
    is_name_ignored: bool,
) -> Result<LocalItemId, TreeError> {
    check_names(local, None)?;
    let mut values: Vec<Value> = vec![onedrive_item.map(|b| b.drive_id.as_str().to_owned()).into(), onedrive_item.map(|b| b.item_id.as_str().to_owned()).into(), kind.as_str().to_owned().into()];
    values.extend(place_values(local.and_then(|local| local.place.as_ref())));
    values.extend(content_values(local.and_then(|local| local.content.as_ref())));
    values.push(content_presence.as_str().to_owned().into());
    values.push(i64::from(is_name_ignored).into());
    let content = CONTENT_COLUMNS.map(|column| format!("local_{column}")).join(", ");
    let places = (1..=values.len()).map(|n| format!("?{n}")).collect::<Vec<_>>().join(", ");
    let sql = format!("INSERT INTO items (onedrive_drive_id, onedrive_item_id, kind, local_parent_id, local_name, {content}, content_presence, is_name_ignored) VALUES ({places})");
    conn.prepare_cached(&sql)?.execute(rusqlite::params_from_iter(values))?;
    Ok(LocalItemId::new(conn.last_insert_rowid()))
}

/// The row of `item.local_item_id` is `item` from now on.
pub(super) fn write_item(conn: &Connection, item: &Item) -> Result<(), TreeError> {
    check_names(item.local.as_ref(), item.synced_place.as_ref())?;
    let sets = written_columns().enumerate().map(|(n, column)| format!("{column} = ?{}", n + 1)).collect::<Vec<_>>().join(", ");
    let mut values = values_of(item);
    values.push(item.local_item_id.as_i64().into());
    let sql = format!("UPDATE items SET {sets} WHERE local_item_id = ?{}", values.len());
    if conn.prepare_cached(&sql)?.execute(rusqlite::params_from_iter(values))? == 0 {
        return Err(refuse(format!("item {} has no row to write", item.local_item_id)));
    }
    Ok(())
}

/// The row of `item` goes. Its id is not given again.
pub(super) fn delete_item(conn: &Connection, item: LocalItemId) -> Result<(), TreeError> {
    conn.prepare_cached("DELETE FROM items WHERE local_item_id = ?1")?.execute([item.as_i64()])?;
    Ok(())
}

pub(super) fn find_item(conn: &Connection, item: LocalItemId) -> Result<Option<Item>, TreeError> {
    let sql = format!("SELECT {ITEM_COLUMNS} FROM items WHERE local_item_id = ?1");
    Ok(conn.prepare_cached(&sql)?.query_row([item.as_i64()], item_from).optional()?)
}

pub(super) fn find_item_at(conn: &Connection, directory: LocalItemId, name: &OsStr) -> Result<Option<Item>, TreeError> {
    let sql = format!("SELECT {ITEM_COLUMNS} FROM items WHERE local_parent_id = ?1 AND local_name = ?2");
    Ok(conn.prepare_cached(&sql)?.query_row(params![directory.as_i64(), name_value(name)], item_from).optional()?)
}

pub(super) fn find_bound_item(conn: &Connection, onedrive_item: &OneDriveItemReference) -> Result<Option<Item>, TreeError> {
    let sql = format!("SELECT {ITEM_COLUMNS} FROM items WHERE onedrive_drive_id = ?1 AND onedrive_item_id = ?2");
    Ok(conn.prepare_cached(&sql)?.query_row([onedrive_item.drive_id.as_str(), onedrive_item.item_id.as_str()], item_from).optional()?)
}

pub(super) fn list_children(conn: &Connection, directory: LocalItemId) -> Result<Vec<Item>, TreeError> {
    let sql = format!("SELECT {ITEM_COLUMNS} FROM items WHERE local_parent_id = ?1 ORDER BY local_name");
    let mut statement = conn.prepare_cached(&sql)?;
    let children = statement.query_map([directory.as_i64()], item_from)?.collect::<Result<Vec<_>, _>>()?;
    Ok(children)
}

/// `item` is at `place` in the folder from now on, and `is_name_ignored` is what the upload
/// rules say of its name there: the two are written together, since the one is said of the
/// other. The index of local places refuses a place another item has.
pub(super) fn set_local_place(conn: &Connection, item: LocalItemId, place: &ItemPlace, is_name_ignored: bool) -> Result<(), TreeError> {
    check_name(&place.name)?;
    write_local_place(conn, item, place.parent_local_item_id, name_value(&place.name), is_name_ignored)
}

/// `item` steps aside in the directory it is in, to a name no file can have (it holds a NUL
/// and a `/`, which [`check_name`] lets no stored name hold) and no other item has (it
/// holds the item's id), so that another item can take its place before it takes its next
/// one. The name must not outlive the transaction.
pub(super) fn set_local_place_aside(conn: &Connection, item: LocalItemId, place: &ItemPlace, is_name_ignored: bool) -> Result<(), TreeError> {
    write_local_place(conn, item, place.parent_local_item_id, Value::Blob(format!("\0/{item}").into_bytes()), is_name_ignored)
}

fn write_local_place(conn: &Connection, item: LocalItemId, parent: LocalItemId, name: Value, is_name_ignored: bool) -> Result<(), TreeError> {
    conn.prepare_cached("UPDATE items SET local_parent_id = ?2, local_name = ?3, is_name_ignored = ?4 WHERE local_item_id = ?1")?
        .execute(params![item.as_i64(), parent.as_i64(), name, is_name_ignored])?;
    Ok(())
}

/// The binding of `item` is `onedrive_item` from now on, whatever it was.
pub(super) fn set_binding(conn: &Connection, item: LocalItemId, onedrive_item: &OneDriveItemReference) -> Result<(), TreeError> {
    conn.prepare_cached("UPDATE items SET onedrive_drive_id = ?2, onedrive_item_id = ?3 WHERE local_item_id = ?1")?
        .execute(params![item.as_i64(), onedrive_item.drive_id.as_str(), onedrive_item.item_id.as_str()])?;
    Ok(())
}
