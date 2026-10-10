//! The rows of `items`: one per item, keyed by its local item id, which is also the kernel's
//! node id of what the folder shows. Everything in a row is written through [`ItemStore`],
//! each change one transaction.
//!
//! **What holds of the rows at all times**, and who holds it:
//!
//! - *An item's parent exists in the folder and is a directory.* The code: an item is made
//!   or moved only into a directory the folder shows ([`check_directory`]), and a directory
//!   leaves the folder only when the folder shows nothing in it ([`check_empty`]).
//! - *No item is above itself.* The schema refuses a row that is its own parent; the code
//!   refuses a move into what is below the item ([`check_not_below`]).
//! - *In the folder, one item per name in a directory.* The schema: the unique index
//!   `items_local_place`.
//! - *A binding belongs to one item.* The schema: the unique index `items_onedrive_item`.
//!   The binding is changed by the three operations of [`bindings`] and by nothing else,
//!   but for the root's, which is written where the store learns the drive's root
//!   ([`bind_root`]).
//! - *The root is item 1, a directory with no place, and stays.* The schema's checks; the
//!   code refuses every change of it.
//! - *An id is never given twice.* The schema: `AUTOINCREMENT`.
//!
//! The OneDrive version is not in the rows yet (`onedrive_items` holds it), so the rules of
//! that side are not held here. A synced place names its directory by local item id; the
//! operations on bindings keep it pointing at a row.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use rusqlite::types::Value;
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::meta;
use crate::model::{
    column, ContentPresence, ContentVersion, DriveId, Item, ItemKind, ItemPlace, ItemVersion, LocalItemId, LocalVersionChange, OneDriveItemId,
    OneDriveItemReference,
};
use crate::outbox::{path_from, path_value};
use crate::{TreeError, TreeStore, MAX_CHAIN};

/// The three operations that move a binding from one item to another.
mod bindings;

/// The columns of a content, after the word that says whose it is.
const CONTENT_COLUMNS: [&str; 6] = ["size", "mtime", "mtime_nsec", "hash", "ctag", "change_count"];

/// The columns of an [`Item`], in the order [`item_from`] reads them and [`values_of`] gives them.
const ITEM_COLUMNS: &str = "local_item_id, onedrive_drive_id, onedrive_item_id, \
     local_parent_id, local_name, local_kind, local_size, local_mtime, local_mtime_nsec, local_hash, local_ctag, local_change_count, \
     synced_parent_id, synced_name, synced_size, synced_mtime, synced_mtime_nsec, synced_hash, synced_ctag, synced_change_count, \
     content_presence, \
     before_write_size, before_write_mtime, before_write_mtime_nsec, before_write_hash, before_write_ctag, before_write_change_count, \
     is_ignored";

/// Where each column of [`ITEM_COLUMNS`] is; a content's is where its first column is.
mod at {
    use super::{column, ITEM_COLUMNS};

    pub(super) const LOCAL_ITEM_ID: usize = column(ITEM_COLUMNS, "local_item_id");
    pub(super) const ONEDRIVE_DRIVE_ID: usize = column(ITEM_COLUMNS, "onedrive_drive_id");
    pub(super) const ONEDRIVE_ITEM_ID: usize = column(ITEM_COLUMNS, "onedrive_item_id");
    pub(super) const LOCAL_PARENT_ID: usize = column(ITEM_COLUMNS, "local_parent_id");
    pub(super) const LOCAL_NAME: usize = column(ITEM_COLUMNS, "local_name");
    pub(super) const LOCAL_KIND: usize = column(ITEM_COLUMNS, "local_kind");
    pub(super) const LOCAL_CONTENT: usize = column(ITEM_COLUMNS, "local_size");
    pub(super) const SYNCED_PARENT_ID: usize = column(ITEM_COLUMNS, "synced_parent_id");
    pub(super) const SYNCED_NAME: usize = column(ITEM_COLUMNS, "synced_name");
    pub(super) const SYNCED_CONTENT: usize = column(ITEM_COLUMNS, "synced_size");
    pub(super) const CONTENT_PRESENCE: usize = column(ITEM_COLUMNS, "content_presence");
    pub(super) const CONTENT_BEFORE_WRITE: usize = column(ITEM_COLUMNS, "before_write_size");
    pub(super) const IS_IGNORED: usize = column(ITEM_COLUMNS, "is_ignored");
}

