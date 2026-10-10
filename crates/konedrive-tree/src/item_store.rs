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
//!
//! A row is read and written in [`stored`], whose writers ask no rule and are reachable
//! from this module and [`bindings`] alone: what is outside changes a row through an
//! operation.
//! - *An item is a file or a directory all its life.* The schema: `kind` is on every row,
//!   also one the folder does not show, and no operation writes it after the row is made.
//!   A binding moves only between items of one kind ([`bindings`]).
//! - *The root is item 1, a directory with no place, and stays.* The schema's checks; the
//!   code refuses every change of it.
//! - *An id is never given twice.* The schema: `AUTOINCREMENT`.
//!
//! The OneDrive version is not in the rows yet (`onedrive_items` holds it), so the rules of
//! that side are not held here. A synced place names its directory by local item id; the
//! operations on bindings keep it pointing at a row.

use std::ffi::OsStr;
use std::path::PathBuf;

use rusqlite::types::Value;
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::meta;
use crate::model::{ContentPresence, ContentVersion, DriveId, Item, ItemKind, ItemPlace, ItemVersion, LocalItemId, LocalVersionChange, OneDriveItemId, OneDriveItemReference};
use crate::{TreeError, TreeStore, MAX_CHAIN};

/// The three operations that move a binding from one item to another.
mod bindings;
/// A row of `items` as the database keeps it, and its raw writers.
mod stored;

use stored::{delete_item, find_bound_item, find_item, find_item_at, insert_item, list_children, name_value, set_binding, set_local_place, write_item};

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
    /// that would break a rule of the rows is refused ([`TreeError::Refused`]) and changes
    /// nothing. A change of the content of a file the folder does not show any more, or
    /// whose row went, is nothing and no error: a program that holds a file open goes on
    /// after the file was removed or saved over.
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
    TreeError::Refused(what)
}

