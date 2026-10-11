//! The records of place changes: the table `place_change_records`, one row for each change
//! of the backing directory's names that the daemon is about to make, written before the
//! disk is touched and gone in the transaction that writes the row of the change, or when
//! the change was undone (design §8, "A name's change is on record before the disk is
//! touched").
//!
//! **State owned:** the table, and nothing of `items`. A record says what the daemon was
//! about to do ([`PlaceChangeRecord`], a case for each way the mount's `PlaceChange`
//! changes a name: an entry made, moved, moved over another, set aside, a landed file
//! linked), in the rows' own terms: an item by its local item id, a place by its directory
//! and name. It is the daemon's own knowledge of its own change; no mark on disk is asked
//! for it.
//!
//! **Who changes it:** whoever is about to change a name writes a record
//! ([`ItemStore::record_place_change`]) in a transaction of its own, before the disk. The
//! record goes with the row of the change, in that row's transaction
//! ([`forget_place_change_record`], for a writer that has a transaction open;
//! [`ItemStore::settle_place_change_record`], which writes the row and takes the record in
//! one), or by itself when the change was undone or left nothing to write
//! ([`ItemStore::forget_place_change_record`]). A record that stands when the daemon
//! starts is a change a death cut short: the check at start reads them
//! ([`ItemStore::list_place_change_records`]) and finishes or undoes each before it looks
//! at anything else.
//!
//! A record does not follow its item: no trigger takes it when a row of `items` goes, and
//! the one who reads a record asks the rows what they say now.

use std::ffi::OsString;

use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};

use super::item_row::{check_name, content_from, content_values, find_item, name_value, set_content_presence};
use super::{apply_change, refuse, refuse_for, ItemStore, StepCommitOutcome};
use crate::model::{ContentPresence, ContentVersion, ItemKind, ItemPlace, LocalItemId, LocalVersionChange};
use crate::path_column::path_from;
use crate::{RowRefusalReason, TreeError};

/// Which record of `place_change_records`: the number the table gave it, never given twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PlaceChangeRecordId(i64);

impl PlaceChangeRecordId {
    pub fn as_i64(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for PlaceChangeRecordId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Whose entry is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MadeEntryOwner {
    /// An item that has no row yet: a program made a file or a directory. `replaced` is
    /// the file removed here whose binding the made file is to take over, as
    /// [`LocalVersionChange::Made`] names it.
    NewItem { replaced: Option<LocalItemId> },
    /// An item that has a row: the backing file of a file whose content was not on disk,
    /// made for a writer after the row was told.
    Item(LocalItemId),
}

/// What the daemon is about to do to the names of the backing directory: a case for each
/// way a name changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaceChangeRecord {
    /// A file or a directory is made at `place`.
    MakeEntry { kind: ItemKind, place: ItemPlace, owner: MadeEntryOwner },
    /// The entry of `item` is moved from `from` to `to`, where nothing is.
    MoveEntry { item: LocalItemId, from: ItemPlace, to: ItemPlace },
    /// The entry of `item` is moved from `from` to `to`, over the entry of `replaced`,
    /// which is set aside first, in the root of the backing directory, under `aside_name`.
    MoveEntryOver { item: LocalItemId, from: ItemPlace, to: ItemPlace, replaced: LocalItemId, aside_name: OsString },
    /// The entry of `item` at `place` is set aside, in the root of the backing directory,
    /// under `aside_name`, to be removed once its row is written.
    SetAside { item: LocalItemId, place: ItemPlace, aside_name: OsString },
    /// The downloaded file of `item`, which holds `content` whole, is linked at `place`.
    LinkFile { item: LocalItemId, place: ItemPlace, content: ContentVersion },
}

/// What is written to the rows as a record goes, in the record's last transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaceChangeRowWrite {
    /// The change of a local version the place change was for.
    LocalVersion(LocalVersionChange),
    /// The file `item` has the content it had before its writer's open again, and that
    /// content is not on disk: the row of a writer's open whose backing file was never
    /// made. Refused for a file that keeps no content from before a write.
    ContentBeforeWrite { item: LocalItemId },
    /// The whole content `content` of the file `item` is on disk: a landed download.
    /// Refused when the item's local content is another by now.
    ContentOnDisk { item: LocalItemId, content: ContentVersion },
}

/// The table: a record once, in the order they were written. `change` is the case;
/// `item_id` the item whose entry it is (null for an item with no row yet); `place_` the
/// place an entry is made at, moved to, set aside from or linked at; `from_` where a moved
/// entry was; `other_item_id` the item a made file replaces or a moved entry is moved
/// over; `aside_name` the name an entry is set aside to; the six columns of a content for
/// a landed file.
pub(crate) fn schema() -> &'static str {
    "CREATE TABLE place_change_records (
         record_id INTEGER PRIMARY KEY AUTOINCREMENT,
         change TEXT NOT NULL CHECK (change IN ('make_entry', 'move_entry', 'move_entry_over', 'set_aside', 'link_file')),
         item_id INTEGER, kind TEXT,
         place_parent_id INTEGER NOT NULL, place_name NOT NULL,
         from_parent_id INTEGER, from_name,
         other_item_id INTEGER, aside_name,
         size INTEGER, mtime INTEGER, mtime_nsec INTEGER, hash TEXT, ctag TEXT, change_count INTEGER);"
}

