//! The rows of `items`: one per item, keyed by its local item id, which is also the kernel's
//! node id of what the folder shows. Everything in a row is written through [`ItemStore`],
//! each change one transaction.
//!
//! **What holds of the rows at all times**, and who holds it:
//!
//! - *An item's parent exists in the folder and is a directory.* The code: an item is made
//!   or moved only into a directory the folder shows ([`check_directory`]), and a directory
//!   leaves the folder only when the folder shows nothing in it ([`check_empty`]). The
//!   schema, whoever writes, and for the synced place as for the local one: a row is made
//!   with, and moved to, only a parent that is the row of a directory (the triggers
//!   `items_parent_directory_insert` and `items_parent_directory_update`), and a row does
//!   not go while another names it as its local or its synced parent (the trigger
//!   `items_parent_kept`).
//! - *No item is above itself.* The schema refuses a row that is its own parent; the code
//!   refuses a move into what is below the item ([`check_not_below`]).
//! - *The folder is no deeper than the rows are walked.* The code: an item is not made at a
//!   place, nor moved to one with what is below it, deeper than `MAX_CHAIN` directories
//!   ([`check_depth`]), so that every walk up from a row ends at the root.
//! - *In the folder, one item per name in a directory.* The schema: the unique index
//!   `items_local_place`.
//! - *A binding belongs to one item.* The schema: the unique index `items_onedrive_item`.
//!   Its drive id and its item id are never the empty text: the schema's checks, and the
//!   code where a binding is written ([`item_row`]).
//! - *Nothing is agreed on for an item with no binding.* The schema's check: a synced place
//!   or content is written to a bound row, or with its binding in one statement. The code:
//!   an item that gives its binding up gives up what was agreed on in the same statement.
//!   A binding is given first and moved in [`bindings`] and nowhere else: to an item of the
//!   folder OneDrive confirmed, with a row made for an item of OneDrive, to the root where
//!   the store learns the drive's root, and from one item to another by its three
//!   operations.
//! - *An item is a file or a directory all its life.* The schema: `kind` is on every row,
//!   also one the folder does not show, and no operation writes it after the row is made.
//!   A binding moves only between items of one kind ([`bindings`]).
//! - *A name is the name of a directory entry.* The code, where a row is written
//!   ([`item_row`]): not empty, not `.` or `..`, with no `/` and no NUL.
//! - *The root is item 1, a directory with no place, and stays.* The schema's checks, and
//!   its trigger `items_parent_kept`, which lets nobody delete row 1; the code refuses
//!   every change of it.
//! - *An id is never given twice.* The schema: `AUTOINCREMENT`.
//!
//! What the code refuses is [`TreeError::Refused`], and a rule of the schema that an
//! operation could trip is asked by the code before, so that an operation fails as SQLite's
//! only when SQLite or the disk failed.
//!
//! A row is read and written in [`item_row`], whose writers ask no rule but the one of names
//! and the one of an item id of OneDrive, and are reachable from this module and [`bindings`] alone: what is outside changes a row
//! through an operation.
//!
//! The OneDrive version of a row is written by a listing ([`onedrive_versions`]) and by
//! nothing else here. Of that side the schema holds what it holds of the others, per row:
//! a parent is a directory's row, and no row is its own parent. It does not hold "one item
//! per name" there, nor "no item is above itself", nor that a parent has an OneDrive version
//! itself: a listing says its changes in any order and over several pages, and the rows say
//! what it said so far. A synced place and an OneDrive place name their directory by local
//! item id; the schema keeps each pointing at a directory's row, and the operations on
//! bindings at the row that has that directory's binding.

use std::ffi::OsStr;
use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::model::{ContentPresence, ContentVersion, Item, ItemKind, ItemPlace, ItemVersion, LocalItemId, LocalVersionChange, OneDriveItemReference, Placement};
use crate::{TreeError, TreeStore, MAX_CHAIN};

/// The three operations that move a binding from one item to another.
mod bindings;
/// A row of `items`: its columns, reading it, and its raw writers.
mod item_row;
/// What a listing of OneDrive says of an item, written into its row.
mod onedrive_versions;

