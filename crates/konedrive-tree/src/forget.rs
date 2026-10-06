//! Forgetting local objects: the one walk that forgets a subtree, and the
//! rule it keeps.
//!
//! **I1. The base records a local object only for an item it places.** A
//! record of an object that is not there is what an examination proves a
//! delete with (`docs/design/writes.md` §4.2 rule 7), so a row keeps none once the
//! base does not place it — the item itself, or a folder above it:
//!
//! - whatever writes a row into `items` that the base then does not place,
//!   forgets it and everything below it in the same transaction
//!   ([`forget_unplaced`], [`forget_all_unplaced`]): the swap, a page of a
//!   first listing, what waited and is applied, an outbox commit;
//! - a row removed takes its record with it;
//! - a row that turns placed again takes none over (`staging::turns_placed`):
//!   a cycle that placed it and failed before its swap left one on a row the
//!   base does not place yet.

use std::collections::HashMap;

use konedrive_fs::handle::FileHandle;
use rusqlite::OptionalExtension;

use crate::meta::{self, ROOT_ITEM_ID};
use crate::model::{placed, Placement};
use crate::{TreeError, MAX_CHAIN};

/// The subtrees at `roots` — `roots` themselves when `with_roots`, and
/// everything `items` or `staging` has below them — forget their local
/// objects in both tables, and so does every row recording one of
/// `handles`: seeded from a temporary table, one statement per
/// table, whatever the number of roots.
pub(crate) fn forget_subtrees(tx: &rusqlite::Transaction<'_>, roots: &[String], with_roots: bool, handles: &[FileHandle]) -> Result<(), TreeError> {
    if !handles.is_empty() {
        tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS forget_handles (handle BLOB PRIMARY KEY); DELETE FROM forget_handles;")?;
        {
            let mut handle = tx.prepare_cached("INSERT OR IGNORE INTO forget_handles (handle) VALUES (?1)")?;
            for h in handles {
                handle.execute([h.encode()])?;
            }
        }
        for table in ["items", "staging"] {
            tx.execute(&format!("UPDATE {table} SET local_handle = NULL WHERE local_handle IN (SELECT handle FROM forget_handles)"), [])?;
        }
        tx.execute_batch("DELETE FROM forget_handles;")?;
    }
    if roots.is_empty() {
        return Ok(());
    }
    // Nothing below a single root (a file) and nothing of its own to forget:
    // no recursive query at all, through the parent indexes.
    if !with_roots && roots.len() == 1 {
        let below: bool = tx
            .prepare_cached("SELECT EXISTS (SELECT 1 FROM items WHERE parent_id = ?1) OR EXISTS (SELECT 1 FROM staging WHERE parent_id = ?1)")?
            .query_row([&roots[0]], |r| r.get(0))?;
        if !below {
            return Ok(());
        }
    }
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS forget_roots (id TEXT PRIMARY KEY);
         CREATE TEMP TABLE IF NOT EXISTS forget_below (id TEXT PRIMARY KEY);
         DELETE FROM forget_roots; DELETE FROM forget_below;",
    )?;
    {
        let mut root = tx.prepare_cached("INSERT OR IGNORE INTO forget_roots (id) VALUES (?1)")?;
        for id in roots {
            root.execute([id])?;
        }
    }
    // The set is built once, each step through a table's parent index, and
    // then forgotten in both tables by primary key.
    let from = if with_roots { 0 } else { 1 };
    tx.execute(
        &format!(
            "WITH RECURSIVE below(id, depth) AS (
                 SELECT id, 0 FROM forget_roots
                 UNION SELECT c.id, b.depth + 1 FROM below b JOIN items c ON c.parent_id = b.id WHERE b.depth < {MAX_CHAIN}
                 UNION SELECT c.id, b.depth + 1 FROM below b JOIN staging c ON c.parent_id = b.id WHERE b.depth < {MAX_CHAIN})
             INSERT OR IGNORE INTO forget_below (id) SELECT id FROM below WHERE depth >= {from}"
        ),
        [],
    )?;
    for table in ["items", "staging"] {
        tx.execute(&format!("UPDATE {table} SET local_handle = NULL WHERE local_handle IS NOT NULL AND id IN (SELECT id FROM forget_below)"), [])?;
    }
    tx.execute_batch("DELETE FROM forget_roots; DELETE FROM forget_below;")?;
    Ok(())
}

