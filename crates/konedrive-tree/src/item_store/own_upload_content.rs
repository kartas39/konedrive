//! The content an upload of this folder left in an item of OneDrive that is not the
//! content it sent: the typed fact "OneDrive's content X is this folder's own upload of
//! the local content L".
//!
//! **State owned:** one fact per item, in a table of its own, `own_upload_contents`, by
//! local item id: it follows the item, as the item an upload left does in the step
//! memory, and goes with the item's row by a trigger of its own. Not columns of
//! `step_memory`: that table's row goes whenever the steps remember nothing else of the
//! item, and is dropped and moved with a binding by rules that are not this fact's. It is
//! answered while the row's OneDrive content is `onedrive_content`; the row of the fact
//! stays until it is forgotten or the item's row goes.
//!
//! **Answers for:** the review's S19: without it, an upload into an existing item that
//! ended with another hash reads at the next plan as "changed on both sides", and the
//! user gets a conflict copy of his own file against this folder's failed upload — once
//! per upload if OneDrive rewrites what it is sent. With it the plan can tell the case;
//! the content is then sent again (the owner's decision, `onedrive_steps::content_upload`).
//!
//! **Tested without the rest:**
// test: the_fact_is_answered_while_onedrives_content_is_the_one_the_upload_left
// test: the_fact_is_not_answered_once_a_listing_wrote_another_content
// test: the_fact_goes_with_the_items_row_and_stays_with_the_item_when_its_binding_moves
use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection, OptionalExtension};

use super::item_row::{content_from, content_values, find_item, CONTENT_COLUMNS};
use super::{refuse, ItemReader, ItemStore};
use crate::model::{ContentVersion, ItemKind, LocalItemId};
use crate::TreeError;

/// What an upload of this folder left in OneDrive in place of what it sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnUploadContent {
    /// The content OneDrive has of the item since that upload, as its answer told it.
    pub onedrive_content: ContentVersion,
    /// The local content that was sent.
    pub local_content: ContentVersion,
}

/// The table: one fact per item, by its local item id, each content in the six columns a
/// content has in `items`; and the trigger that takes the fact with the item's row.
pub(crate) fn schema() -> String {
    let content = |of: &str| format!("{of}_size INTEGER NOT NULL, {of}_mtime INTEGER, {of}_mtime_nsec INTEGER, {of}_hash TEXT, {of}_ctag TEXT, {of}_change_count INTEGER");
    format!(
        "CREATE TABLE own_upload_contents (local_item_id INTEGER PRIMARY KEY, {}, {});
         CREATE TRIGGER own_upload_contents_item_gone AFTER DELETE ON items
             BEGIN DELETE FROM own_upload_contents WHERE local_item_id = OLD.local_item_id; END;",
        content("onedrive"),
        content("local")
    )
}

/// The columns of a fact after its key: OneDrive's content, then the local one.
fn fact_columns() -> String {
    ["onedrive", "local"].iter().flat_map(|of| CONTENT_COLUMNS.iter().map(move |column| format!("{of}_{column}"))).collect::<Vec<_>>().join(", ")
}

impl ItemStore<'_> {
    /// An upload of `item`'s local content ended with other content in OneDrive. Written
    /// by the step before it answers `StepRefusal::ContentMismatch`. One transaction; a
    /// fact remembered of the item before is replaced. Refused for an id with no row and
    /// for an item that is no file OneDrive has.
    pub fn remember_own_upload_content(&mut self, item: LocalItemId, upload: &OwnUploadContent) -> Result<(), TreeError> {
        self.write(|tx| {
            let row = find_item(tx, item)?.ok_or_else(|| refuse(format!("item {item} has no row")))?;
            if row.kind != ItemKind::File || row.onedrive_item.is_none() {
                return Err(refuse(format!("item {item} is no file OneDrive has: no upload left a content in it")));
            }
            let mut values: Vec<Value> = vec![item.as_i64().into()];
            values.extend(content_values(Some(&upload.onedrive_content)));
            values.extend(content_values(Some(&upload.local_content)));
            let places = (1..=values.len()).map(|n| format!("?{n}")).collect::<Vec<_>>().join(", ");
            tx.prepare_cached(&format!("INSERT OR REPLACE INTO own_upload_contents (local_item_id, {}) VALUES ({places})", fact_columns()))?.execute(params_from_iter(values))?;
            Ok(())
        })
    }

    /// The fact is done with: the content was sent again and confirmed, or OneDrive's was
    /// taken.
    pub fn forget_own_upload_content(&mut self, item: LocalItemId) -> Result<(), TreeError> {
        self.write(|tx| {
            tx.prepare_cached("DELETE FROM own_upload_contents WHERE local_item_id = ?1")?.execute([item.as_i64()])?;
            Ok(())
        })
    }
}

impl ItemReader<'_> {
    /// The fact for `item`, while its row's OneDrive content is still the one the upload
    /// left. Called by the plan's input and by `SendContent` before it sends again.
    pub fn find_own_upload_content(&self, item: LocalItemId) -> Result<Option<OwnUploadContent>, TreeError> {
        find_own_upload_content(self.conn(), item)
    }
}

/// [`ItemReader::find_own_upload_content`], inside the caller's transaction: asked so by
/// the question and the commit of a directory's removal
/// ([`ItemStore::can_directory_removal_be_sent`]).
pub(crate) fn find_own_upload_content(conn: &Connection, item: LocalItemId) -> Result<Option<OwnUploadContent>, TreeError> {
    let sql = format!("SELECT {} FROM own_upload_contents WHERE local_item_id = ?1", fact_columns());
    let fact = conn.prepare_cached(&sql)?.query_row([item.as_i64()], |row| Ok((content_from(row, 0)?, content_from(row, CONTENT_COLUMNS.len())?))).optional()?;
    let Some((Some(onedrive_content), Some(local_content))) = fact else { return Ok(None) };
    let onedrive_content_now = find_item(conn, item)?.and_then(|row| row.onedrive).and_then(|onedrive| onedrive.content);
    Ok((onedrive_content_now.as_ref() == Some(&onedrive_content)).then_some(OwnUploadContent { onedrive_content, local_content }))
}

#[cfg(test)]
mod tests;
