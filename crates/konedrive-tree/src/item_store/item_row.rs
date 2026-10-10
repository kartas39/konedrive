//! A row of `items` in the database: the one place it is read from SQLite, written to it
//! and removed from it. What is here writes a row as it is told, with no rule of the rows
//! asked but the one of names and the one of an item id of OneDrive, each asked wherever
//! such a thing is written: only
//! [`ItemStore`](super::ItemStore)'s operations and the operations on bindings call it, and
//! nothing outside `item_store` can.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rusqlite::types::Value;
use rusqlite::{params, Connection, OptionalExtension};

use super::refuse;
use crate::model::{ContentPresence, ContentVersion, DriveId, Item, ItemKind, ItemPlace, ItemVersion, LocalItemId, OneDriveItemId, OneDriveItemReference, Placement, PLACED};
use crate::outbox::{path_from, path_value};
use crate::FoldedName;
use crate::TreeError;

/// The columns of a content, after the word that says whose it is.
const CONTENT_COLUMNS: [&str; 6] = ["size", "mtime", "mtime_nsec", "hash", "ctag", "change_count"];

/// The columns of an [`Item`], in the order [`item_from`] reads them and [`values_of`] gives them.
const ITEM_COLUMNS: &str = "local_item_id, onedrive_drive_id, onedrive_item_id, kind, \
     local_parent_id, local_name, local_folded_name, local_size, local_mtime, local_mtime_nsec, local_hash, local_ctag, local_change_count, \
     onedrive_parent_id, onedrive_name, onedrive_folded_name, onedrive_size, onedrive_mtime, onedrive_mtime_nsec, onedrive_hash, onedrive_ctag, onedrive_change_count, \
     onedrive_etag, onedrive_mime, \
     synced_parent_id, synced_name, synced_size, synced_mtime, synced_mtime_nsec, synced_hash, synced_ctag, synced_change_count, \
     content_presence, \
     before_write_size, before_write_mtime, before_write_mtime_nsec, before_write_hash, before_write_ctag, before_write_change_count, \
     placement, is_name_ignored, confirmed_at_count, unsent_since";

/// The column of [`ITEM_COLUMNS`] that is read and never written here: the schema keeps it
/// with every write of a local version or of a synced place or content (the triggers
/// `items_unsent_since_*`).
const UNSENT_SINCE: &str = "unsent_since";

/// SQL for "the local place or content of `row` is not the synced one", `row` being the
/// empty text for the row a statement is about, or `NEW.` in a trigger: the two differ in
/// the place or in the content, or one is there and the other is not. The triggers that
/// keep `unsent_since` ask it in these words.
pub(crate) fn unsent(row: &str) -> String {
    let differs = |column: &str| format!("{row}local_{column} IS NOT {row}synced_{column}");
    let places = ["parent_id", "name"].map(differs).join(" OR ");
    let contents = CONTENT_COLUMNS.map(differs).join(" OR ");
    format!("({places} OR {contents})")
}

/// The columns [`unsent`] reads: a write of any of them is a write of a local version or
/// of a synced place or content.
pub(crate) fn unsent_columns() -> String {
    let of = |side: &str| std::iter::once("parent_id").chain(["name"]).chain(CONTENT_COLUMNS).map(|column| format!("{side}_{column}")).collect::<Vec<_>>();
    [of("local"), of("synced")].concat().join(", ")
}

/// SQL for "the row's OneDrive place or content is not the synced one": what OneDrive
/// changed, made or removed since the two sides last agreed, and what they never agreed
/// on. The index `items_changed_in_onedrive` holds these rows, and a query finds them by it
/// when it asks in these words.
pub(crate) const CHANGED_IN_ONEDRIVE: &str = "(onedrive_parent_id IS NOT synced_parent_id OR onedrive_name IS NOT synced_name \
     OR onedrive_size IS NOT synced_size OR onedrive_mtime IS NOT synced_mtime OR onedrive_mtime_nsec IS NOT synced_mtime_nsec \
     OR onedrive_hash IS NOT synced_hash OR onedrive_ctag IS NOT synced_ctag OR onedrive_change_count IS NOT synced_change_count)";