pub(crate) use bindings::bind_root;
pub(crate) use item_row::{BINDING_ALONE, CHANGED_IN_ONEDRIVE};

use item_row::{delete_item, find_bound_item, find_item, find_item_at, insert_item, list_changed_in_onedrive, list_children, set_local_place, set_local_place_aside, write_item};

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
    /// does not show and for an id with no row, which [`find_item`](Self::find_item) tells
    /// apart. Refused, not `None`, when the directories above it do not end at the root
    /// within the bound of the folder's depth: no operation leaves such rows.
    pub fn find_path(&self, item: LocalItemId) -> Result<Option<PathBuf>, TreeError> {
        find_path(&self.store.conn, item)
    }

    /// Whether `item` is left here, and so never sent: it has no binding, and its own name is
    /// ignored or that of a directory above it that has no binding. An item OneDrive has is
    /// never left here, also below an ignored directory that OneDrive does not have. Derived
    /// from the rows at each call, never stored. Refused for an id with no row.
    pub fn is_ignored(&self, item: LocalItemId) -> Result<bool, TreeError> {
        is_ignored(&self.store.conn, item)
    }

    /// The items whose OneDrive version is not the synced one, in its place or its content:
    /// what OneDrive made, moved, changed or removed since the two sides last agreed. At
    /// most `limit` of them by local item id, after the item `after` when one is given: a
    /// plan reads a bounded number and asks for the next. Found by an index, never by
    /// reading every row. A row with no version on either side and nothing agreed on is
    /// not among them.
    pub fn list_changed_in_onedrive(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<Item>, TreeError> {
        list_changed_in_onedrive(&self.store.conn, after, limit)
    }

    /// Whether the folder can show `item` at all, as OneDrive has it: its own placement
    /// says so, and that of every directory above it in OneDrive, up to the root. An item
    /// OneDrive does not have cannot be shown as OneDrive's, nor one whose directories do
    /// not end at the root within the bound of every walk: a directory that was never
    /// listed, or rows of a listing that has not ended that name each other in a circle.
    /// The root can. Refused for an id with no row.
    pub fn can_be_shown(&self, item: LocalItemId) -> Result<bool, TreeError> {
        can_be_shown(&self.store.conn, item)
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
    /// exchanged, as `renameat2` does with `RENAME_EXCHANGE`. Each takes, with the other's
    /// name, what the upload rules say of that name; nothing else of either changes.
    pub fn exchange_local_places(&mut self, first: LocalItemId, second: LocalItemId) -> Result<(), TreeError> {
        self.write(|tx| exchange_local_places(tx, first, second))
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

/// The error of a change the rows' rules refuse: the change wrote nothing.
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
    Err(refuse(format!("the directories above item {item} do not end at the root")))
}

/// The item `id` as the folder shows it, with its local version; refused when it has no
/// row, or the folder does not show it.
fn read_shown(conn: &Connection, id: LocalItemId) -> Result<(Item, ItemVersion), TreeError> {
    let item = find_item(conn, id)?.ok_or_else(|| refuse(format!("item {id} has no row")))?;
    let local = item.local.clone().ok_or_else(|| refuse(format!("the folder does not show item {id}")))?;
    Ok((item, local))
}

fn is_ignored(conn: &Connection, id: LocalItemId) -> Result<bool, TreeError> {
    let item = find_item(conn, id)?.ok_or_else(|| refuse(format!("item {id} has no row")))?;
    if item.onedrive_item.is_some() {
        return Ok(false);
    }
    if item.is_name_ignored {
        return Ok(true);
    }
    let mut above = item.local.and_then(|local| local.place).map(|place| place.parent_local_item_id);
    // One row for each directory above the item.
    for _ in 0..=MAX_CHAIN {
        let Some(directory) = above.and_then(|directory| find_item(conn, directory).transpose()).transpose()? else { return Ok(false) };
        if directory.is_name_ignored && directory.onedrive_item.is_none() {
            return Ok(true);
        }
        above = directory.local.and_then(|local| local.place).map(|place| place.parent_local_item_id);
    }
    Err(refuse(format!("the directories above item {id} do not end at the root")))
}

fn can_be_shown(conn: &Connection, id: LocalItemId) -> Result<bool, TreeError> {
    let mut row = find_item(conn, id)?.ok_or_else(|| refuse(format!("item {id} has no row")))?;
    // One row for each directory above the item.
    for _ in 0..=MAX_CHAIN {
        if row.local_item_id == LocalItemId::ROOT {
            return Ok(true);
        }
        let Some(place) = row.onedrive.and_then(|onedrive| onedrive.place).filter(|_| row.placement == Placement::Placed) else { return Ok(false) };
        let Some(directory) = find_item(conn, place.parent_local_item_id)? else { return Ok(false) };
        row = directory;
    }
    Ok(false)
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

/// `directory` and each directory above it in the folder, up to the root. Refused when
/// they do not end at the root within the bound of every walk of the rows.
fn list_directories_up(conn: &Connection, directory: LocalItemId) -> Result<Vec<LocalItemId>, TreeError> {
    let mut parent_of = conn.prepare_cached("SELECT local_parent_id FROM items WHERE local_item_id = ?1")?;
    let mut directories = Vec::new();
    let mut at = Some(directory);
    while let Some(here) = at {
        if directories.len() > MAX_CHAIN {
            return Err(refuse(format!("the directories above item {directory} do not end at the root")));
        }
        directories.push(here);
        at = parent_of.query_row([here.as_i64()], |row| row.get::<_, Option<i64>>(0)).optional()?.flatten().map(LocalItemId::new);
    }
    Ok(directories)
}

/// The rule "no item is above itself": refused when `item` is among `directories`, a
/// directory and those above it, so that `item` placed in that directory would be in itself
/// or below itself.
fn check_not_below(directories: &[LocalItemId], item: LocalItemId) -> Result<(), TreeError> {
    if directories.contains(&item) {
        return Err(refuse(format!("item {item} would be placed in itself or below itself")));
    }
    Ok(())
}

/// How many directories deep the folder shows something below `item`: 0 for a file and for
/// a directory that holds nothing.
fn count_levels_below(conn: &Connection, item: LocalItemId) -> Result<usize, TreeError> {
    let levels: i64 = conn
        .prepare_cached(&format!(
            "WITH RECURSIVE below(id, depth) AS (
                 SELECT ?1, 0
                 UNION ALL
                 SELECT c.local_item_id, b.depth + 1 FROM items c JOIN below b ON c.local_parent_id = b.id WHERE b.depth <= {MAX_CHAIN})
             SELECT max(depth) FROM below"
        ))?
        .query_row([item.as_i64()], |row| row.get(0))?;
    Ok(levels.max(0) as usize)
}

/// The bound of the folder's depth, which every walk of the rows has: refused when an item
/// placed in the first of `directories` (a directory and those above it), with
/// `levels_below` levels of the folder below the item, would be deeper than the walks go.
fn check_depth(directories: &[LocalItemId], levels_below: usize) -> Result<(), TreeError> {
    if directories.len() + levels_below > MAX_CHAIN {
        return Err(refuse(format!("the place is deeper than {MAX_CHAIN} directories below the root of the folder, with what is below the item")));
    }
    Ok(())
}

/// `item`, with all the folder shows below it, can be placed in `directory`: not in itself
/// or below itself ([`check_not_below`]), and not too deep ([`check_depth`]).
fn check_can_hold(conn: &Connection, directory: LocalItemId, item: LocalItemId) -> Result<(), TreeError> {
    let directories = list_directories_up(conn, directory)?;
    check_not_below(&directories, item)?;
    check_depth(&directories, count_levels_below(conn, item)?)
}

/// The rule "one item per name in a directory": refused when the folder shows an item at
/// `place` already.
fn check_place_is_free(conn: &Connection, place: &ItemPlace) -> Result<(), TreeError> {
    match find_item_at(conn, place.parent_local_item_id, &place.name)? {
        Some(holder) => Err(refuse(format!("item {} has the name {:?} in directory {}", holder.local_item_id, place.name, place.parent_local_item_id))),
        None => Ok(()),
    }
}

/// The folder does not show `item` any more. The row of an item OneDrive never had goes;
/// a bound one stays, with what the two sides last agreed on.
fn remove_local_version(conn: &Connection, mut item: Item) -> Result<(), TreeError> {
    if item.onedrive_item.is_none() {
        // No operation leaves something agreed to be in a directory OneDrive does not have;
        // rows that say so all the same are refused here, before the schema would.
        let holds = conn.prepare_cached("SELECT 1 FROM items WHERE synced_parent_id = ?1 LIMIT 1")?.exists([item.local_item_id.as_i64()])?;
        if holds {
            return Err(refuse(format!("the two sides agreed something is in item {}, which has no binding: its row does not go", item.local_item_id)));
        }
        return delete_item(conn, item.local_item_id);
    }
    item.local = None;
    item.content_presence = ContentPresence::NotOnDisk;
    item.content_before_write = None;
    write_item(conn, &item)
}

/// The statement of a write operation that a file it made replaces the item `replaced`
/// holds: `replaced` is a file of OneDrive the folder does not show, so one removed here
/// whose removal OneDrive does not know of. Which removed file a made file replaces, where
/// several could be meant, is not asked here.
fn check_replaced_by_made_file(conn: &Connection, kind: ItemKind, replaced: LocalItemId) -> Result<(), TreeError> {
    if kind != ItemKind::File {
        return Err(refuse(format!("a directory is made as a new item: it does not replace item {replaced}")));
    }
    if replaced == LocalItemId::ROOT {
        return Err(refuse("the root of the folder is not replaced".to_owned()));
    }
    let row = find_item(conn, replaced)?.ok_or_else(|| refuse(format!("item {replaced}, which a made file is to replace, has no row")))?;
    match row {
        Item { local: Some(_), .. } => Err(refuse(format!("the folder still shows item {replaced}: a made file does not replace it"))),
        Item { onedrive_item: None, .. } => Err(refuse(format!("item {replaced} has no binding for a made file to take over"))),
        Item { kind: ItemKind::Directory, .. } => Err(refuse(format!("item {replaced} is a directory: a made file does not replace it"))),
        Item { kind: ItemKind::File, .. } => Ok(()),
    }
}

fn make(tx: &Transaction<'_>, kind: ItemKind, place: &ItemPlace, content: Option<&ContentVersion>, is_name_ignored: bool, replaced: Option<LocalItemId>) -> Result<LocalItemId, TreeError> {
    if (kind == ItemKind::File) != content.is_some() {
        return Err(refuse("a file is made with a content and a directory without".to_owned()));
    }
    check_directory(tx, place.parent_local_item_id)?;
    check_depth(&list_directories_up(tx, place.parent_local_item_id)?, 0)?;
    check_place_is_free(tx, place)?;
    if let Some(replaced) = replaced {
        check_replaced_by_made_file(tx, kind, replaced)?;
    }
    // What a program makes is in the backing directory.
    let content_presence = if kind == ItemKind::File { ContentPresence::OnDisk } else { ContentPresence::NotOnDisk };
    let made = insert_item(tx, kind, None, Some(&ItemVersion { place: Some(place.clone()), content: content.cloned() }), content_presence, is_name_ignored)?;
    if let Some(replaced) = replaced {
        bindings::take_over_binding(tx, made, replaced)?;
    }
    Ok(made)
}

fn move_item(tx: &Transaction<'_>, item: LocalItemId, place: &ItemPlace, is_name_ignored: bool) -> Result<(), TreeError> {
    read_shown_below_root(tx, item)?;
    check_directory(tx, place.parent_local_item_id)?;
    check_can_hold(tx, place.parent_local_item_id, item)?;
    check_place_is_free(tx, place)?;
    set_local_place(tx, item, place, is_name_ignored)
}

/// `replacing` has the place of `replaced`, which leaves the folder. What becomes of the
/// two bindings is a case of what the two items are, as today's rename records it:
///
/// - *The replaced item is not in OneDrive*: its row goes, and nothing of it is left.
/// - *A file that is not in OneDrive over a file that is*: a program saved the file by a
///   rename. The replacing file takes over the binding: OneDrive sees the replaced file
///   with new content.
/// - *An item of OneDrive over an item of OneDrive*, files or directories: each keeps its
///   binding. The replacing one is moved in OneDrive, and the replaced one is removed
///   there: its row stays as one the folder does not show, with what was agreed on.
/// - *A directory that is not in OneDrive over a directory that is*: the replacing one is
///   a new directory at that name, and the replaced one is removed in OneDrive as above. A
///   directory takes no binding over.
fn save_over(tx: &Transaction<'_>, replacing: LocalItemId, replaced: LocalItemId) -> Result<(), TreeError> {
    if replacing == replaced {
        return Err(refuse(format!("item {replacing} is not saved over itself")));
    }
    let (replacing_item, _, _) = read_shown_below_root(tx, replacing)?;
    let (replaced_item, _, place) = read_shown_below_root(tx, replaced)?;
    if replacing_item.kind != replaced_item.kind {
        return Err(refuse(format!("item {replacing} and item {replaced} are not of one kind")));
    }
    check_empty(tx, replaced)?;
    check_can_hold(tx, place.parent_local_item_id, replacing)?;
    // The replacing item has the replaced one's name, and what the rules say of that name.
    let is_name_ignored = replaced_item.is_name_ignored;
    match (replacing_item.kind, &replacing_item.onedrive_item, &replaced_item.onedrive_item) {
        // Never in OneDrive: the row goes.
        (_, _, None) => remove_local_version(tx, replaced_item)?,
        // Saved by a rename. The replaced file leaves the folder first: a binding is taken
        // over from a file the folder does not show.
        (ItemKind::File, None, Some(_)) => {
            remove_local_version(tx, replaced_item)?;
            bindings::take_over_binding(tx, replacing, replaced)?;
        }
        // Removed here, to be removed in OneDrive; the replacing item is its own.
        (_, Some(_), Some(_)) | (ItemKind::Directory, None, Some(_)) => remove_local_version(tx, replaced_item)?,
    }
    set_local_place(tx, replacing, &place, is_name_ignored)
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
/// with other rows of the store in one. On any error the caller drops its transaction and
/// never commits it: a refusal comes before the change wrote anything, but a failure of
/// SQLite, of the disk or of a rule of the schema may come after a part of it was written.
pub(crate) fn apply_change(tx: &Transaction<'_>, change: &LocalVersionChange) -> Result<LocalItemId, TreeError> {
    match change {
        LocalVersionChange::Made { kind, place, content, is_name_ignored, replaced } => make(tx, *kind, place, content.as_ref(), *is_name_ignored, *replaced),
        LocalVersionChange::Moved { item, place, is_name_ignored } => move_item(tx, *item, place, *is_name_ignored).map(|()| *item),
        LocalVersionChange::SavedOver { replacing, replaced } => save_over(tx, *replacing, *replaced).map(|()| *replacing),
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
/// each other's place in one: the first steps aside to a name no file can have, the second
/// takes the first's place, and the first the second's. Nobody reads the name in between:
/// it does not outlive the transaction. On any error the caller drops its transaction and
/// never commits it: after the first step it holds that name.
pub(crate) fn exchange_local_places(tx: &Transaction<'_>, first: LocalItemId, second: LocalItemId) -> Result<(), TreeError> {
    if first == second {
        return Err(refuse(format!("item {first} does not exchange places with itself")));
    }
    let (first_item, _, first_place) = read_shown_below_root(tx, first)?;
    let (second_item, _, second_place) = read_shown_below_root(tx, second)?;
    check_can_hold(tx, second_place.parent_local_item_id, first)?;
    check_can_hold(tx, first_place.parent_local_item_id, second)?;
    set_local_place_aside(tx, first, &first_place, first_item.is_name_ignored)?;
    set_local_place(tx, second, &first_place, first_item.is_name_ignored)?;
    set_local_place(tx, first, &second_place, second_item.is_name_ignored)
}

#[cfg(test)]
mod tests;
