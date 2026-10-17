//! The copies kept of local versions, on record.

use rusqlite::params;

use crate::model::column;
use crate::{TreeError, TreeStore};

/// A local version of a file changed on both sides, kept as a copy beside the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRow {
    pub at: i64,
    /// Where the file is, as a full path.
    pub original: String,
    /// Where the copy kept of the local version is, as a full path.
    pub kept_copy: String,
}

/// What a query of conflicts selects, in the order [`conflict_row`] reads.
const COLUMNS: &str = "at, original, kept_copy";

fn conflict_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConflictRow> {
    Ok(ConflictRow {
        at: row.get(const { column(COLUMNS, "at") })?,
        original: row.get(const { column(COLUMNS, "original") })?,
        kept_copy: row.get(const { column(COLUMNS, "kept_copy") })?,
    })
}

impl TreeStore {
    /// Records the copies kept: `(at, original, kept_copy)`, full paths. A copy kept at
    /// a path again replaces its row.
    pub fn add_conflicts(&mut self, conflicts: &[ConflictRow]) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        for c in conflicts {
            tx.execute(
                "INSERT INTO conflicts (kept_copy, at, original) VALUES (?1, ?2, ?3)
                 ON CONFLICT(kept_copy) DO UPDATE SET at = excluded.at, original = excluded.original",
                params![c.kept_copy, c.at, c.original],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every recorded conflict, newest first.
    pub fn conflicts(&self) -> Result<Vec<ConflictRow>, TreeError> {
        let mut statement =
            self.conn.prepare(&format!("SELECT {COLUMNS} FROM conflicts ORDER BY at DESC, kept_copy"))?;
        let rows = statement.query_map([], conflict_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Up to `limit` conflicts whose kept copy's path sorts after `after`, in
    /// that order.
    pub fn conflicts_after(&self, after: &str, limit: usize) -> Result<Vec<ConflictRow>, TreeError> {
        let mut statement =
            self.conn.prepare_cached(&format!("SELECT {COLUMNS} FROM conflicts WHERE kept_copy > ?1 ORDER BY kept_copy LIMIT ?2"))?;
        let rows = statement.query_map(params![after, limit as i64], conflict_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// How many conflicts are on record.
    pub fn conflict_count(&self) -> Result<u64, TreeError> {
        Ok(self.conn.query_row("SELECT count(*) FROM conflicts", [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Deletes the conflicts whose kept copies are at `kept_copies`, in one
    /// transaction; how many there were.
    pub fn remove_conflicts(&mut self, kept_copies: &[String]) -> Result<usize, TreeError> {
        let tx = self.conn.transaction()?;
        let mut removed = 0;
        {
            let mut delete = tx.prepare_cached("DELETE FROM conflicts WHERE kept_copy = ?1")?;
            for path in kept_copies {
                removed += delete.execute([path])?;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Deletes the conflict whose kept copy is at `kept_copy`; whether there
    /// was one.
    pub fn remove_conflict(&self, kept_copy: &str) -> Result<bool, TreeError> {
        Ok(self.conn.execute("DELETE FROM conflicts WHERE kept_copy = ?1", [kept_copy])? > 0)
    }
}
