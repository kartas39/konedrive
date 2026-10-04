//! Forgetting local objects: the rows of a subtree, in both tables.

use konedrive_fs::handle::FileHandle;

use crate::{TreeError, MAX_CHAIN};

/// The subtrees at `roots` — `roots` themselves when `with_roots`, and
/// everything `items` or `staging` has below them — forget their local
/// objects in both tables, and so does every row recording one of
/// `handles` (issue #104): seeded from a temporary table, one statement per
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
