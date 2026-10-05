//! The new tree a cycle builds, and its swap into `items`.

use rusqlite::OptionalExtension;

use crate::forget::{forget_all_unplaced, forget_subtrees, forget_unplaced};
use crate::model::{placed, upsert, Change, NewTree, Placement, Row, Table, COLUMNS, ROW_COLUMNS};
use crate::query::descendants_in;
use crate::meta::{self, DELTA_LINK, LISTING_NEXT, ROOT_ITEM_ID, STAGING_WHOLE};
use crate::source::Source;
use crate::{TreeError, TreeStore, MAX_CHAIN};

impl TreeStore {
    /// Starts building a new tree: a full listing's from nothing, or a
    /// delta's over `items`, which it leaves as it is until the swap.
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

    /// The new tree becomes `items`, with the link to ask from next time, in
    /// one transaction. A delta's swap writes only the rows it staged and
    /// removes only what it removed; a full listing's replaces
    /// every row. The version a cached thumbnail was made for, the local
    /// inode and the last outbox commit travel along: a row staged without
    /// them keeps what `items` has — but for the inode of a row `items` does
    /// not place, which is no object of a row placed again.
    /// A row the new tree does not place keeps no inode at all, nor does
    /// anything below it (I1, [`crate::forget`]).
    pub fn commit_staging(&mut self, delta_link: &str) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        swap(&tx, self.whole, delta_link)?;
        tx.commit()?;
        self.whole = false;
        Ok(())
    }

    /// One page of a first listing, placed: its entries applied to
    /// `items` as [`stage`](Self::stage) applies them to the new tree, and
    /// `next`, the link to the page after it, kept as where the listing goes
    /// on from — in one transaction, so that a listing stopped anywhere
    /// resumes with every page placed so far and asks for none of them
    /// again. Entries whose folder has not come yet go in too; they are
    /// placed when it comes. A delta staged over `items` is done with: the
    /// new tree is `items` again.
    pub fn commit_page(&mut self, changes: &[Change], next: &str) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        apply(&tx, Source::Items, changes)?;
        // The handles the placement recorded in `staging` for items that
        // were not in `items` yet.
        tx.execute(
            "UPDATE items SET local_handle = s.local_handle FROM staging s
              WHERE s.id = items.id AND items.local_handle IS NULL AND s.local_handle IS NOT NULL",
            [],
        )?;
        if !self.whole {
            tx.execute("DELETE FROM staging", [])?;
            tx.execute("DELETE FROM staging_gone", [])?;
        }
        meta::set(&tx, LISTING_NEXT, Some(next))?;
        tx.commit()?;
        Ok(())
    }

    /// Ids that differ between `items` and the new tree — added, removed or
    /// changed in any column but the local ones. For a delta, read from what
    /// it staged alone.
    pub fn changed_ids(&self) -> Result<Vec<String>, TreeError> {
        let sql = if self.whole {
            format!(
                "SELECT id FROM (SELECT {ROW_COLUMNS} FROM staging EXCEPT SELECT {ROW_COLUMNS} FROM items)
                 UNION
                 SELECT id FROM (SELECT {ROW_COLUMNS} FROM items EXCEPT SELECT {ROW_COLUMNS} FROM staging)"
            )
        } else {
            "SELECT s.id FROM staging s LEFT JOIN items i ON i.id = s.id
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
/// transaction: the new tree (`whole`: a full listing's) becomes `items`, and
/// `delta_link` the link to ask from next time. The caller commits, and only
/// then takes the store's new tree for a delta's again.
pub(super) fn swap(tx: &rusqlite::Transaction<'_>, whole: bool, delta_link: &str) -> Result<(), TreeError> {
    if whole {
        tx.execute(
            "UPDATE staging SET thumb_key = (SELECT i.thumb_key FROM items i WHERE i.id = staging.id)
              WHERE thumb_key IS NULL",
            [],
        )?;
        // A row that turns placed again takes no object from `items`:
        // whatever was there when it stopped being placed is gone.
        tx.execute(
            &format!(
                "UPDATE staging SET local_handle = (SELECT i.local_handle FROM items i WHERE i.id = staging.id AND {was})
              WHERE local_handle IS NULL",
                was = placed("i.placement"),
            ),
            [],
        )?;
        tx.execute(
            "UPDATE staging SET local_seq = MAX(local_seq, COALESCE((SELECT i.local_seq FROM items i WHERE i.id = staging.id), 0))",
            [],
        )?;
        tx.execute("DELETE FROM items", [])?;
        tx.execute(&format!("INSERT INTO items ({COLUMNS}) SELECT {COLUMNS} FROM staging"), [])?;
        tx.execute("DELETE FROM staging", [])?;
        forget_all_unplaced(tx)?;
    } else {
        tx.execute(
            &format!(
                "INSERT INTO items ({COLUMNS})
                 SELECT s.id, s.parent_id, s.name, s.kind, s.size, s.mtime, s.etag, s.ctag, s.quickxor, s.mime, s.placement,
                        COALESCE(s.thumb_key, i.thumb_key), COALESCE(s.local_handle, CASE WHEN {was} THEN i.local_handle END),
                        MAX(s.local_seq, COALESCE(i.local_seq, 0))
                   FROM staging s LEFT JOIN items i ON i.id = s.id WHERE true
                 ON CONFLICT(id) DO UPDATE SET
                   parent_id = excluded.parent_id, name = excluded.name, kind = excluded.kind,
                   size = excluded.size, mtime = excluded.mtime, etag = excluded.etag, ctag = excluded.ctag,
                   quickxor = excluded.quickxor, mime = excluded.mime, placement = excluded.placement,
                   thumb_key = excluded.thumb_key, local_handle = excluded.local_handle, local_seq = excluded.local_seq",
                was = placed("i.placement"),
            ),
            [],
        )?;
        tx.execute("DELETE FROM items WHERE id IN (SELECT id FROM staging_gone)", [])?;
        // What the delta wrote, and the base does not place now.
        let written: Vec<String> = tx.prepare_cached("SELECT id FROM staging")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        tx.execute("DELETE FROM staging", [])?;
        forget_unplaced(tx, written.iter().map(String::as_str))?;
    }
    tx.execute("DELETE FROM staging_gone", [])?;
    meta::set(tx, STAGING_WHOLE, None)?;
    meta::set(tx, DELTA_LINK, Some(delta_link))?;
    // A first listing placed page by page ends here too.
    meta::set(tx, LISTING_NEXT, None)?;
    Ok(())
}

/// Delta entries applied to the tree `source`, in order (see
/// [`TreeStore::stage`]). Over `items` (a delta staged), a row written is
/// first copied from `items`, local columns and all, and then changed; a row
/// removed is noted in `staging_gone` when `items` has it. Written into
/// `items` itself, a row the base does not place then keeps no local
/// object, nor does anything below it (I1, [`crate::forget`]).
pub(crate) fn apply(tx: &rusqlite::Transaction<'_>, source: Source, changes: &[Change]) -> Result<(), TreeError> {
    apply_each(tx, source, changes)?;
    if source == Source::Items {
        forget_unplaced(tx, changes.iter().filter(|c| !matches!(c, Change::Delete(_))).map(Change::id))?;
    }
    Ok(())
}

fn apply_each(tx: &rusqlite::Transaction<'_>, source: Source, changes: &[Change]) -> Result<(), TreeError> {
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
            }
            Change::Upsert(row) => write(tx, source, row)?,
            Change::Delete(id) => match source {
                Source::Items | Source::Whole => {
                    let t = if source == Source::Items { "items" } else { "staging" };
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
                    let mut note = tx.prepare_cached("INSERT OR IGNORE INTO staging_gone (id) SELECT id FROM items WHERE id = ?1")?;
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
        Source::Items => {
            let was = placement_in_items(tx, &row.id)?;
            upsert(tx, Table::Items, row)?;
            if turns_placed(was, row) {
                forget_subtrees(tx, std::slice::from_ref(&row.id), true, &[])?;
            }
        }
        Source::Whole => {
            upsert(tx, Table::Staging, row)?;
            // What is below it, by `items`: the swap would give it back.
            if turns_placed(placement_in_items(tx, &row.id)?, row) {
                forget_subtrees(tx, std::slice::from_ref(&row.id), false, &[])?;
            }
        }
        Source::Overlay => {
            tx.prepare_cached(&format!("INSERT OR IGNORE INTO staging ({COLUMNS}) SELECT {COLUMNS} FROM items WHERE id = ?1"))?
                .execute([&row.id])?;
            tx.prepare_cached("DELETE FROM staging_gone WHERE id = ?1")?.execute([&row.id])?;
            upsert(tx, Table::Staging, row)?;
            if turns_placed(placement_in_items(tx, &row.id)?, row) {
                // The row itself, and everything below it in both tables:
                // the rows below that the delta does not stage show through
                // from `items` at the swap.
                forget_subtrees(tx, std::slice::from_ref(&row.id), true, &[])?;
            }
        }
    }
    Ok(())
}

/// Item `id`'s own placement in `items`, if it has a row there.
fn placement_in_items(tx: &rusqlite::Transaction<'_>, id: &str) -> Result<Option<Placement>, TreeError> {
    let placement: Option<String> = tx
        .prepare_cached("SELECT placement FROM items WHERE id = ?1")?
        .query_row([id], |r| r.get(0))
        .optional()?;
    Ok(placement.as_deref().map(Placement::decode))
}

/// Whether `row` turns placed again over a row of `items` that was not
/// (`was`). Such a row, and every row below it, carries no local object:
/// what was on disk when it stopped being placed was taken
/// off, and its placement records the objects it is placed as. Forgotten
/// as it is staged, so that the placement that follows records them anew —
/// and so that what a cycle that failed before its swap recorded for them,
/// on rows the base did not place yet, is not taken for theirs.
fn turns_placed(was: Option<Placement>, row: &Row) -> bool {
    row.placement == Placement::Placed && was.is_some_and(|was| was != Placement::Placed)
}