/// SQL for "the row is a binding and nothing else": no version on either side and nothing
/// agreed on, and not the root's. The index `items_binding_alone` holds these rows by their
/// stamp, and a query finds them by it when it asks in these words.
pub(crate) const BINDING_ALONE: &str = "onedrive_item_id IS NOT NULL AND local_parent_id IS NULL AND onedrive_parent_id IS NULL \
     AND synced_parent_id IS NULL AND synced_size IS NULL AND local_item_id != 1";

/// Where each column of [`ITEM_COLUMNS`] is; a content's is where its first column is.
mod column_index {
    use super::ITEM_COLUMNS;
    use crate::model::column;

    pub(super) const LOCAL_ITEM_ID: usize = column(ITEM_COLUMNS, "local_item_id");
    pub(super) const ONEDRIVE_DRIVE_ID: usize = column(ITEM_COLUMNS, "onedrive_drive_id");
    pub(super) const ONEDRIVE_ITEM_ID: usize = column(ITEM_COLUMNS, "onedrive_item_id");
    pub(super) const KIND: usize = column(ITEM_COLUMNS, "kind");
    pub(super) const LOCAL_PARENT_ID: usize = column(ITEM_COLUMNS, "local_parent_id");
    pub(super) const LOCAL_NAME: usize = column(ITEM_COLUMNS, "local_name");
    pub(super) const LOCAL_CONTENT: usize = column(ITEM_COLUMNS, "local_size");
    pub(super) const ONEDRIVE_PARENT_ID: usize = column(ITEM_COLUMNS, "onedrive_parent_id");
    pub(super) const ONEDRIVE_NAME: usize = column(ITEM_COLUMNS, "onedrive_name");
    pub(super) const ONEDRIVE_CONTENT: usize = column(ITEM_COLUMNS, "onedrive_size");
    pub(super) const ONEDRIVE_ETAG: usize = column(ITEM_COLUMNS, "onedrive_etag");
    pub(super) const ONEDRIVE_MIME: usize = column(ITEM_COLUMNS, "onedrive_mime");
    pub(super) const SYNCED_PARENT_ID: usize = column(ITEM_COLUMNS, "synced_parent_id");
    pub(super) const SYNCED_NAME: usize = column(ITEM_COLUMNS, "synced_name");
    pub(super) const SYNCED_CONTENT: usize = column(ITEM_COLUMNS, "synced_size");
    pub(super) const CONTENT_PRESENCE: usize = column(ITEM_COLUMNS, "content_presence");
    pub(super) const CONTENT_BEFORE_WRITE: usize = column(ITEM_COLUMNS, "before_write_size");
    pub(super) const PLACEMENT: usize = column(ITEM_COLUMNS, "placement");
    pub(super) const IS_NAME_IGNORED: usize = column(ITEM_COLUMNS, "is_name_ignored");
    pub(super) const CONFIRMED_AT_COUNT: usize = column(ITEM_COLUMNS, "confirmed_at_count");
    pub(super) const UNSENT_SINCE: usize = column(ITEM_COLUMNS, "unsent_since");
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

/// The one check of every binding a row is written with: an item id of OneDrive is not the
/// empty text. A drive id never is ([`DriveId`]).
fn check_onedrive_item_id(onedrive_item: Option<&OneDriveItemReference>) -> Result<(), TreeError> {
    match onedrive_item {
        Some(reference) if reference.item_id.as_str().is_empty() => Err(refuse("the empty text is no item id of OneDrive".to_owned())),
        _ => Ok(()),
    }
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
    let (drive_id, item_id): (Option<String>, Option<String>) = (row.get(column_index::ONEDRIVE_DRIVE_ID)?, row.get(column_index::ONEDRIVE_ITEM_ID)?);
    let onedrive_item = match (drive_id.and_then(DriveId::new), item_id) {
        (Some(drive_id), Some(item_id)) => Some(OneDriveItemReference { drive_id, item_id: OneDriveItemId::new(item_id) }),
        _ => None,
    };
    let local_item_id = LocalItemId::new(row.get(column_index::LOCAL_ITEM_ID)?);
    // The folder shows what has a local place, and the root, which has none.
    let local = match place_from(row, column_index::LOCAL_PARENT_ID, column_index::LOCAL_NAME)? {
        Some(place) => Some(ItemVersion { place: Some(place), content: content_from(row, column_index::LOCAL_CONTENT)? }),
        None if local_item_id == LocalItemId::ROOT => Some(ItemVersion { place: None, content: None }),
        None => None,
    };
    // OneDrive has what has a place there, and the root, which has none, once it is bound.
    let onedrive = match place_from(row, column_index::ONEDRIVE_PARENT_ID, column_index::ONEDRIVE_NAME)? {
        Some(place) => Some(ItemVersion { place: Some(place), content: content_from(row, column_index::ONEDRIVE_CONTENT)? }),
        None if local_item_id == LocalItemId::ROOT && onedrive_item.is_some() => Some(ItemVersion { place: None, content: None }),
        None => None,
    };
    Ok(Item {
        local_item_id,
        onedrive_item,
        kind: ItemKind::decode(&row.get::<_, String>(column_index::KIND)?),
        local,
        onedrive,
        onedrive_etag: row.get(column_index::ONEDRIVE_ETAG)?,
        onedrive_mime: row.get(column_index::ONEDRIVE_MIME)?,
        synced_place: place_from(row, column_index::SYNCED_PARENT_ID, column_index::SYNCED_NAME)?,
        synced_content: content_from(row, column_index::SYNCED_CONTENT)?,
        content_presence: ContentPresence::decode(&row.get::<_, String>(column_index::CONTENT_PRESENCE)?),
        content_before_write: content_from(row, column_index::CONTENT_BEFORE_WRITE)?,
        placement: Placement::decode(&row.get::<_, String>(column_index::PLACEMENT)?),
        is_name_ignored: row.get::<_, i64>(column_index::IS_NAME_IGNORED)? != 0,
        confirmed_at_count: row.get::<_, i64>(column_index::CONFIRMED_AT_COUNT)?.max(0) as u64,
        unsent_since: row.get::<_, Option<i64>>(column_index::UNSENT_SINCE)?.map(|stamp| stamp.max(0) as u64),
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

/// The values of a local or an OneDrive place: the parent, the name, and the name folded as
/// OneDrive compares names, which is written wherever the name is.
fn folded_place_values(place: Option<&ItemPlace>) -> [Value; 3] {
    let [parent, name] = place_values(place);
    [parent, name, place.map_or(Value::Null, |place| name_value(FoldedName::of(&place.name).as_os_str()))]
}

/// The values of the `onedrive_` columns, in their order: the place, the content, the eTag,
/// the MIME type.
fn onedrive_values(onedrive: Option<&ItemVersion>, etag: Option<&str>, mime: Option<&str>) -> Vec<Value> {
    let mut values: Vec<Value> = folded_place_values(onedrive.and_then(|onedrive| onedrive.place.as_ref())).into();
    values.extend(content_values(onedrive.and_then(|onedrive| onedrive.content.as_ref())));
    values.push(etag.map(str::to_owned).into());
    values.push(mime.map(str::to_owned).into());
    values
}

/// The value of every column [`written_columns`] names, in their order.
fn values_of(item: &Item) -> Vec<Value> {
    let binding = item.onedrive_item.as_ref();
    let local = item.local.as_ref();
    let mut values: Vec<Value> = vec![binding.map(|b| b.drive_id.as_str().to_owned()).into(), binding.map(|b| b.item_id.as_str().to_owned()).into()];
    values.push(item.kind.as_str().to_owned().into());
    values.extend(folded_place_values(local.and_then(|local| local.place.as_ref())));
    values.extend(content_values(local.and_then(|local| local.content.as_ref())));
    values.extend(onedrive_values(item.onedrive.as_ref(), item.onedrive_etag.as_deref(), item.onedrive_mime.as_deref()));
    values.extend(place_values(item.synced_place.as_ref()));
    values.extend(content_values(item.synced_content.as_ref()));
    values.push(item.content_presence.as_str().to_owned().into());
    values.extend(content_values(item.content_before_write.as_ref()));
    values.push(item.placement.encode().into());
    values.push(i64::from(item.is_name_ignored).into());
    values.push((item.confirmed_at_count as i64).into());
    values
}

/// The names of [`ITEM_COLUMNS`] but the first, the key, and [`UNSENT_SINCE`], which the
/// schema keeps.
fn written_columns() -> impl Iterator<Item = &'static str> {
    ITEM_COLUMNS.split(',').map(str::trim).skip(1).filter(|column| *column != UNSENT_SINCE)
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
    check_onedrive_item_id(onedrive_item)?;
    let mut values: Vec<Value> = vec![onedrive_item.map(|b| b.drive_id.as_str().to_owned()).into(), onedrive_item.map(|b| b.item_id.as_str().to_owned()).into(), kind.as_str().to_owned().into()];
    values.extend(folded_place_values(local.and_then(|local| local.place.as_ref())));
    values.extend(content_values(local.and_then(|local| local.content.as_ref())));
    values.push(content_presence.as_str().to_owned().into());
    values.push(i64::from(is_name_ignored).into());
    let content = CONTENT_COLUMNS.map(|column| format!("local_{column}")).join(", ");
    let places = (1..=values.len()).map(|n| format!("?{n}")).collect::<Vec<_>>().join(", ");
    let sql = format!("INSERT INTO items (onedrive_drive_id, onedrive_item_id, kind, local_parent_id, local_name, local_folded_name, {content}, content_presence, is_name_ignored) VALUES ({places})");
    conn.prepare_cached(&sql)?.execute(rusqlite::params_from_iter(values))?;
    Ok(LocalItemId::new(conn.last_insert_rowid()))
}

/// The row of `item.local_item_id` is `item` from now on.
pub(super) fn write_item(conn: &Connection, item: &Item) -> Result<(), TreeError> {
    check_names(item.local.as_ref(), item.synced_place.as_ref())?;
    check_onedrive_item_id(item.onedrive_item.as_ref())?;
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

pub(super) fn list_children_in_onedrive(conn: &Connection, directory: LocalItemId) -> Result<Vec<Item>, TreeError> {
    let sql = format!("SELECT {ITEM_COLUMNS} FROM items INDEXED BY items_onedrive_place WHERE onedrive_parent_id = ?1 ORDER BY onedrive_name");
    let mut statement = conn.prepare_cached(&sql)?;
    let children = statement.query_map([directory.as_i64()], item_from)?.collect::<Result<Vec<_>, _>>()?;
    Ok(children)
}

/// The rows of one folded name in `directory` on the side `side` (`local` or `onedrive`),
/// by local item id: found by that side's index of folded names.
fn list_children_of_side_with_folded_name(conn: &Connection, side: &str, directory: LocalItemId, name: &FoldedName) -> Result<Vec<Item>, TreeError> {
    let sql = format!("SELECT {ITEM_COLUMNS} FROM items INDEXED BY items_{side}_folded_place WHERE {side}_parent_id = ?1 AND {side}_folded_name = ?2 ORDER BY local_item_id");
    let mut statement = conn.prepare_cached(&sql)?;
    let children = statement.query_map(params![directory.as_i64(), name_value(name.as_os_str())], item_from)?.collect::<Result<Vec<_>, _>>()?;
    Ok(children)
}

pub(super) fn list_children_with_folded_name(conn: &Connection, directory: LocalItemId, name: &FoldedName) -> Result<Vec<Item>, TreeError> {
    list_children_of_side_with_folded_name(conn, "local", directory, name)
}

pub(super) fn list_children_in_onedrive_with_folded_name(conn: &Connection, directory: LocalItemId, name: &FoldedName) -> Result<Vec<Item>, TreeError> {
    list_children_of_side_with_folded_name(conn, "onedrive", directory, name)
}

/// `item` is at `place` in the folder from now on, and `is_name_ignored` is what the upload
/// rules say of its name there: the two are written together, since the one is said of the
/// other. The index of local places refuses a place another item has.
pub(super) fn set_local_place(conn: &Connection, item: LocalItemId, place: &ItemPlace, is_name_ignored: bool) -> Result<(), TreeError> {
    check_name(&place.name)?;
    write_local_place(conn, item, place.parent_local_item_id, name_value(&place.name), name_value(FoldedName::of(&place.name).as_os_str()), is_name_ignored)
}

/// `item` steps aside in the directory it is in, to a name no file can have (it holds a NUL
/// and a `/`, which [`check_name`] lets no stored name hold) and no other item has (it
/// holds the item's id), so that another item can take its place before it takes its next
/// one. The name must not outlive the transaction. It is no text, and folds to itself.
pub(super) fn set_local_place_aside(conn: &Connection, item: LocalItemId, place: &ItemPlace, is_name_ignored: bool) -> Result<(), TreeError> {
    let aside = Value::Blob(format!("\0/{item}").into_bytes());
    write_local_place(conn, item, place.parent_local_item_id, aside.clone(), aside, is_name_ignored)
}

fn write_local_place(conn: &Connection, item: LocalItemId, parent: LocalItemId, name: Value, folded_name: Value, is_name_ignored: bool) -> Result<(), TreeError> {
    conn.prepare_cached("UPDATE items SET local_parent_id = ?2, local_name = ?3, local_folded_name = ?4, is_name_ignored = ?5 WHERE local_item_id = ?1")?
        .execute(params![item.as_i64(), parent.as_i64(), name, folded_name, is_name_ignored])?;
    Ok(())
}

/// The binding of `item` is `onedrive_item` from now on, whatever it was.
pub(super) fn set_binding(conn: &Connection, item: LocalItemId, onedrive_item: &OneDriveItemReference) -> Result<(), TreeError> {
    check_onedrive_item_id(Some(onedrive_item))?;
    conn.prepare_cached("UPDATE items SET onedrive_drive_id = ?2, onedrive_item_id = ?3 WHERE local_item_id = ?1")?
        .execute(params![item.as_i64(), onedrive_item.drive_id.as_str(), onedrive_item.item_id.as_str()])?;
    Ok(())
}

/// The names of the `onedrive_` columns, in the order [`onedrive_values`] gives them.
fn onedrive_columns() -> Vec<String> {
    let mut columns = vec!["onedrive_parent_id".to_owned(), "onedrive_name".to_owned(), "onedrive_folded_name".to_owned()];
    columns.extend(CONTENT_COLUMNS.map(|column| format!("onedrive_{column}")));
    columns.push("onedrive_etag".to_owned());
    columns.push("onedrive_mime".to_owned());
    columns
}

/// What OneDrive has of `item` is `onedrive` from now on, with this eTag, this MIME type of
/// its content and this placement, and nothing else of the row is written.
/// `seen_in_full_listing`: the count of the full listing that says so, when one does; the
/// row keeps what it has otherwise. A name is kept as OneDrive gave it and is not asked to
/// be the name of a directory entry: whether the folder can show the item is its placement.
pub(super) fn set_onedrive_version(conn: &Connection, item: LocalItemId, onedrive: &ItemVersion, etag: Option<&str>, mime: Option<&str>, placement: Placement, seen_in_full_listing: Option<u64>) -> Result<(), TreeError> {
    let mut values = onedrive_values(Some(onedrive), etag, mime);
    let mut sets: Vec<String> = onedrive_columns().iter().enumerate().map(|(n, column)| format!("{column} = ?{}", n + 1)).collect();
    values.push(placement.encode().into());
    sets.push(format!("placement = ?{}", values.len()));
    values.push(seen_in_full_listing.map(|count| count as i64).into());
    sets.push(format!("seen_in_full_listing = COALESCE(?{}, seen_in_full_listing)", values.len()));
    values.push(item.as_i64().into());
    let sql = format!("UPDATE items SET {} WHERE local_item_id = ?{}", sets.join(", "), values.len());
    conn.prepare_cached(&sql)?.execute(rusqlite::params_from_iter(values))?;
    Ok(())
}

/// OneDrive does not have `item` from now on: no version, no eTag, no MIME type, and the
/// placement of an item it does not have. Nothing else of the row is written.
pub(super) fn clear_onedrive_version(conn: &Connection, item: LocalItemId) -> Result<(), TreeError> {
    conn.prepare_cached(&format!("{} WHERE local_item_id = ?1", clear_onedrive_version_sql("")))?.execute([item.as_i64()])?;
    Ok(())
}

/// The eTag OneDrive gave for `item` is `etag` from now on: the root's, which has no other
/// version there.
pub(super) fn set_onedrive_etag(conn: &Connection, item: LocalItemId, etag: Option<&str>) -> Result<(), TreeError> {
    conn.prepare_cached("UPDATE items SET onedrive_etag = ?2 WHERE local_item_id = ?1")?.execute(params![item.as_i64(), etag])?;
    Ok(())
}

/// The count of the full listing that last listed `item`; 0 when none did, and for an id
/// with no row.
pub(super) fn find_seen_in_full_listing(conn: &Connection, item: LocalItemId) -> Result<u64, TreeError> {
    let count: Option<i64> = conn.prepare_cached("SELECT seen_in_full_listing FROM items WHERE local_item_id = ?1")?.query_row([item.as_i64()], |row| row.get(0)).optional()?;
    Ok(count.unwrap_or(0).max(0) as u64)
}

/// The full listing of this count listed `item`.
pub(super) fn set_seen_in_full_listing(conn: &Connection, item: LocalItemId, count: u64) -> Result<(), TreeError> {
    conn.prepare_cached("UPDATE items SET seen_in_full_listing = ?2 WHERE local_item_id = ?1")?.execute(params![item.as_i64(), count as i64])?;
    Ok(())
}

/// The items whose OneDrive place or content is not the synced one, by local item id: at
/// most `limit` of them, after the item `after` when one is given. Found by the index of
/// those rows, whatever the store holds beside them.
pub(super) fn list_changed_in_onedrive(conn: &Connection, after: Option<LocalItemId>, limit: usize) -> Result<Vec<Item>, TreeError> {
    let sql = format!("SELECT {ITEM_COLUMNS} FROM items INDEXED BY items_changed_in_onedrive WHERE {CHANGED_IN_ONEDRIVE} AND local_item_id > ?1 ORDER BY local_item_id LIMIT ?2");
    let mut statement = conn.prepare_cached(&sql)?;
    let items = statement.query_map(params![after.map_or(0, LocalItemId::as_i64), limit as i64], item_from)?.collect::<Result<Vec<_>, _>>()?;
    Ok(items)
}

/// The unsent items in the order in which each came to be unsent: at most `limit` of
/// them, after the stamp `after` when one is given. Found by the index of their stamps.
pub(super) fn list_unsent(conn: &Connection, after: Option<u64>, limit: usize) -> Result<Vec<Item>, TreeError> {
    let sql = format!("SELECT {ITEM_COLUMNS} FROM items INDEXED BY items_unsent WHERE unsent_since IS NOT NULL AND unsent_since > ?1 ORDER BY unsent_since LIMIT ?2");
    let mut statement = conn.prepare_cached(&sql)?;
    let items = statement.query_map(params![after.map_or(0, |stamp| stamp as i64), limit as i64], item_from)?.collect::<Result<Vec<_>, _>>()?;
    Ok(items)
}

/// Every item OneDrive has in `directory` and below it, as the rows say ([`BELOW_IN_ONEDRIVE`]).
pub(super) fn list_below_in_onedrive(conn: &Connection, directory: LocalItemId) -> Result<Vec<Item>, TreeError> {
    let sql = format!("SELECT {ITEM_COLUMNS} FROM items WHERE local_item_id IN ({BELOW_IN_ONEDRIVE})");
    let mut statement = conn.prepare_cached(&sql)?;
    let items = statement.query_map([directory.as_i64()], item_from)?.collect::<Result<Vec<_>, _>>()?;
    Ok(items)
}

/// SQL that takes the OneDrive version from the rows it is run for: no version, no eTag,
/// no MIME type, and the placement of an item OneDrive does not have. Nothing else of a row
/// is written. `found_by`: how the statement finds its rows, the empty text or an
/// `INDEXED BY`, so that a statement an index cannot serve fails and reads no table whole.
fn clear_onedrive_version_sql(found_by: &str) -> String {
    let sets = onedrive_columns().iter().map(|column| format!("{column} = NULL")).collect::<Vec<_>>().join(", ");
    format!("UPDATE items {found_by} SET {sets}, placement = '{PLACED}'")
}

/// The item ids of OneDrive a query answers, one a row.
fn list_onedrive_item_ids(conn: &Connection, sql: &str, values: [i64; 2]) -> Result<Vec<OneDriveItemId>, TreeError> {
    let mut statement = conn.prepare_cached(sql)?;
    let ids = statement.query_map(values, |row| row.get::<_, String>(0).map(OneDriveItemId::new))?.collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// SQL for "the local item ids of what the rows say OneDrive has in the directory `?1` and
/// below it": found from the directory down by the index of places in OneDrive, each item
/// once also when the rows of a listing that has not ended name each other in a circle.
const BELOW_IN_ONEDRIVE: &str = "WITH RECURSIVE below(id) AS (
         SELECT local_item_id FROM items INDEXED BY items_onedrive_place WHERE onedrive_parent_id = ?1
         UNION
         SELECT child.local_item_id FROM below JOIN items child INDEXED BY items_onedrive_place ON child.onedrive_parent_id = below.id)
     SELECT id FROM below";

/// What the rows say OneDrive has in `directory` and below it has no OneDrive version from
/// now on, but the rows stamped after the count `stamped_until`: those are left, and their
/// item ids of OneDrive answered. Two statements whatever is below: the rows that are left
/// are read, and then the others are written, each found from what the rows said before
/// either.
pub(super) fn clear_onedrive_versions_below(conn: &Connection, directory: LocalItemId, stamped_until: u64) -> Result<Vec<OneDriveItemId>, TreeError> {
    let values = [directory.as_i64(), stamped_until as i64];
    let left = list_onedrive_item_ids(conn, &format!("SELECT onedrive_item_id FROM items WHERE local_item_id IN ({BELOW_IN_ONEDRIVE}) AND confirmed_at_count > ?2"), values)?;
    conn.prepare_cached(&format!("{} WHERE local_item_id IN ({BELOW_IN_ONEDRIVE}) AND confirmed_at_count <= ?2", clear_onedrive_version_sql("")))?.execute(values)?;
    Ok(left)
}

/// What OneDrive has, by the rows, that the full listing of the count `full_listing_count`
/// did not list has no OneDrive version from now on, but the rows stamped after the count
/// `stamped_until`: those are left, and their item ids of OneDrive answered. Two statements
/// whatever was not seen, by the index of the listing that saw a row.
pub(super) fn clear_onedrive_versions_unseen(conn: &Connection, full_listing_count: u64, stamped_until: u64) -> Result<Vec<OneDriveItemId>, TreeError> {
    const UNSEEN: &str = "onedrive_parent_id IS NOT NULL AND seen_in_full_listing < ?1";
    let values = [full_listing_count as i64, stamped_until as i64];
    let left = list_onedrive_item_ids(conn, &format!("SELECT onedrive_item_id FROM items INDEXED BY items_seen_in_full_listing WHERE {UNSEEN} AND confirmed_at_count > ?2"), values)?;
    conn.prepare_cached(&format!("{} WHERE {UNSEEN} AND confirmed_at_count <= ?2", clear_onedrive_version_sql("INDEXED BY items_seen_in_full_listing")))?.execute(values)?;
    Ok(left)
}

/// The rows that are a binding and nothing else, that no row names as its parent on any
/// side, and whose stamp is `stamped_until` at most, go. Found by the index of such rows.
/// A row that goes names no parent, so none of them holds another.
pub(super) fn delete_rows_of_binding_alone(conn: &Connection, stamped_until: u64) -> Result<(), TreeError> {
    let sql = format!(
        "DELETE FROM items INDEXED BY items_binding_alone WHERE {BINDING_ALONE} AND confirmed_at_count <= ?1
            AND NOT EXISTS (SELECT 1 FROM items child WHERE child.local_parent_id = items.local_item_id)
            AND NOT EXISTS (SELECT 1 FROM items child WHERE child.onedrive_parent_id = items.local_item_id)
            AND NOT EXISTS (SELECT 1 FROM items child WHERE child.synced_parent_id = items.local_item_id)"
    );
    conn.prepare_cached(&sql)?.execute([stamped_until as i64])?;
    Ok(())
}
