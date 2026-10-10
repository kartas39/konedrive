//! The new tree a cycle builds, and its swap into `onedrive_items`.

use crate::model::{upsert, Change, NewTree, Row, Table, COLUMNS, ROW_COLUMNS};
use crate::query::descendants_in;
use crate::meta::{self, DELTA_LINK, LISTING_NEXT, ROOT_ITEM_ID, STAGING_WHOLE};
use crate::source::Source;
use crate::{TreeError, TreeStore, MAX_CHAIN};

impl TreeStore {
    /// Starts building a new tree: a full listing's from nothing, or a
    /// delta's over `onedrive_items`, which it leaves as it is until the swap.
    pub fn begin_staging(&mut self, tree: NewTree) -> Result<(), TreeError> {
        let whole = tree == NewTree::Whole;
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM staging", [])?;
        tx.execute("DELETE FROM staging_gone", [])?;
        meta::set(&tx, STAGING_WHOLE, whole.then_some("1"))?;
        tx.commit()?;
        self.whole = whole;
        Ok(())
    }

    /// Applies delta entries to the new tree, in order. Deleting a folder
    /// takes whatever is still inside it in the new tree — an item the feed
    /// moved out before, or moves out after, survives (the order of a batch
    /// is not the order of events).
    pub fn stage(&mut self, changes: &[Change]) -> Result<(), TreeError> {
        let source = self.source(Table::Staging);
        let tx = self.conn.transaction()?;
        apply(&tx, source, changes)?;
        tx.commit()?;
        Ok(())
    }

    /// The new tree becomes `onedrive_items`, with the link to ask from next time, in
    /// one transaction. A delta's swap writes only the rows it staged and
    /// removes only what it removed; a full listing's replaces
    /// every row. The version a cached thumbnail was made for and the last
    /// outbox commit travel along: a row staged without them keeps what
    /// `onedrive_items` has.
    pub fn commit_staging(&mut self, delta_link: &str) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        swap(&tx, self.whole, delta_link)?;
        tx.commit()?;
        self.whole = false;
        Ok(())
    }

    /// One page of a first listing, placed: its entries applied to
    /// `onedrive_items` as [`stage`](Self::stage) applies them to the new tree, and
    /// `next`, the link to the page after it, kept as where the listing goes
    /// on from — in one transaction, so that a listing stopped anywhere
    /// resumes with every page placed so far and asks for none of them
    /// again. Entries whose folder has not come yet go in too; they are
    /// placed when it comes. A delta staged over `onedrive_items` is done with: the
    /// new tree is `onedrive_items` again.
    pub fn commit_page(&mut self, changes: &[Change], next: &str) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        apply(&tx, Source::OneDriveItems, changes)?;
        if !self.whole {
            tx.execute("DELETE FROM staging", [])?;
            tx.execute("DELETE FROM staging_gone", [])?;
        }
        meta::set(&tx, LISTING_NEXT, Some(next))?;
        tx.commit()?;
        Ok(())
    }

    /// Ids that differ between `onedrive_items` and the new tree — added, removed or
    /// changed in any column but the local ones. For a delta, read from what
    /// it staged alone.
    pub fn changed_ids(&self) -> Result<Vec<String>, TreeError> {
        let sql = if self.whole {
            format!(
                "SELECT id FROM (SELECT {ROW_COLUMNS} FROM staging EXCEPT SELECT {ROW_COLUMNS} FROM onedrive_items)
                 UNION
                 SELECT id FROM (SELECT {ROW_COLUMNS} FROM onedrive_items EXCEPT SELECT {ROW_COLUMNS} FROM staging)"
            )
        } else {
            "SELECT s.id FROM staging s LEFT JOIN onedrive_items i ON i.id = s.id
              WHERE i.id IS NULL OR s.parent_id IS NOT i.parent_id OR s.name IS NOT i.name OR s.kind IS NOT i.kind
                 OR s.size IS NOT i.size OR s.mtime IS NOT i.mtime OR s.etag IS NOT i.etag OR s.ctag IS NOT i.ctag
                 OR s.quickxor IS NOT i.quickxor OR s.mime IS NOT i.mime OR s.placement IS NOT i.placement
             UNION
             SELECT id FROM staging_gone"
                .to_owned()
        };
        let mut statement = self.conn.prepare(&sql)?;
        let ids = statement.query_map([], |row| row.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }
}