/// Whether the base places rows of `items`: the row itself and every folder
/// above it, up to the drive's root. A point lookup for each folder on the
/// way up, asked once however many rows share it.
pub(crate) struct Placing {
    known: HashMap<String, bool>,
}

impl Placing {
    /// `None` while the drive's root has not come: nothing is placed yet.
    pub(crate) fn new(tx: &rusqlite::Transaction<'_>) -> Result<Option<Self>, TreeError> {
        Ok(meta::get(tx, ROOT_ITEM_ID)?.map(|root| Self { known: HashMap::from([(root, true)]) }))
    }

    /// Whether the base places row `id`; `None` when `items` has no such row.
    pub(crate) fn of(&mut self, tx: &rusqlite::Transaction<'_>, id: &str) -> Result<Option<bool>, TreeError> {
        let mut row_of = tx.prepare_cached("SELECT parent_id, placement FROM items WHERE id = ?1")?;
        let mut chain: Vec<String> = Vec::new();
        let mut at = id.to_owned();
        let placed = loop {
            if let Some(&placed) = self.known.get(&at) {
                break placed;
            }
            let row: Option<(Option<String>, String)> = row_of.query_row([&at], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let Some((parent, placement)) = row else {
                if chain.is_empty() {
                    return Ok(None);
                }
                // A folder that never came places nothing.
                break false;
            };
            chain.push(at);
            match parent {
                Some(parent) if Placement::decode(&placement) == Placement::Placed && chain.len() <= MAX_CHAIN => at = parent,
                _ => break false,
            }
        };
        // Below a folder the base places, each row on the way was read as
        // placed itself; below any other, none is placed.
        self.known.extend(chain.into_iter().map(|id| (id, placed)));
        Ok(Some(placed))
    }
}

/// Whether the base places row `id`, as `items` is now.
pub(crate) fn base_places(tx: &rusqlite::Transaction<'_>, id: &str) -> Result<bool, TreeError> {
    let Some(mut placing) = Placing::new(tx)? else { return Ok(false) };
    Ok(placing.of(tx, id)?.unwrap_or(false))
}

/// I1 for the rows `ids`, just written into `items`: each one the base does
/// not place forgets its local object, and so does everything below it, in
/// both tables.
pub(crate) fn forget_unplaced<'a>(tx: &rusqlite::Transaction<'_>, ids: impl IntoIterator<Item = &'a str>) -> Result<(), TreeError> {
    let Some(mut placing) = Placing::new(tx)? else { return Ok(()) };
    let mut roots = Vec::new();
    for id in ids {
        if placing.of(tx, id)? == Some(false) {
            roots.push(id.to_owned());
        }
    }
    forget_subtrees(tx, &roots, true, &[])
}

/// I1 for the whole of `items`, after a full listing took its place: every
/// row the base does not place forgets its local object. One walk down from
/// the root.
pub(crate) fn forget_all_unplaced(tx: &rusqlite::Transaction<'_>) -> Result<(), TreeError> {
    let Some(root) = meta::get(tx, ROOT_ITEM_ID)? else { return Ok(()) };
    tx.execute(
        &format!(
            "WITH RECURSIVE placed(id, depth) AS (
                 SELECT ?1, 0
                 UNION ALL
                 SELECT c.id, p.depth + 1 FROM items c JOIN placed p ON c.parent_id = p.id
                  WHERE {own} AND p.depth < {MAX_CHAIN})
             UPDATE items SET local_handle = NULL WHERE local_handle IS NOT NULL AND id NOT IN (SELECT id FROM placed)",
            own = placed("c.placement"),
        ),
        [&root],
    )?;
    Ok(())
}