const RECORD_COLUMNS: &str = "change, item_id, kind, place_parent_id, place_name, from_parent_id, from_name, other_item_id, aside_name, size, mtime, mtime_nsec, hash, ctag, change_count";

/// The words of `change`, a case each.
const MAKE_ENTRY: &str = "make_entry";
const MOVE_ENTRY: &str = "move_entry";
const MOVE_ENTRY_OVER: &str = "move_entry_over";
const SET_ASIDE: &str = "set_aside";
const LINK_FILE: &str = "link_file";

/// The names a record is written with.
fn list_names(record: &PlaceChangeRecord) -> Vec<&OsString> {
    match record {
        PlaceChangeRecord::MakeEntry { place, .. } | PlaceChangeRecord::LinkFile { place, .. } => vec![&place.name],
        PlaceChangeRecord::MoveEntry { from, to, .. } => vec![&from.name, &to.name],
        PlaceChangeRecord::MoveEntryOver { from, to, aside_name, .. } => vec![&from.name, &to.name, aside_name],
        PlaceChangeRecord::SetAside { place, aside_name, .. } => vec![&place.name, aside_name],
    }
}

/// The values of [`RECORD_COLUMNS`] for `record`, in their order.
fn values_of(record: &PlaceChangeRecord) -> Vec<Value> {
    let id = |item: &LocalItemId| Value::from(item.as_i64());
    let place = |place: &ItemPlace| [id(&place.parent_local_item_id), name_value(&place.name)];
    let no_place = || [Value::Null, Value::Null];
    // The case, the item, the kind, the place, where from, the other item, the aside name, the content.
    let (change, item, kind, at, from, other, aside_name, content) = match record {
        PlaceChangeRecord::MakeEntry { kind, place: at, owner } => {
            let (item, replaced) = match owner {
                MadeEntryOwner::NewItem { replaced } => (Value::Null, replaced.as_ref().map_or(Value::Null, id)),
                MadeEntryOwner::Item(item) => (id(item), Value::Null),
            };
            (MAKE_ENTRY, item, Value::from(kind.as_str().to_owned()), place(at), no_place(), replaced, Value::Null, None)
        }
        PlaceChangeRecord::MoveEntry { item, from, to } => (MOVE_ENTRY, id(item), Value::Null, place(to), place(from), Value::Null, Value::Null, None),
        PlaceChangeRecord::MoveEntryOver { item, from, to, replaced, aside_name } => (MOVE_ENTRY_OVER, id(item), Value::Null, place(to), place(from), id(replaced), name_value(aside_name), None),
        PlaceChangeRecord::SetAside { item, place: at, aside_name } => (SET_ASIDE, id(item), Value::Null, place(at), no_place(), Value::Null, name_value(aside_name), None),
        PlaceChangeRecord::LinkFile { item, place: at, content } => (LINK_FILE, id(item), Value::Null, place(at), no_place(), Value::Null, Value::Null, Some(content)),
    };
    [Value::from(change.to_owned()), item, kind].into_iter().chain(at).chain(from).chain([other, aside_name]).chain(content_values(content)).collect()
}

/// A record read from a query that selects `record_id` and then [`RECORD_COLUMNS`]. A row
/// that does not hold what its case needs is not one this code wrote: an error of SQLite's
/// kind, never a record made up.
fn record_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<(PlaceChangeRecordId, PlaceChangeRecord)> {
    let missing = |column: usize| rusqlite::Error::InvalidColumnType(column, "place_change_records".to_owned(), rusqlite::types::Type::Null);
    let item_at = |column: usize| -> rusqlite::Result<Option<LocalItemId>> { Ok(row.get::<_, Option<i64>>(column)?.map(LocalItemId::new)) };
    let name_at = |column: usize| -> rusqlite::Result<OsString> {
        match row.get_ref(column)? {
            rusqlite::types::ValueRef::Null => Err(missing(column)),
            name => Ok(path_from(name).into_os_string()),
        }
    };
    let place_at = |parent: usize| -> rusqlite::Result<ItemPlace> { Ok(ItemPlace { parent_local_item_id: item_at(parent)?.ok_or_else(|| missing(parent))?, name: name_at(parent + 1)? }) };
    let item = || item_at(2)?.ok_or_else(|| missing(2));
    let at = place_at(4)?;
    let change: String = row.get(1)?;
    let record = match change.as_str() {
        MAKE_ENTRY => {
            let owner = match item_at(2)? {
                Some(item) => MadeEntryOwner::Item(item),
                None => MadeEntryOwner::NewItem { replaced: item_at(8)? },
            };
            PlaceChangeRecord::MakeEntry { kind: ItemKind::decode(&row.get::<_, String>(3)?), place: at, owner }
        }
        MOVE_ENTRY => PlaceChangeRecord::MoveEntry { item: item()?, from: place_at(6)?, to: at },
        MOVE_ENTRY_OVER => PlaceChangeRecord::MoveEntryOver { item: item()?, from: place_at(6)?, to: at, replaced: item_at(8)?.ok_or_else(|| missing(8))?, aside_name: name_at(9)? },
        SET_ASIDE => PlaceChangeRecord::SetAside { item: item()?, place: at, aside_name: name_at(9)? },
        LINK_FILE => PlaceChangeRecord::LinkFile { item: item()?, place: at, content: content_from(row, 10)?.ok_or_else(|| missing(10))? },
        _ => return Err(rusqlite::Error::InvalidColumnType(1, change, rusqlite::types::Type::Text)),
    };
    Ok((PlaceChangeRecordId(row.get(0)?), record))
}