fn find_path(conn: &Connection, item: LocalItemId) -> Result<Option<PathBuf>, TreeError> {
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
fn read_shown(conn: &Connection, id: LocalItemId) -> Result<(Item, ItemVersion), TreeError> {
    let item = find_item(conn, id)?.ok_or_else(|| refuse(format!("item {id} has no row")))?;
    let local = item.local.clone().ok_or_else(|| refuse(format!("the folder does not show item {id}")))?;
    Ok((item, local))
}

/// [`read_shown`] for an item that is not the root, with its local place.
fn read_shown_below_root(conn: &Connection, id: LocalItemId) -> Result<(Item, ItemVersion, ItemPlace), TreeError> {
    let (item, local) = read_shown(conn, id)?;
    let place = local.place.clone().ok_or_else(|| refuse("the root of the folder has no place and is not changed".to_owned()))?;
    Ok((item, local, place))
}

/// The file `id` as the folder shows it, with its local content: what a change of a file's
/// content is written to. `None` when the folder does not show the item, or its row went: a
/// program may hold a file open that was removed or saved over since, and what it does to
/// that file is no change of a row. Refused for a directory.
fn find_shown_file(conn: &Connection, id: LocalItemId) -> Result<Option<(Item, ItemVersion, ContentVersion)>, TreeError> {
    let Some(item) = find_item(conn, id)? else { return Ok(None) };
    let Some(local) = item.local.clone() else { return Ok(None) };
    match local.content.clone() {
        Some(content) if item.kind == ItemKind::File => Ok(Some((item, local, content))),
        _ => Err(refuse(format!("item {id} is no file: it has no content to change"))),
    }
}

/// The rule "an item's parent exists and is a directory": refused unless `id` is a
/// directory the folder shows.
fn check_directory(conn: &Connection, id: LocalItemId) -> Result<(), TreeError> {
    match find_item(conn, id)? {
        Some(item) if item.local.is_none() => Err(refuse(format!("the folder does not show item {id}: nothing is placed in it"))),
        Some(item) if item.kind == ItemKind::Directory => Ok(()),
        Some(_) => Err(refuse(format!("item {id} is no directory: nothing is placed in it"))),
        None => Err(refuse(format!("the folder shows no directory {id} to place something in"))),
    }
}

/// The same rule from the other end: refused while the folder shows something in `id`.
fn check_empty(conn: &Connection, id: LocalItemId) -> Result<(), TreeError> {
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

/// The rule "one item per name in a directory": refused when the folder shows an item at
/// `place` already.
fn check_place_is_free(conn: &Connection, place: &ItemPlace) -> Result<(), TreeError> {
    match find_item_at(conn, place.parent_local_item_id, &place.name)? {
        Some(holder) => Err(refuse(format!("item {} has the name {:?} in directory {}", holder.local_item_id, place.name, place.parent_local_item_id))),
        None => Ok(()),
    }
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
/// bound row of a file the folder does not show, whose synced place is `place`.
fn find_removed_file_at(conn: &Connection, place: &ItemPlace) -> Result<Option<LocalItemId>, TreeError> {
    let id: Option<i64> = conn
        .prepare_cached(
            "SELECT local_item_id FROM items
              WHERE synced_parent_id = ?1 AND synced_name = ?2 AND onedrive_item_id IS NOT NULL AND local_parent_id IS NULL AND kind = 'file'
              ORDER BY local_item_id LIMIT 1",
        )?
        .query_row(params![place.parent_local_item_id.as_i64(), name_value(&place.name)], |row| row.get(0))
        .optional()?;
    Ok(id.map(LocalItemId::new))
}

fn make(tx: &Transaction<'_>, kind: ItemKind, place: &ItemPlace, content: Option<&ContentVersion>, is_ignored: bool) -> Result<LocalItemId, TreeError> {
    if (kind == ItemKind::File) != content.is_some() {
        return Err(refuse("a file is made with a content and a directory without".to_owned()));
    }
    check_directory(tx, place.parent_local_item_id)?;
    check_place_is_free(tx, place)?;
    let removed = if kind == ItemKind::File { find_removed_file_at(tx, place)? } else { None };
    // What a program makes is in the backing directory.
    let content_presence = if kind == ItemKind::File { ContentPresence::OnDisk } else { ContentPresence::NotOnDisk };
    let made = insert_item(tx, kind, None, Some(&ItemVersion { place: Some(place.clone()), content: content.cloned() }), content_presence, is_ignored)?;
    if let Some(removed) = removed {
        bindings::take_over_binding(tx, made, removed)?;
    }
    Ok(made)
}

fn move_item(tx: &Transaction<'_>, item: LocalItemId, place: &ItemPlace, is_ignored: bool) -> Result<(), TreeError> {
    read_shown_below_root(tx, item)?;
    check_directory(tx, place.parent_local_item_id)?;
    check_not_below(tx, place.parent_local_item_id, item)?;
    check_place_is_free(tx, place)?;
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
    let (replacing_item, _, _) = read_shown_below_root(tx, replacing)?;
    let (replaced_item, _, place) = read_shown_below_root(tx, replaced)?;
    if replacing_item.kind != replaced_item.kind {
        return Err(refuse(format!("item {replacing} and item {replaced} are not of one kind")));
    }
    check_empty(tx, replaced)?;
    check_not_below(tx, place.parent_local_item_id, replacing)?;
    let takes_over = replacing_item.kind == ItemKind::File && replacing_item.onedrive_item.is_none() && replaced_item.onedrive_item.is_some();
    if takes_over {
        bindings::take_over_binding(tx, replacing, replaced)?;
    } else {
        remove_local_version(tx, replaced_item)?;
    }
    set_local_place(tx, replacing, place.parent_local_item_id, name_value(&place.name))?;
    set_ignored(tx, replacing, is_ignored)
}

fn remove(tx: &Transaction<'_>, id: LocalItemId) -> Result<(), TreeError> {
    let (item, _, _) = read_shown_below_root(tx, id)?;
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
        LocalVersionChange::Made { kind, place, content, is_ignored } => make(tx, *kind, place, content.as_ref(), *is_ignored),
        LocalVersionChange::Moved { item, place, is_ignored } => move_item(tx, *item, place, *is_ignored).map(|()| *item),
        LocalVersionChange::SavedOver { replacing, replaced, is_ignored } => save_over(tx, *replacing, *replaced, *is_ignored).map(|()| *replacing),
        LocalVersionChange::Removed { item } => remove(tx, *item).map(|()| *item),
        LocalVersionChange::OpenedForWriting { item } => {
            let Some((row, local, content)) = find_shown_file(tx, *item)? else { return Ok(*item) };
            // Another content from now on, before any byte: of the same size and time until
            // the close says otherwise. What it was is kept, unless an open whose close was
            // never written kept an older one.
            let (mtime, mtime_nsec) = content.mtime();
            let opened = ContentVersion::ChangedHere { size: content.size(), mtime, mtime_nsec, change_count: content.change_count() + 1 };
            let before_write = row.content_before_write.clone().or(Some(content));
            set_local_content(tx, row, local, opened, before_write).map(|()| *item)
        }
        LocalVersionChange::ClosedChanged { item, size, mtime, mtime_nsec } => {
            let Some((row, local, content)) = find_shown_file(tx, *item)? else { return Ok(*item) };
            let closed = ContentVersion::ChangedHere { size: *size, mtime: *mtime, mtime_nsec: *mtime_nsec, change_count: content.change_count().max(1) };
            set_local_content(tx, row, local, closed, None).map(|()| *item)
        }
        LocalVersionChange::ClosedUnchanged { item } => {
            let Some((row, local, content)) = find_shown_file(tx, *item)? else { return Ok(*item) };
            // With no former content kept, the content it has is the one to leave: a
            // truncation, or a close already written.
            let before_write = row.content_before_write.clone().unwrap_or(content);
            set_local_content(tx, row, local, before_write, None).map(|()| *item)
        }
        LocalVersionChange::Truncated { item, size, mtime, mtime_nsec } => {
            let Some((row, local, content)) = find_shown_file(tx, *item)? else { return Ok(*item) };
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
    let (_, _, first_place) = read_shown_below_root(tx, first)?;
    let (_, _, second_place) = read_shown_below_root(tx, second)?;
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
    set_binding(conn, LocalItemId::ROOT, &OneDriveItemReference { drive_id, item_id: OneDriveItemId::new(root_item_id) })
}

#[cfg(test)]
mod tests;