/// The store's rows of `items`, for one job of the store's thread: what finds an item and
/// what changes one. Borrowed from the store as [`ReadStore`](crate::ReadStore) is
/// ([`TreeStore::item_store`]).
pub struct ItemStore<'a> {
    store: &'a mut TreeStore,
}

impl TreeStore {
    /// The rows of `items`.
    pub fn item_store(&mut self) -> ItemStore<'_> {
        ItemStore { store: self }
    }
}

impl ItemStore<'_> {
    /// The item with this local item id; `None` when its row is gone.
    pub fn find_item(&self, item: LocalItemId) -> Result<Option<Item>, TreeError> {
        find_item(&self.store.conn, item)
    }

    /// The item the folder shows in `directory` under `name`, compared as it was given.
    pub fn find_item_at(&self, directory: LocalItemId, name: &OsStr) -> Result<Option<Item>, TreeError> {
        find_item_at(&self.store.conn, directory, name)
    }

    /// The item `onedrive_item` belongs to.
    pub fn find_bound_item(&self, onedrive_item: &OneDriveItemReference) -> Result<Option<Item>, TreeError> {
        find_bound_item(&self.store.conn, onedrive_item)
    }

    /// What the folder shows in `directory`, by name.
    pub fn list_children(&self, directory: LocalItemId) -> Result<Vec<Item>, TreeError> {
        list_children(&self.store.conn, directory)
    }

    /// Where the folder shows `item`, relative to the root: the names of its local places
    /// from the root down, and the empty path for the root. `None` for an item the folder
    /// does not show, and for an id with no row.
    pub fn find_path(&self, item: LocalItemId) -> Result<Option<PathBuf>, TreeError> {
        find_path(&self.store.conn, item)
    }

    /// Writes one change of an item's local version, in one transaction: all of it or
    /// nothing. The item it is about: the one made, moved, or saved over another. A change
    /// that would break a rule of the rows is refused and changes nothing.
    pub fn apply(&mut self, change: &LocalVersionChange) -> Result<LocalItemId, TreeError> {
        self.write(|tx| apply_change(tx, change))
    }

    /// The two items have each the other's local place, in one transaction: two names
    /// exchanged, as `renameat2` does with `RENAME_EXCHANGE`. `first_is_ignored` and
    /// `second_is_ignored` are what the upload rules say of each at its new place.
    pub fn exchange_local_places(&mut self, first: LocalItemId, second: LocalItemId, first_is_ignored: bool, second_is_ignored: bool) -> Result<(), TreeError> {
        self.write(|tx| exchange_local_places(tx, first, second, first_is_ignored, second_is_ignored))
    }

    /// Runs `work` in a transaction of its own and commits what it did; an error leaves
    /// the rows as they were.
    fn write<T>(&mut self, work: impl FnOnce(&Transaction<'_>) -> Result<T, TreeError>) -> Result<T, TreeError> {
        let tx = self.store.conn.transaction()?;
        let done = work(&tx)?;
        tx.commit()?;
        Ok(done)
    }
}

/// The error of a change the rows' rules refuse: nothing was written.
fn refuse(what: String) -> TreeError {
    TreeError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("the items: {what}")))
}