impl ItemStore<'_> {
    /// Writes `record`, in a transaction of its own: what the daemon is about to do to the
    /// names of the backing directory, before it does it. Its id, by which it is taken
    /// again. Refused for a name that is no name of a directory entry.
    pub fn record_place_change(&mut self, record: &PlaceChangeRecord) -> Result<PlaceChangeRecordId, TreeError> {
        list_names(record).into_iter().try_for_each(|name| check_name(name))?;
        self.write(|tx| {
            tx.prepare_cached(&format!("INSERT INTO place_change_records ({RECORD_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)"))?.execute(params_from_iter(values_of(record)))?;
            Ok(PlaceChangeRecordId(tx.last_insert_rowid()))
        })
    }

    /// The records that stand, in the order they were written: the changes whose row was
    /// not written and that were not undone.
    pub fn list_place_change_records(&self) -> Result<Vec<(PlaceChangeRecordId, PlaceChangeRecord)>, TreeError> {
        let mut statement = self.store.conn.prepare_cached(&format!("SELECT record_id, {RECORD_COLUMNS} FROM place_change_records ORDER BY record_id"))?;
        let records = statement.query_map([], record_from)?;
        Ok(records.collect::<Result<Vec<_>, _>>()?)
    }

    /// The change of `record` was undone, or left nothing to write: the record goes, in a
    /// transaction of its own. Whether it stood.
    pub fn forget_place_change_record(&mut self, record: PlaceChangeRecordId) -> Result<bool, TreeError> {
        self.write(|tx| forget_place_change_record(tx, record))
    }

    /// Writes `write` to the rows and takes `record`, in one transaction: all of it or
    /// nothing. The item the write is about. A write the rows' rules refuse leaves the
    /// rows and the record as they were.
    pub fn settle_place_change_record(&mut self, record: PlaceChangeRecordId, write: &PlaceChangeRowWrite) -> Result<LocalItemId, TreeError> {
        self.write(|tx| {
            let item = match write {
                PlaceChangeRowWrite::LocalVersion(change) => apply_change(tx, change)?,
                PlaceChangeRowWrite::ContentBeforeWrite { item } => {
                    let row = find_item(tx, *item)?.ok_or_else(|| refuse_for(RowRefusalReason::ItemGone, format!("item {item} has no row")))?;
                    if row.local.is_none() || row.content_before_write.is_none() {
                        return Err(refuse(format!("item {item} keeps no content from before a write to put back")));
                    }
                    apply_change(tx, &LocalVersionChange::ClosedUnchanged { item: *item })?;
                    set_content_presence(tx, *item, ContentPresence::NotOnDisk)?;
                    *item
                }
                PlaceChangeRowWrite::ContentOnDisk { item, content } => {
                    let local_content = find_item(tx, *item)?.and_then(|row| row.local).and_then(|local| local.content);
                    if local_content.as_ref() != Some(content) {
                        return Err(refuse(format!("the local content of item {item} is not the one that landed")));
                    }
                    set_content_presence(tx, *item, ContentPresence::OnDisk)?;
                    *item
                }
            };
            forget_place_change_record(tx, record)?;
            Ok(item)
        })
    }
}

/// [`ItemStore::forget_place_change_record`], inside the caller's transaction: for the
/// writer of the row a place change was for — a change of a local version, a step's
/// commit — which takes the record in the transaction that writes the row. Whether it
/// stood.
pub(crate) fn forget_place_change_record(conn: &Connection, record: PlaceChangeRecordId) -> Result<bool, TreeError> {
    Ok(conn.prepare_cached("DELETE FROM place_change_records WHERE record_id = ?1")?.execute([record.0])? > 0)
}

/// What a step's commit ends with: `record`, the record of the step's place change when
/// it made one, goes in the commit's transaction when the commit was written, and stands
/// when the rows changed. The commit's outcome, as it was given.
pub(crate) fn forget_place_change_record_when_written(conn: &Connection, record: Option<PlaceChangeRecordId>, outcome: StepCommitOutcome) -> Result<StepCommitOutcome, TreeError> {
    if let (StepCommitOutcome::Written, Some(record)) = (outcome, record) {
        forget_place_change_record(conn, record)?;
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests;
