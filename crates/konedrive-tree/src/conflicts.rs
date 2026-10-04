//! The local versions kept, on record.

use rusqlite::params;

use crate::model::column;
use crate::{TreeError, TreeStore};

/// A local version kept because the file changed or was removed in
/// OneDrive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRow {
    pub at: i64,
    /// Where it was, as a full path.
    pub original: String,
    /// Where it is now, as a full path.
    pub rescued: String,
    pub kind: ConflictKind,
}

/// How a local version was kept (`Conflicts.List()`'s fourth field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// Moved out of the way, out of the folder (the read phase's rescue).
    Rescued,
    /// Kept as a copy beside the original, in a read-write folder, and
    /// uploaded (`docs/design/writes.md` §7).
    Copy,
}

impl ConflictKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rescued => "rescued",
            Self::Copy => "copy",
        }
    }

    fn parse(value: &str) -> Self {
        if value == Self::Copy.as_str() {
            Self::Copy
        } else {
            Self::Rescued
        }
    }
}

/// What a query of conflicts selects, in the order [`conflict_row`] reads.
const COLUMNS: &str = "at, original, rescued, kind";

fn conflict_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConflictRow> {
    Ok(ConflictRow {
        at: row.get(const { column(COLUMNS, "at") })?,
        original: row.get(const { column(COLUMNS, "original") })?,
        rescued: row.get(const { column(COLUMNS, "rescued") })?,
        kind: ConflictKind::parse(&row.get::<_, String>(const { column(COLUMNS, "kind") })?),
    })
}

impl TreeStore {
    /// Records local versions a reconcile kept: `(at, original, rescued,
    /// kind)`, full paths. A path kept at again replaces its row.
    pub fn add_conflicts(&mut self, conflicts: &[ConflictRow]) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        for c in conflicts {
            tx.execute(
                "INSERT INTO conflicts (rescued, at, original, kind) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(rescued) DO UPDATE SET at = excluded.at, original = excluded.original, kind = excluded.kind",
                params![c.rescued, c.at, c.original, c.kind.as_str()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every recorded conflict, newest first.
    pub fn conflicts(&self) -> Result<Vec<ConflictRow>, TreeError> {
        let mut statement =
            self.conn.prepare(&format!("SELECT {COLUMNS} FROM conflicts ORDER BY at DESC, rescued"))?;
        let rows = statement.query_map([], conflict_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Up to `limit` conflicts whose rescued path sorts after `after`, in
    /// that order.
    pub fn conflicts_after(&self, after: &str, limit: usize) -> Result<Vec<ConflictRow>, TreeError> {
        let mut statement =
            self.conn.prepare_cached(&format!("SELECT {COLUMNS} FROM conflicts WHERE rescued > ?1 ORDER BY rescued LIMIT ?2"))?;
        let rows = statement.query_map(params![after, limit as i64], conflict_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// How many conflicts are on record.
    pub fn conflict_count(&self) -> Result<u64, TreeError> {
        Ok(self.conn.query_row("SELECT count(*) FROM conflicts", [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Deletes the conflicts whose rescued files are `rescued`, in one
    /// transaction; how many there were.
    pub fn remove_conflicts(&mut self, rescued: &[String]) -> Result<usize, TreeError> {
        let tx = self.conn.transaction()?;
        let mut removed = 0;
        {
            let mut delete = tx.prepare_cached("DELETE FROM conflicts WHERE rescued = ?1")?;
            for path in rescued {
                removed += delete.execute([path])?;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Deletes the conflict whose rescued file is `rescued`; whether there
    /// was one.
    pub fn remove_conflict(&self, rescued: &str) -> Result<bool, TreeError> {
        Ok(self.conn.execute("DELETE FROM conflicts WHERE rescued = ?1", [rescued])? > 0)
    }
}
