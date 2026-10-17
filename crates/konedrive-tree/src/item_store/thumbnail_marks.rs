//! Which version of a file's content a thumbnail was last made for, kept by local item id.
//!
//! **State owned:** what tells that a thumbnail in the cache is of the content the row has
//! now: its cTag, its path and its time, and whether a thumbnail was written at all
//! (`is_written`, 0 where OneDrive has none, refuses one or gives bytes that are no image).
//! Written by [`ItemStore::set_thumbnail_made`] and taken by
//! [`ItemStore::forget_thumbnail_marks`] alone.
//! Kept in a table of its own, `thumbnail_marks`, keyed by local item id, which goes with
//! the item's row by a trigger of its own (`thumbnail_marks_item_gone`). The index of the
//! files a thumbnail is made for, `items_thumbnail_type`, holds the rows of
//! [`HAS_THUMBNAIL_TYPE`]. Version 31 of the schema made it in place of
//! `items_image_or_video`, which held images and videos alone, and added `is_written`
//! (`schema::old_store`).
//!
//! **Answers for:** the thumbnail filler's three questions (`desktop/thumbs.rs`): what to
//! make a thumbnail for, that one was made, and which of those written to look for in the
//! cache.
//!
//! **Tested without the rest:**
// test: the_shown_images_and_videos_without_a_thumbnail_of_their_content_are_candidates
// test: the_documents_a_thumbnail_is_made_for_are_candidates_and_plain_text_is_not
// test: a_file_whose_content_or_path_changed_since_its_thumbnail_is_a_candidate_again
// test: a_file_the_folder_does_not_show_is_no_candidate
// test: the_marks_that_stand_are_listed_in_batches_after_an_id
// test: a_forgotten_mark_makes_its_file_a_candidate_again
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

/// A file whose mark stands and says a thumbnail was written: the cache is to hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailMark {
    pub item: LocalItemId,
    /// Where the folder shows it, relative to the root: the cache names a thumbnail by it.
    pub path: PathBuf,
}

/// SQL for "the MIME type OneDrive gives for the row's content is one a thumbnail is made
/// for": an image's or a video's; PDF and PostScript; OpenDocument and Office Open XML;
/// the older Word, Excel and PowerPoint; EPUB. Not plain text: a folder of code would ask
/// for thousands of thumbnails. The index `items_thumbnail_type` holds these rows, and a
/// query finds them by it when it asks in these words.
pub(crate) const HAS_THUMBNAIL_TYPE: &str = concat!(
    "(onedrive_mime LIKE 'image/%' OR onedrive_mime LIKE 'video/%'",
    " OR onedrive_mime IN ('application/pdf', 'application/postscript', 'application/msword', 'application/vnd.ms-excel', 'application/vnd.ms-powerpoint', 'application/epub+zip')",
    " OR onedrive_mime LIKE 'application/vnd.oasis.opendocument.%' OR onedrive_mime LIKE 'application/vnd.openxmlformats-officedocument.%')"
);

/// The column of `thumbnail_marks` that version 31 added, as the schema of a new store has
/// it and as the step from version 30 adds it (`schema::old_store`): one text, so that a
/// store brought from 30 and a store made at 31 have the same column. A mark of version 30
/// is taken for one of a thumbnail written: the filler's next look into the cache tells.
pub(crate) const IS_WRITTEN_COLUMN: &str = "is_written INTEGER NOT NULL DEFAULT 1";

/// The table: one mark per item, by its local item id — the six columns of the content
/// the thumbnail was made for, the path it was made at, kept as a name of `items` is, and
/// whether a thumbnail was written; and the trigger that takes the mark with the item's row.
pub(crate) fn schema() -> String {
    format!(
        "CREATE TABLE thumbnail_marks (
             marked_item_id INTEGER PRIMARY KEY,
             marked_size INTEGER NOT NULL, marked_mtime INTEGER, marked_mtime_nsec INTEGER, marked_hash TEXT, marked_ctag TEXT, marked_change_count INTEGER,
             marked_path NOT NULL, {IS_WRITTEN_COLUMN});
         CREATE TRIGGER thumbnail_marks_item_gone AFTER DELETE ON items
             BEGIN DELETE FROM thumbnail_marks WHERE marked_item_id = OLD.local_item_id; END;"
    )
}

/// SQL for "the mark is of the content OneDrive has now", over a row of `items` joined with
/// its mark.
fn is_of_content() -> String {
    CONTENT_COLUMNS.map(|column| format!("marked_{column} IS onedrive_{column}")).join(" AND ")
}

