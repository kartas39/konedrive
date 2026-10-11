//! The items uploads left in OneDrive with other content than was sent whose file cannot
//! remove them any more: the table `mismatched_item_removals`, one row for each,
//! kept until the item is removed in OneDrive, found gone, or found changed by another. A
//! file cannot once its row went, once it is left here (its name, or that of a directory
//! above it, is ignored: it is never sent, so no step of it runs), and once it has a
//! binding (it was merged with or bound to an item of OneDrive: it is no new file any
//! more, and the step that removes a leftover is a new file's). The trigger
//! `step_memory_item_gone` holds the first and `mismatched_item_file_unable` the other
//! two, whoever writes the row — at the write that makes it so and at no other: the row
//! gets a binding it did not have, or another one; or it is left here and was not before.
//! A file that is renamed, moved or changed here keeps its leftover. As
//! the address of an abandoned upload session, it is what is left to do in OneDrive once
//! the item's memory is gone: without it the item with the wrong content stays, and the
//! next listing shows it in the folder as a file nobody made.
//!
//! The row of a file that OneDrive never had goes in the write operation that removes the
//! file, not in a step: no step could read the memory first. So the schema keeps the item's
//! reference past the row.
//!
//! **An item the folder has is never here, nor in a file's memory.** An item of OneDrive
//! that a row is bound to, and that the folder shows or the two sides agreed on a place of,
//! is the user's by now, whatever upload made it: a listing brought it and it was shown, a
//! plan merged the file with it, the commit of the upload bound the file to it. The
//! schema's triggers `mismatched_item_of_folder_insert` and `mismatched_item_of_folder`
//! take it out of both tables whoever writes such a row; the reader of the removals asks
//! the rows again; and nothing is kept of it when it is given.

use rusqlite::{params, Connection, OptionalExtension, Transaction};

use super::item_memory::MismatchedItem;
use super::memory_row::mismatched_item_from;
use crate::item_store::{confirm_onedrive_version_of_item, find_item_bound_to, ConfirmedOneDriveVersion};
use crate::model::{LocalItemId, OneDriveItemReference};
use crate::TreeError;

/// SQL for "a row has the binding (`drive`, `item`) and the folder has that item": it
/// shows it, or the two sides agreed on a place of it.
fn folder_has(drive: &str, item: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM items WHERE onedrive_drive_id = {drive} AND onedrive_item_id = {item}
                  AND (local_parent_id IS NOT NULL OR synced_parent_id IS NOT NULL))"
    )
}

/// The table: an item of OneDrive once, by its drive and its id, in the order the rows
/// went, with the tag that says whether the item is still what the upload left. And the
/// two triggers that hold "an item the folder has is never here, nor in a file's memory",
/// and the one that takes a leftover from a file that cannot remove it any more.
pub(super) fn schema() -> String {
    let take_out = "
             DELETE FROM mismatched_item_removals WHERE onedrive_drive_id = NEW.onedrive_drive_id AND onedrive_item_id = NEW.onedrive_item_id;
             UPDATE step_memory SET mismatched_drive_id = NULL, mismatched_item_id = NULL, mismatched_ctag = NULL, mismatched_etag = NULL
              WHERE mismatched_item_id = NEW.onedrive_item_id AND mismatched_drive_id = NEW.onedrive_drive_id;";
    let is_of_folder = "NEW.onedrive_item_id IS NOT NULL AND (NEW.local_parent_id IS NOT NULL OR NEW.synced_parent_id IS NOT NULL)";
    format!(
        "CREATE TABLE mismatched_item_removals (
             removal_order INTEGER PRIMARY KEY AUTOINCREMENT, onedrive_drive_id TEXT NOT NULL, onedrive_item_id TEXT NOT NULL, ctag TEXT, etag TEXT,
             UNIQUE (onedrive_drive_id, onedrive_item_id));
         CREATE TRIGGER mismatched_item_of_folder_insert AFTER INSERT ON items WHEN {is_of_folder} BEGIN{take_out}
         END;
         CREATE TRIGGER mismatched_item_of_folder AFTER UPDATE OF onedrive_drive_id, onedrive_item_id, local_parent_id, synced_parent_id ON items
             WHEN {is_of_folder} BEGIN{take_out}
         END;
         CREATE TRIGGER mismatched_item_file_unable AFTER UPDATE OF onedrive_drive_id, onedrive_item_id, is_name_ignored, is_below_ignored_directory ON items
             WHEN (NEW.onedrive_item_id IS NOT NULL AND (OLD.onedrive_item_id IS NULL OR OLD.onedrive_item_id != NEW.onedrive_item_id OR OLD.onedrive_drive_id != NEW.onedrive_drive_id))
               OR ({new_left_here} AND NOT {old_left_here}) BEGIN{leave}
             UPDATE step_memory SET mismatched_drive_id = NULL, mismatched_item_id = NULL, mismatched_ctag = NULL, mismatched_etag = NULL
              WHERE local_item_id = NEW.local_item_id AND mismatched_item_id IS NOT NULL;
         END;",
        leave = leave_statement("NEW"),
        new_left_here = is_left_here("NEW"),
        old_left_here = is_left_here("OLD"),
    )
}