/// A name as `items` keeps it: text when it is UTF-8 and its bytes otherwise, as the outbox
/// keeps a path, so that one name always has one form and is found by equality.
fn name_value(name: &OsStr) -> Value {
    path_value(Path::new(name))
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
    let local = match row.get::<_, Option<String>>(at::LOCAL_KIND)? {
        Some(kind) => Some(ItemVersion {
            place: place_from(row, at::LOCAL_PARENT_ID, at::LOCAL_NAME)?,
            kind: ItemKind::decode(&kind),
            content: content_from(row, at::LOCAL_CONTENT)?,
        }),
        None => None,
    };
    Ok(Item {
        local_item_id: LocalItemId::new(row.get(at::LOCAL_ITEM_ID)?),
        onedrive_item,
        local,
        synced_place: place_from(row, at::SYNCED_PARENT_ID, at::SYNCED_NAME)?,
        synced_content: content_from(row, at::SYNCED_CONTENT)?,
        content_presence: ContentPresence::decode(&row.get::<_, String>(at::CONTENT_PRESENCE)?),
        content_before_write: content_from(row, at::CONTENT_BEFORE_WRITE)?,
        is_ignored: row.get::<_, i64>(at::IS_IGNORED)? != 0,
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
    values.extend(place_values(local.and_then(|local| local.place.as_ref())));
    values.push(local.map(|local| local.kind.as_str().to_owned()).into());
    values.extend(content_values(local.and_then(|local| local.content.as_ref())));
    values.extend(place_values(item.synced_place.as_ref()));
    values.extend(content_values(item.synced_content.as_ref()));
    values.push(item.content_presence.as_str().to_owned().into());
    values.extend(content_values(item.content_before_write.as_ref()));
    values.push(i64::from(item.is_ignored).into());
    values
}

/// The names of [`ITEM_COLUMNS`] but the first, the key.
fn written_columns() -> impl Iterator<Item = &'static str> {
    ITEM_COLUMNS.split(',').map(str::trim).skip(1)
}

/// `item` as a new row, whatever id it names; the id the row was given.
pub(crate) fn insert_item(conn: &Connection, item: &Item) -> Result<LocalItemId, TreeError> {
    let columns: Vec<&str> = written_columns().collect();
    let places = (1..=columns.len()).map(|n| format!("?{n}")).collect::<Vec<_>>().join(", ");
    let sql = format!("INSERT INTO items ({}) VALUES ({places})", columns.join(", "));
    conn.prepare_cached(&sql)?.execute(rusqlite::params_from_iter(values_of(item)))?;
    Ok(LocalItemId::new(conn.last_insert_rowid()))
}

/// The row of `item.local_item_id` is `item` from now on.
pub(crate) fn write_item(conn: &Connection, item: &Item) -> Result<(), TreeError> {
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
pub(crate) fn delete_item(conn: &Connection, item: LocalItemId) -> Result<(), TreeError> {
    conn.prepare_cached("DELETE FROM items WHERE local_item_id = ?1")?.execute([item.as_i64()])?;
    Ok(())
}

pub(crate) fn find_item(conn: &Connection, item: LocalItemId) -> Result<Option<Item>, TreeError> {
    let sql = format!("SELECT {ITEM_COLUMNS} FROM items WHERE local_item_id = ?1");
    Ok(conn.prepare_cached(&sql)?.query_row([item.as_i64()], item_from).optional()?)
}

pub(crate) fn find_item_at(conn: &Connection, directory: LocalItemId, name: &OsStr) -> Result<Option<Item>, TreeError> {
    let sql = format!("SELECT {ITEM_COLUMNS} FROM items WHERE local_parent_id = ?1 AND local_name = ?2");
    Ok(conn.prepare_cached(&sql)?.query_row(params![directory.as_i64(), name_value(name)], item_from).optional()?)
}

pub(crate) fn find_bound_item(conn: &Connection, onedrive_item: &OneDriveItemReference) -> Result<Option<Item>, TreeError> {
    let sql = format!("SELECT {ITEM_COLUMNS} FROM items WHERE onedrive_drive_id = ?1 AND onedrive_item_id = ?2");
    Ok(conn.prepare_cached(&sql)?.query_row([onedrive_item.drive_id.as_str(), onedrive_item.item_id.as_str()], item_from).optional()?)
}

pub(crate) fn list_children(conn: &Connection, directory: LocalItemId) -> Result<Vec<Item>, TreeError> {
    let sql = format!("SELECT {ITEM_COLUMNS} FROM items WHERE local_parent_id = ?1 ORDER BY local_name");
    let mut statement = conn.prepare_cached(&sql)?;
    let children = statement.query_map([directory.as_i64()], item_from)?.collect::<Result<Vec<_>, _>>()?;
    Ok(children)
}

pub(crate) fn find_path(conn: &Connection, item: LocalItemId) -> Result<Option<PathBuf>, TreeError> {
    let mut names = Vec::new();
    let mut at = item;
    // One row for each directory above the item.
    for _ in 0..=MAX_CHAIN {
        let Some(local) = find_item(conn, at)?.and_then(|row| row.local) else { return Ok(None) };
        match local.place {
            Some(place) => {
                names.push(place.name);
                at = place.parent_local_item_id;
            }
            None => return Ok(Some(names.iter().rev().collect())),
        }
    }
    Ok(None)
}

/// The item `id` as the folder shows it, with its local version; refused when it has no
/// row, or the folder does not show it.
fn shown(conn: &Connection, id: LocalItemId) -> Result<(Item, ItemVersion), TreeError> {
    let item = find_item(conn, id)?.ok_or_else(|| refuse(format!("item {id} has no row")))?;
    let local = item.local.clone().ok_or_else(|| refuse(format!("the folder does not show item {id}")))?;
    Ok((item, local))
}

/// [`shown`] for an item that is not the root, with its local place.
fn shown_below_root(conn: &Connection, id: LocalItemId) -> Result<(Item, ItemVersion, ItemPlace), TreeError> {
    let (item, local) = shown(conn, id)?;
    let place = local.place.clone().ok_or_else(|| refuse("the root of the folder has no place and is not changed".to_owned()))?;
    Ok((item, local, place))
}

/// [`shown`] for a file, with its local content.
fn shown_file(conn: &Connection, id: LocalItemId) -> Result<(Item, ItemVersion, ContentVersion), TreeError> {
    let (item, local) = shown(conn, id)?;
    match local.content.clone() {
        Some(content) if local.kind == ItemKind::File => Ok((item, local, content)),
        _ => Err(refuse(format!("item {id} is no file: it has no content to change"))),
    }
}

/// The rule "an item's parent exists and is a directory": refused unless `id` is a
/// directory the folder shows.
fn check_directory(conn: &Connection, id: LocalItemId) -> Result<(), TreeError> {
    match find_item(conn, id)?.and_then(|item| item.local) {
        Some(local) if local.kind == ItemKind::Directory => Ok(()),
        Some(_) => Err(refuse(format!("item {id} is no directory: nothing is placed in it"))),
        None => Err(refuse(format!("the folder shows no directory {id} to place something in"))),
    }
}

/// The same rule from the other end: refused while the folder shows something in `id`.
pub(crate) fn check_empty(conn: &Connection, id: LocalItemId) -> Result<(), TreeError> {
    let holds = conn.prepare_cached("SELECT 1 FROM items WHERE local_parent_id = ?1 LIMIT 1")?.exists([id.as_i64()])?;
    if holds {
        return Err(refuse(format!("the folder shows something in item {id}: it goes after what is in it")));
    }
    Ok(())
}

/// The rule "no item is above itself": refused when `directory` is `item` or below it, so
/// that `item` placed in it would be above itself.
fn check_not_below(conn: &Connection, directory: LocalItemId, item: LocalItemId) -> Result<(), TreeError> {
    let mut parent_of = conn.prepare_cached("SELECT local_parent_id FROM items WHERE local_item_id = ?1")?;
    let mut at = Some(directory);
    for _ in 0..=MAX_CHAIN {
        let Some(here) = at else { return Ok(()) };
        if here == item {
            return Err(refuse(format!("item {item} would be placed in itself or below itself")));
        }
        at = parent_of.query_row([here.as_i64()], |row| row.get::<_, Option<i64>>(0)).optional()?.flatten().map(LocalItemId::new);
    }
    Err(refuse(format!("the directories above item {directory} do not end at the root")))
}

/// `item`'s local place is `place` from now on. The index of local places refuses a place
/// another item has.
fn set_local_place(conn: &Connection, item: LocalItemId, parent: LocalItemId, name: Value) -> Result<(), TreeError> {
    conn.prepare_cached("UPDATE items SET local_parent_id = ?2, local_name = ?3 WHERE local_item_id = ?1")?.execute(params![item.as_i64(), parent.as_i64(), name])?;
    Ok(())
}

/// What the upload rules say of `item` is `is_ignored` from now on, for it and for all the
/// folder shows below it.
fn set_ignored(conn: &Connection, item: LocalItemId, is_ignored: bool) -> Result<(), TreeError> {
    conn.prepare_cached(&format!(
        "WITH RECURSIVE below(id, depth) AS (
             SELECT ?1, 0
             UNION ALL
             SELECT c.local_item_id, b.depth + 1 FROM items c JOIN below b ON c.local_parent_id = b.id WHERE b.depth < {MAX_CHAIN})
         UPDATE items SET is_ignored = ?2 WHERE local_item_id IN (SELECT id FROM below)"
    ))?
    .execute(params![item.as_i64(), is_ignored])?;
    Ok(())
}

/// The folder does not show `item` any more. The row of an item OneDrive never had goes;
/// a bound one stays, with what the two sides last agreed on.
fn remove_local_version(conn: &Connection, mut item: Item) -> Result<(), TreeError> {
    if item.onedrive_item.is_none() {
        return delete_item(conn, item.local_item_id);
    }
    item.local = None;
    item.content_presence = ContentPresence::NotOnDisk;
    item.content_before_write = None;
    write_item(conn, &item)
}

/// The file removed here whose removal is not sent yet and that OneDrive has at `place`: a
/// bound row the folder does not show, whose synced place is `place` and that has a synced
/// content, which a directory has not.
fn find_removed_file_at(conn: &Connection, place: &ItemPlace) -> Result<Option<LocalItemId>, TreeError> {
    let id: Option<i64> = conn
        .prepare_cached(
            "SELECT local_item_id FROM items
              WHERE synced_parent_id = ?1 AND synced_name = ?2 AND onedrive_item_id IS NOT NULL AND local_kind IS NULL AND synced_size IS NOT NULL
              ORDER BY local_item_id LIMIT 1",
        )?
        .query_row(params![place.parent_local_item_id.as_i64(), name_value(&place.name)], |row| row.get(0))
        .optional()?;
    Ok(id.map(LocalItemId::new))
}

fn make(tx: &Transaction<'_>, version: &ItemVersion, is_ignored: bool) -> Result<LocalItemId, TreeError> {
    let place = version.place.as_ref().ok_or_else(|| refuse("an item is made at a place: only the root has none".to_owned()))?;
    if (version.kind == ItemKind::File) != version.content.is_some() {
        return Err(refuse("a file is made with a content and a directory without".to_owned()));
    }
    check_directory(tx, place.parent_local_item_id)?;
    let removed = if version.kind == ItemKind::File { find_removed_file_at(tx, place)? } else { None };
    let made = insert_item(
        tx,
        &Item {
            local_item_id: LocalItemId::ROOT,
            onedrive_item: None,
            local: Some(version.clone()),
            synced_place: None,
            synced_content: None,
            // What a program makes is in the backing directory.
            content_presence: if version.kind == ItemKind::File { ContentPresence::OnDisk } else { ContentPresence::NotOnDisk },
            content_before_write: None,
            is_ignored,
        },
    )?;
    if let Some(removed) = removed {
        bindings::take_over_binding(tx, made, removed)?;
    }
    Ok(made)
}

fn move_item(tx: &Transaction<'_>, item: LocalItemId, place: &ItemPlace, is_ignored: bool) -> Result<(), TreeError> {
    shown_below_root(tx, item)?;
    check_directory(tx, place.parent_local_item_id)?;
    check_not_below(tx, place.parent_local_item_id, item)?;
    set_local_place(tx, item, place.parent_local_item_id, name_value(&place.name))?;
    set_ignored(tx, item, is_ignored)
}

/// `replacing` has the place of `replaced`, which leaves the folder. A file OneDrive does
/// not have, over a file it has, takes over the binding; otherwise each binding stays with
/// its item, and the replaced one is removed here like any other.
fn save_over(tx: &Transaction<'_>, replacing: LocalItemId, replaced: LocalItemId, is_ignored: bool) -> Result<(), TreeError> {
    if replacing == replaced {
        return Err(refuse(format!("item {replacing} is not saved over itself")));
    }
    let (replacing_item, replacing_version, _) = shown_below_root(tx, replacing)?;
    let (replaced_item, replaced_version, place) = shown_below_root(tx, replaced)?;
    if replacing_version.kind != replaced_version.kind {
        return Err(refuse(format!("item {replacing} and item {replaced} are not of one kind")));
    }
    check_empty(tx, replaced)?;
    check_not_below(tx, place.parent_local_item_id, replacing)?;
    let takes_over = replacing_version.kind == ItemKind::File && replacing_item.onedrive_item.is_none() && replaced_item.onedrive_item.is_some();
    if takes_over {
        bindings::take_over_binding(tx, replacing, replaced)?;
    } else {
        remove_local_version(tx, replaced_item)?;
    }
    set_local_place(tx, replacing, place.parent_local_item_id, name_value(&place.name))?;
    set_ignored(tx, replacing, is_ignored)
}

fn remove(tx: &Transaction<'_>, id: LocalItemId) -> Result<(), TreeError> {
    let (item, _, _) = shown_below_root(tx, id)?;
    check_empty(tx, id)?;
    remove_local_version(tx, item)
}

/// The file `id` has `content` from now on, and `before_write` as what it had before its
/// first writer.
fn set_local_content(tx: &Transaction<'_>, mut item: Item, mut local: ItemVersion, content: ContentVersion, before_write: Option<ContentVersion>) -> Result<(), TreeError> {
    local.content = Some(content);
    item.local = Some(local);
    item.content_before_write = before_write;
    write_item(tx, &item)
}

/// [`ItemStore::apply`], inside the caller's transaction: for a change that is written
/// with other rows of the store in one.
pub(crate) fn apply_change(tx: &Transaction<'_>, change: &LocalVersionChange) -> Result<LocalItemId, TreeError> {
    match change {
        LocalVersionChange::Made { version, is_ignored } => make(tx, version, *is_ignored),
        LocalVersionChange::Moved { item, place, is_ignored } => move_item(tx, *item, place, *is_ignored).map(|()| *item),
        LocalVersionChange::SavedOver { replacing, replaced, is_ignored } => save_over(tx, *replacing, *replaced, *is_ignored).map(|()| *replacing),
        LocalVersionChange::Removed { item } => remove(tx, *item).map(|()| *item),
        LocalVersionChange::OpenedForWriting { item } => {
            let (row, local, content) = shown_file(tx, *item)?;
            // Another content from now on, before any byte: of the same size and time until
            // the close says otherwise. What it was is kept, unless an open whose close was
            // never written kept an older one.
            let (mtime, mtime_nsec) = content.mtime();
            let opened = ContentVersion::ChangedHere { size: content.size(), mtime, mtime_nsec, change_count: content.change_count() + 1 };
            let before_write = row.content_before_write.clone().or(Some(content));
            set_local_content(tx, row, local, opened, before_write).map(|()| *item)
        }
        LocalVersionChange::ClosedChanged { item, size, mtime, mtime_nsec } => {
            let (row, local, content) = shown_file(tx, *item)?;
            let closed = ContentVersion::ChangedHere { size: *size, mtime: *mtime, mtime_nsec: *mtime_nsec, change_count: content.change_count().max(1) };
            set_local_content(tx, row, local, closed, None).map(|()| *item)
        }
        LocalVersionChange::ClosedUnchanged { item } => {
            let (row, local, content) = shown_file(tx, *item)?;
            // With no former content kept, the content it has is the one to leave: a
            // truncation, or a close already written.
            let before_write = row.content_before_write.clone().unwrap_or(content);
            set_local_content(tx, row, local, before_write, None).map(|()| *item)
        }
        LocalVersionChange::Truncated { item, size, mtime, mtime_nsec } => {
            let (row, local, content) = shown_file(tx, *item)?;
            let cut = ContentVersion::ChangedHere { size: *size, mtime: *mtime, mtime_nsec: *mtime_nsec, change_count: content.change_count() + 1 };
            set_local_content(tx, row, local, cut, None).map(|()| *item)
        }
    }
}

/// [`ItemStore::exchange_local_places`], inside the caller's transaction.
///
/// The index of local places is checked at each statement, so the two rows cannot take
/// each other's place in one: the first steps aside to a name no file can have (it holds a
/// NUL and a `/`), the second takes the first's place, and the first the second's. Nobody
/// reads the name in between: it does not outlive the transaction.
pub(crate) fn exchange_local_places(tx: &Transaction<'_>, first: LocalItemId, second: LocalItemId, first_is_ignored: bool, second_is_ignored: bool) -> Result<(), TreeError> {
    if first == second {
        return Err(refuse(format!("item {first} does not exchange places with itself")));
    }
    let (_, _, first_place) = shown_below_root(tx, first)?;
    let (_, _, second_place) = shown_below_root(tx, second)?;
    check_not_below(tx, second_place.parent_local_item_id, first)?;
    check_not_below(tx, first_place.parent_local_item_id, second)?;
    let aside = Value::Blob(format!("\0/{first}").into_bytes());
    set_local_place(tx, first, first_place.parent_local_item_id, aside)?;
    set_local_place(tx, second, first_place.parent_local_item_id, name_value(&first_place.name))?;
    set_local_place(tx, first, second_place.parent_local_item_id, name_value(&second_place.name))?;
    set_ignored(tx, first, first_is_ignored)?;
    set_ignored(tx, second, second_is_ignored)
}

/// The root of the folder is bound to the root of the drive, once the store knows both the
/// drive and its root's item id: called wherever it learns either. Nothing to do before.
pub(crate) fn bind_root(conn: &Connection) -> Result<(), TreeError> {
    let (Some(drive_id), Some(root_item_id)) = (meta::get(conn, meta::DRIVE_ID)?.and_then(DriveId::new), meta::get(conn, meta::ROOT_ITEM_ID)?) else {
        return Ok(());
    };
    conn.prepare_cached("UPDATE items SET onedrive_drive_id = ?2, onedrive_item_id = ?3 WHERE local_item_id = ?1")?
        .execute(params![LocalItemId::ROOT.as_i64(), drive_id.as_str(), root_item_id])?;
    Ok(())
}

#[cfg(test)]
mod tests;