/// The swap of [`TreeStore::commit_staging`], inside the caller's
/// transaction: the new tree (`whole`: a full listing's) becomes `onedrive_items`, and
/// `delta_link` the link to ask from next time. The caller commits, and only
/// then takes the store's new tree for a delta's again.
pub(super) fn swap(tx: &rusqlite::Transaction<'_>, whole: bool, delta_link: &str) -> Result<(), TreeError> {
    if whole {
        tx.execute(
            "UPDATE staging SET thumb_key = (SELECT i.thumb_key FROM onedrive_items i WHERE i.id = staging.id)
              WHERE thumb_key IS NULL",
            [],
        )?;
        tx.execute(
            "UPDATE staging SET local_seq = MAX(local_seq, COALESCE((SELECT i.local_seq FROM onedrive_items i WHERE i.id = staging.id), 0))",
            [],
        )?;
        tx.execute("DELETE FROM onedrive_items", [])?;
        tx.execute(&format!("INSERT INTO onedrive_items ({COLUMNS}) SELECT {COLUMNS} FROM staging"), [])?;
        tx.execute("DELETE FROM staging", [])?;
    } else {
        tx.execute(
            &format!(
                "INSERT INTO onedrive_items ({COLUMNS})
                 SELECT s.id, s.parent_id, s.name, s.kind, s.size, s.mtime, s.etag, s.ctag, s.quickxor, s.mime, s.placement,
                        COALESCE(s.thumb_key, i.thumb_key), MAX(s.local_seq, COALESCE(i.local_seq, 0))
                   FROM staging s LEFT JOIN onedrive_items i ON i.id = s.id WHERE true
                 ON CONFLICT(id) DO UPDATE SET
                   parent_id = excluded.parent_id, name = excluded.name, kind = excluded.kind,
                   size = excluded.size, mtime = excluded.mtime, etag = excluded.etag, ctag = excluded.ctag,
                   quickxor = excluded.quickxor, mime = excluded.mime, placement = excluded.placement,
                   thumb_key = excluded.thumb_key, local_seq = excluded.local_seq"
            ),
            [],
        )?;
        tx.execute("DELETE FROM onedrive_items WHERE id IN (SELECT id FROM staging_gone)", [])?;
        tx.execute("DELETE FROM staging", [])?;
    }
    tx.execute("DELETE FROM staging_gone", [])?;
    meta::set(tx, STAGING_WHOLE, None)?;
    meta::set(tx, DELTA_LINK, Some(delta_link))?;
    // A first listing placed page by page ends here too.
    meta::set(tx, LISTING_NEXT, None)?;
    Ok(())
}

/// Delta entries applied to the tree `source`, in order (see
/// [`TreeStore::stage`]). Over `onedrive_items` (a delta staged), a row written is
/// first copied from `onedrive_items`, local columns and all, and then changed; a row
/// removed is noted in `staging_gone` when `onedrive_items` has it.
pub(crate) fn apply(tx: &rusqlite::Transaction<'_>, source: Source, changes: &[Change]) -> Result<(), TreeError> {
    for change in changes {
        match change {
            Change::Root(row) => {
                write(tx, source, row)?;
                // Written with the row, as soon as it is staged, rather
                // than deferred to `commit_staging` — the materializer
                // reconciles `staging` against the folder before the
                // commit and needs the drive's root id then, and a
                // drive's root id never changes. Do not move this.
                meta::set(tx, ROOT_ITEM_ID, Some(&row.id))?;
                // The root of the folder is that item, in the drive the store knows.
                crate::item_store::bind_root_once_known(tx)?;
            }
            Change::Upsert(row) => write(tx, source, row)?,
            Change::Delete(id) => match source {
                Source::OneDriveItems | Source::Whole => {
                    let t = if source == Source::OneDriveItems { "onedrive_items" } else { "staging" };
                    tx.execute(
                        &format!(
                            "WITH RECURSIVE below(id, depth) AS (
                                 SELECT id, 1 FROM {t} WHERE parent_id = ?1
                                 UNION ALL
                                 SELECT c.id, b.depth + 1 FROM {t} c JOIN below b ON c.parent_id = b.id
                                  WHERE b.depth < {MAX_CHAIN})
                             DELETE FROM {t} WHERE id IN (SELECT id FROM below)"
                        ),
                        [id],
                    )?;
                    tx.execute(&format!("DELETE FROM {t} WHERE id = ?1"), [id])?;
                }
                Source::Overlay => {
                    let mut gone = descendants_in(tx, source, id)?;
                    gone.push(id.clone());
                    let mut unstage = tx.prepare_cached("DELETE FROM staging WHERE id = ?1")?;
                    let mut note = tx.prepare_cached("INSERT OR IGNORE INTO staging_gone (id) SELECT id FROM onedrive_items WHERE id = ?1")?;
                    for id in &gone {
                        unstage.execute([id])?;
                        note.execute([id])?;
                    }
                }
            },
        }
    }
    Ok(())
}

/// `row` written into the tree `source` (see [`apply`]).
fn write(tx: &rusqlite::Transaction<'_>, source: Source, row: &Row) -> Result<(), TreeError> {
    match source {
        Source::OneDriveItems => drop(upsert(tx, Table::OneDriveItems, row)?),
        Source::Whole => drop(upsert(tx, Table::Staging, row)?),
        Source::Overlay => {
            tx.prepare_cached(&format!("INSERT OR IGNORE INTO staging ({COLUMNS}) SELECT {COLUMNS} FROM onedrive_items WHERE id = ?1"))?
                .execute([&row.id])?;
            tx.prepare_cached("DELETE FROM staging_gone WHERE id = ?1")?.execute([&row.id])?;
            upsert(tx, Table::Staging, row)?;
        }
    }
    Ok(())
}