/// What the trigger `step_memory_item_gone` does to this table when the row `OLD` of `items`
/// goes, while the row of `step_memory` is still there: the item an upload of the file left
/// is kept to be removed, unless the folder has it.
pub(super) fn item_gone_statements() -> String {
    leave_statement("OLD")
}

/// SQL of a trigger on `items` for "the item of the row `row` (`OLD` or `NEW`) is left here":
/// it has no binding, and its name or that of a directory above it is ignored
/// (`item_store`'s `LEFT_HERE`, of a trigger's row).
fn is_left_here(row: &str) -> String {
    format!("({row}.onedrive_item_id IS NULL AND ({row}.is_name_ignored != 0 OR {row}.is_below_ignored_directory != 0))")
}

/// SQL of a trigger on `items`: the item an upload of the file of the row `row` (`OLD` or
/// `NEW`) left is kept to be removed by the daemon, unless the folder has it.
fn leave_statement(row: &str) -> String {
    format!(
        "
    INSERT OR IGNORE INTO mismatched_item_removals (onedrive_drive_id, onedrive_item_id, ctag, etag)
        SELECT mismatched_drive_id, mismatched_item_id, mismatched_ctag, mismatched_etag FROM step_memory
         WHERE local_item_id = {row}.local_item_id AND mismatched_item_id IS NOT NULL AND NOT {};",
        folder_has("mismatched_drive_id", "mismatched_item_id")
    )
}

/// Whether the folder has the item of OneDrive `item` as the rows are now.
pub(super) fn is_item_of_folder(conn: &Connection, item: &OneDriveItemReference) -> Result<bool, TreeError> {
    let sql = format!("SELECT {}", folder_has("?1", "?2"));
    Ok(conn.prepare_cached(&sql)?.query_row(params![item.drive_id.as_str(), item.item_id.as_str()], |row| row.get(0))?)
}

fn removal_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<MismatchedItem>> {
    let (drive_id, item_id, ctag, etag): (Option<String>, Option<String>, Option<String>, Option<String>) = (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?);
    Ok(mismatched_item_from(drive_id, item_id, ctag.as_deref(), etag.as_deref()))
}

/// Up to `limit` items left to remove, the one whose file's row went first first. None of
/// them is an item the folder has: the rows are asked here too.
pub(crate) fn list_removals(conn: &Connection, limit: usize) -> Result<Vec<MismatchedItem>, TreeError> {
    let sql = format!(
        "SELECT onedrive_drive_id, onedrive_item_id, ctag, etag FROM mismatched_item_removals WHERE NOT {} ORDER BY removal_order LIMIT ?1",
        folder_has("mismatched_item_removals.onedrive_drive_id", "mismatched_item_removals.onedrive_item_id")
    );
    let mut statement = conn.prepare_cached(&sql)?;
    let items = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], removal_from)?;
    Ok(items.collect::<Result<Vec<_>, _>>()?.into_iter().flatten().collect())
}