impl ItemReader<'_> {
    /// The files the folder shows that OneDrive has, whose MIME type is one a thumbnail is
    /// made for ([`HAS_THUMBNAIL_TYPE`]) and that have no thumbnail of the content OneDrive
    /// has now at the path they are at, by local item id, after `after`, at most `limit`.
    /// Called by the thumbnail filler after a listing ended and after steps changed the
    /// folder. The content is compared in the statement; the path is not kept in a row, so
    /// it is read for each such file the folder shows that the call passes, one read per
    /// directory above it: a call that finds few candidates reads all of them.
    pub fn list_thumbnail_candidates(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<ThumbnailCandidate>, TreeError> {
        let sql = format!(
            "SELECT {ITEM_COLUMNS}, marked_path, {is_of_content} FROM items INDEXED BY items_thumbnail_type LEFT JOIN thumbnail_marks ON marked_item_id = local_item_id
             WHERE {HAS_THUMBNAIL_TYPE} AND local_parent_id IS NOT NULL AND onedrive_parent_id IS NOT NULL AND local_item_id > ?1 ORDER BY local_item_id LIMIT ?2",
            is_of_content = is_of_content()
        );
        let (path_at, is_of_content_at) = (ITEM_COLUMNS.split(',').count(), ITEM_COLUMNS.split(',').count() + 1);
        let mut candidates = Vec::new();
        let mut after = after.map_or(0, LocalItemId::as_i64);
        // The files of these types the folder shows, a bounded number at a time, until
        // enough of them have no thumbnail of their content at their path.
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

    /// The marks that stand and say a thumbnail was written: of a file the folder shows
    /// that OneDrive has, of the content OneDrive has now, at the path the file is at; by
    /// local item id, after `after`, at most `limit`. Called by the thumbnail filler when
    /// it looks into the cache. The marks are read by their key and the content compared in
    /// the statement; the path is read for each mark of the content, one read per directory
    /// above its file, as [`list_thumbnail_candidates`](Self::list_thumbnail_candidates)
    /// reads it.
    pub fn list_thumbnail_marks(&self, after: Option<LocalItemId>, limit: usize) -> Result<Vec<ThumbnailMark>, TreeError> {
        let sql = format!(
            "SELECT marked_item_id, marked_path FROM thumbnail_marks JOIN items ON local_item_id = marked_item_id
             WHERE marked_item_id > ?1 AND is_written AND local_parent_id IS NOT NULL AND onedrive_parent_id IS NOT NULL AND {is_of_content}
             ORDER BY marked_item_id LIMIT ?2",
            is_of_content = is_of_content()
        );
        let mut marks = Vec::new();
        let mut after = after.map_or(0, LocalItemId::as_i64);
        // The marks of the content, a bounded number at a time, until enough of them are at
        // the path their file is at.
        while marks.len() < limit {
            let mut statement = self.conn().prepare_cached(&sql)?;
            let rows = statement
                .query_map(params![after, limit as i64], |row| Ok((LocalItemId::new(row.get(0)?), path_from(row.get_ref(1)?))))?
                .collect::<Result<Vec<_>, _>>()?;
            let Some((last, _)) = rows.last() else { break };
            after = last.as_i64();
            let is_last_read = rows.len() < limit;
            for (item, marked_path) in rows {
                if find_path(self.conn(), item)?.as_ref() == Some(&marked_path) {
                    marks.push(ThumbnailMark { item, path: marked_path });
                }
            }
            if is_last_read {
                break;
            }
        }
        marks.truncate(limit);
        Ok(marks)
    }
}

impl ItemStore<'_> {
    /// What OneDrive's answer for the file `item` at `path` settled for `content`: a
    /// thumbnail written into the cache (`is_written`), or none to be had of it. Nothing is
    /// written when the row's OneDrive content is another by now. Called by the thumbnail
    /// filler. One transaction.
    pub fn set_thumbnail_made(&mut self, item: LocalItemId, content: &ContentVersion, path: &std::path::Path, is_written: bool) -> Result<(), TreeError> {
        self.write(|tx| {
            let content_now = find_item(tx, item)?.and_then(|row| row.onedrive).and_then(|onedrive| onedrive.content);
            if content_now.as_ref() != Some(content) {
                return Ok(());
            }
            let mut values: Vec<Value> = vec![item.as_i64().into()];
            values.extend(content_values(Some(content)));
            values.push(path_value(path));
            values.push(is_written.into());
            let columns = CONTENT_COLUMNS.map(|column| format!("marked_{column}")).join(", ");
            tx.prepare_cached(&format!("INSERT OR REPLACE INTO thumbnail_marks (marked_item_id, {columns}, marked_path, is_written) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"))?.execute(params_from_iter(values))?;
            Ok(())
        })
    }

    /// The marks of `items` are gone: their files are candidates again. Called by the
    /// thumbnail filler for the files whose thumbnail the cache does not hold whole. One
    /// transaction; an item with no mark is passed over.
    pub fn forget_thumbnail_marks(&mut self, items: &[LocalItemId]) -> Result<(), TreeError> {
        self.write(|tx| {
            let mut statement = tx.prepare_cached("DELETE FROM thumbnail_marks WHERE marked_item_id = ?1")?;
            for item in items {
                statement.execute([item.as_i64()])?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;
