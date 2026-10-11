//! Which version of a file's content a thumbnail was last made for, kept by local item id.
//!
//! **State owned:** what tells that a thumbnail in the cache is of the content the row has
//! now: today `onedrive_items.thumb_key` (cTag, path, time). Written by
//! [`ItemStore::set_thumbnail_made`] alone.
//! Kept in a table of its own, `thumbnail_marks`, keyed by local item id, which goes with
//! the item's row by a trigger of its own (`thumbnail_marks_item_gone`). The table and
//! the index of the files a thumbnail is made for (`items_image_or_video`) come with the
//! raise of the schema's version that only adds (`schema::old_store`).
//!
//! **Answers for:** the thumbnail filler's two questions (`desktop/thumbs.rs:110`, `:258`).
//!
//! **Tested without the rest:**
// test: the_shown_images_and_videos_without_a_thumbnail_of_their_content_are_candidates
// test: a_file_whose_content_or_path_changed_since_its_thumbnail_is_a_candidate_again
// test: a_file_the_folder_does_not_show_is_no_candidate
use std::path::PathBuf;

use rusqlite::types::{Value, ValueRef};
use rusqlite::{params, params_from_iter};

use super::item_row::{content_values, find_item, item_from, CONTENT_COLUMNS, ITEM_COLUMNS};
use super::{find_path, ItemReader, ItemStore};
use crate::path_column::{path_from, path_value};
use crate::model::{ContentVersion, Item, LocalItemId};
use crate::TreeError;

/// A file a thumbnail is to be made for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailCandidate {
    pub item: Item,
    /// Where the folder shows it, relative to the root: the cache names a thumbnail by it.
    pub path: PathBuf,
}

/// SQL for "the MIME type OneDrive gives for the row's content is an image's or a video's".
/// The index `items_image_or_video` holds these rows, and a query finds them by it when it
/// asks in these words.
pub(crate) const IMAGE_OR_VIDEO: &str = "(onedrive_mime LIKE 'image/%' OR onedrive_mime LIKE 'video/%')";

/// The table: one mark per item, by its local item id — the six columns of the content
/// the thumbnail was made for, and the path it was made at, kept as a name of `items` is;
/// and the trigger that takes the mark with the item's row.
pub(crate) fn schema() -> &'static str {
    "CREATE TABLE thumbnail_marks (
         marked_item_id INTEGER PRIMARY KEY,
         marked_size INTEGER NOT NULL, marked_mtime INTEGER, marked_mtime_nsec INTEGER, marked_hash TEXT, marked_ctag TEXT, marked_change_count INTEGER,
         marked_path NOT NULL);
     CREATE TRIGGER thumbnail_marks_item_gone AFTER DELETE ON items
         BEGIN DELETE FROM thumbnail_marks WHERE marked_item_id = OLD.local_item_id; END;"
}

impl ItemReader<'_> {
    /// The files the folder shows that OneDrive has, whose MIME type is an image's or a
    /// video's and that have no thumbnail of the content OneDrive has now at the path they
    /// are at, by local item id, after `after`, at most `limit`. Called by the thumbnail
    /// filler after a listing ended and after steps changed the folder. The content is
    /// compared in the statement; the path is not kept in a row, so it is read for each
    /// image and video the folder shows that the call passes, one read per directory
    /// above it: a call that finds few candidates reads all of them.
    pub fn list_thumbnail_candidates(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<ThumbnailCandidate>, TreeError> {
        let sql = format!(
            "SELECT {ITEM_COLUMNS}, marked_path, {is_of_content} FROM items INDEXED BY items_image_or_video LEFT JOIN thumbnail_marks ON marked_item_id = local_item_id
             WHERE {IMAGE_OR_VIDEO} AND local_parent_id IS NOT NULL AND onedrive_parent_id IS NOT NULL AND local_item_id > ?1 ORDER BY local_item_id LIMIT ?2",
            is_of_content = CONTENT_COLUMNS.map(|column| format!("marked_{column} IS onedrive_{column}")).join(" AND ")
        );
        let (path_at, is_of_content_at) = (ITEM_COLUMNS.split(',').count(), ITEM_COLUMNS.split(',').count() + 1);
        let mut candidates = Vec::new();
        let mut after = after.map_or(0, LocalItemId::as_i64);
        // The images and videos the folder shows, a bounded number at a time, until enough
        // of them have no thumbnail of their content at their path.
        while candidates.len() < limit {
            let mut statement = self.conn().prepare_cached(&sql)?;
            let rows = statement
                .query_map(params![after, limit as i64], |row| {
                    let marked_path = match row.get_ref(path_at)? {
                        ValueRef::Null => None,
                        path => Some(path_from(path)),
                    };
                    Ok((item_from(row)?, marked_path.filter(|_| row.get::<_, bool>(is_of_content_at).unwrap_or(false))))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let Some((last, _)) = rows.last() else { break };
            after = last.local_item_id.as_i64();
            let is_last_read = rows.len() < limit;
            for (item, path_of_thumbnail) in rows {
                let Some(path) = find_path(self.conn(), item.local_item_id)? else { continue };
                if path_of_thumbnail.as_ref() != Some(&path) {
                    candidates.push(ThumbnailCandidate { item, path });
                }
            }
            if is_last_read {
                break;
            }
        }
        candidates.truncate(limit);
        Ok(candidates)
    }
}

impl ItemStore<'_> {
    /// A thumbnail of `content` was made for the file `item` at `path`; nothing is written
    /// when the row's OneDrive content is another by now. Called by the thumbnail filler.
    /// One transaction.
    pub fn set_thumbnail_made(&mut self, item: LocalItemId, content: &ContentVersion, path: &std::path::Path) -> Result<(), TreeError> {
        self.write(|tx| {
            let content_now = find_item(tx, item)?.and_then(|row| row.onedrive).and_then(|onedrive| onedrive.content);
            if content_now.as_ref() != Some(content) {
                return Ok(());
            }
            let mut values: Vec<Value> = vec![item.as_i64().into()];
            values.extend(content_values(Some(content)));
            values.push(path_value(path));
            let columns = CONTENT_COLUMNS.map(|column| format!("marked_{column}")).join(", ");
            tx.prepare_cached(&format!("INSERT OR REPLACE INTO thumbnail_marks (marked_item_id, {columns}, marked_path) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"))?.execute(params_from_iter(values))?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;