/// `upload` is left to remove with no file's memory keeping it: its file's memory holds a
/// later one from now on.
pub(super) fn leave_removal(conn: &Connection, upload: &MismatchedItem) -> Result<(), TreeError> {
    let (ctag, etag) = match &upload.tag {
        super::item_memory::UploadTag::CTag(ctag) => (Some(ctag.as_str()), None),
        super::item_memory::UploadTag::ETag(etag) => (None, Some(etag.as_str())),
        super::item_memory::UploadTag::NotGiven => (None, None),
    };
    conn.prepare_cached("INSERT OR IGNORE INTO mismatched_item_removals (onedrive_drive_id, onedrive_item_id, ctag, etag) VALUES (?1, ?2, ?3, ?4)")?
        .execute(params![upload.item.drive_id.as_str(), upload.item.item_id.as_str(), ctag, etag])?;
    Ok(())
}

/// The removal left of the item `item`, if one is.
pub(super) fn find_removal(conn: &Connection, item: &OneDriveItemReference) -> Result<Option<MismatchedItem>, TreeError> {
    let sql = "SELECT onedrive_drive_id, onedrive_item_id, ctag, etag FROM mismatched_item_removals WHERE onedrive_drive_id = ?1 AND onedrive_item_id = ?2";
    Ok(conn.prepare_cached(sql)?.query_row(params![item.drive_id.as_str(), item.item_id.as_str()], removal_from).optional()?.flatten())
}

/// The item `item` is not left to remove any more.
pub(super) fn forget_removal(conn: &Connection, item: &OneDriveItemReference) -> Result<(), TreeError> {
    conn.prepare_cached("DELETE FROM mismatched_item_removals WHERE onedrive_drive_id = ?1 AND onedrive_item_id = ?2")?.execute(params![item.drive_id.as_str(), item.item_id.as_str()])?;
    Ok(())
}

/// The files whose memory holds the item `item` as the one an upload of theirs left.
pub(super) fn list_files_of(conn: &Connection, item: &OneDriveItemReference) -> Result<Vec<LocalItemId>, TreeError> {
    let mut statement = conn.prepare_cached("SELECT local_item_id FROM step_memory WHERE mismatched_item_id = ?1 AND mismatched_drive_id = ?2 ORDER BY local_item_id")?;
    let files = statement.query_map(params![item.item_id.as_str(), item.drive_id.as_str()], |row| Ok(LocalItemId::new(row.get(0)?)))?;
    Ok(files.collect::<Result<Vec<_>, _>>()?)
}

/// [`StepMemory::forget_removed_mismatched_item`](super::StepMemory::forget_removed_mismatched_item),
/// inside the caller's transaction: the item is forgotten as
/// [`forget_mismatched_item`](super::forget_mismatched_item) forgets it, and the row a
/// listing made of it loses its OneDrive version as the confirmation of a removal a step
/// sent takes it — stamped, so that a page fetched before the removal does not bring the
/// item back. So the plan is never given a row that says OneDrive has an item that is
/// gone. A row the folder has — it shows the item, or the two sides agreed on a place of
/// it — is no leftover's, and is not written.
pub(super) fn forget_removed_item(tx: &Transaction<'_>, onedrive_item: &OneDriveItemReference) -> Result<(), TreeError> {
    super::forget_mismatched_item(tx, onedrive_item)?;
    let listed = find_item_bound_to(tx, onedrive_item)?.filter(|row| row.local.is_none() && row.synced_place.is_none() && row.onedrive.is_some());
    if let Some(row) = listed {
        confirm_onedrive_version_of_item(tx, row.local_item_id, &ConfirmedOneDriveVersion::Removed)?;
    }
    Ok(())
}
