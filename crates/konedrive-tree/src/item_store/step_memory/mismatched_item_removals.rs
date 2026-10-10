//! The items uploads left in OneDrive with other content than was sent whose file's row
//! went before they were removed: the table `mismatched_item_removals`, one row for each,
//! kept until the item is removed in OneDrive, found gone, or found changed by another. As
//! the address of an abandoned upload session, it is what is left to do in OneDrive once
//! the item's memory is gone: without it the item with the wrong content stays, and the
//! next listing shows it in the folder as a file nobody made.
//!
//! The row of a file that OneDrive never had goes in the write operation that removes the
//! file, not in a step: no step could read the memory first. So the schema keeps the item's
//! id past the row.

use rusqlite::Connection;

use super::item_memory::MismatchedItem;
use crate::model::OneDriveItemId;
use crate::TreeError;

/// The table: an item id of OneDrive once, in the order the rows went, with the tag that
/// says whether the item is still what the upload left.
pub(super) fn schema() -> &'static str {
    "CREATE TABLE mismatched_item_removals (
         removal_order INTEGER PRIMARY KEY AUTOINCREMENT, onedrive_item_id TEXT NOT NULL UNIQUE, ctag TEXT, etag TEXT);"
}

/// What the trigger `step_memory_item_gone` does to this table when the row `OLD` of `items`
/// goes, while the row of `step_memory` is still there: the item an upload of the file left
/// is kept to be removed.
pub(super) const WHEN_ITEM_GOES: &str = "
    INSERT OR IGNORE INTO mismatched_item_removals (onedrive_item_id, ctag, etag)
        SELECT mismatched_item_id, mismatched_ctag, mismatched_etag FROM step_memory
         WHERE local_item_id = OLD.local_item_id AND mismatched_item_id IS NOT NULL;";

/// Up to `limit` items left to remove, the one whose file's row went first first.
pub(crate) fn list_removals(conn: &Connection, limit: usize) -> Result<Vec<MismatchedItem>, TreeError> {
    let mut statement = conn.prepare_cached("SELECT onedrive_item_id, ctag, etag FROM mismatched_item_removals ORDER BY removal_order LIMIT ?1")?;
    let items = statement.query_map([limit as i64], |row| {
        let (item_id, ctag, etag): (String, Option<String>, Option<String>) = (row.get(0)?, row.get(1)?, row.get(2)?);
        Ok(MismatchedItem::of_answer(OneDriveItemId::new(item_id), ctag.as_deref(), etag.as_deref()))
    })?;
    Ok(items.collect::<Result<Vec<_>, _>>()?)
}

/// The item `item_id` was removed in OneDrive, found gone, or found changed by another: it
/// is not left to remove any more. Whether a row was kept for it.
pub(crate) fn forget_removal(conn: &Connection, item_id: &OneDriveItemId) -> Result<bool, TreeError> {
    Ok(conn.prepare_cached("DELETE FROM mismatched_item_removals WHERE onedrive_item_id = ?1")?.execute([item_id.as_str()])? > 0)
}
