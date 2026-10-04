//! Reading the tree: a row, what is below it, where it is, and the counts.

use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension};

use crate::model::{placed, row_from, skipped, Chain, Counts, Located, Placement, Row, SkipReason, Table, ROW_COLUMNS};
use crate::source::{below_sql, chains_sql, chains_then, Source};
use crate::{TreeError, TreeStore, MAX_CHAIN};

impl TreeStore {
    /// Where `table`'s rows are.
    pub(crate) fn source(&self, table: Table) -> Source {
        match (table, self.whole) {
            (Table::Items, _) => Source::Items,
            (Table::Staging, true) => Source::Whole,
            (Table::Staging, false) => Source::Overlay,
        }
    }

    /// Whether `table` holds no row at all, the root's included.
    pub fn is_empty(&self, table: Table) -> Result<bool, TreeError> {
        let sql = format!("SELECT 1 FROM {} LIMIT 1", self.source(table).rows());
        let any: Option<i64> = self.conn.query_row(&sql, [], |row| row.get(0)).optional()?;
        Ok(any.is_none())
    }

    pub fn get(&self, table: Table, id: &str) -> Result<Option<Row>, TreeError> {
        get_row(&self.conn, self.source(table), id)
    }

    pub fn children(&self, table: Table, id: &str) -> Result<Vec<Row>, TreeError> {
        let sql = format!("SELECT {ROW_COLUMNS} FROM {} WHERE parent_id = ?1 ORDER BY name", self.source(table).rows());
        let mut statement = self.conn.prepare_cached(&sql)?;
        let rows = statement.query_map([id], row_from)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn descendants(&self, table: Table, id: &str) -> Result<Vec<String>, TreeError> {
        descendants_in(&self.conn, self.source(table), id)
    }

    /// Where `id` is: the chain of names from the root. `None` for an item that
    /// is not in the table, or whose chain does not reach the drive's root —
    /// its parent never arrived, or the chain is a cycle.
    pub fn locate(&self, table: Table, id: &str) -> Result<Option<Located>, TreeError> {
        let root = self.root_item_id()?;
        let source = self.source(table);
        let sql = format!(
            "WITH RECURSIVE chain(id, parent_id, name, placement, depth) AS (
                 SELECT id, parent_id, name, placement, 0 FROM {rows} WHERE id = ?1
                 UNION ALL
                 {step})
             SELECT id, parent_id, name, placement FROM chain ORDER BY depth DESC",
            rows = source.rows(),
            step = source.step("p.id, p.parent_id, p.name, p.placement, c.depth + 1", "chain", "p.id = c.parent_id", &format!("c.depth < {MAX_CHAIN}"))
        );
        let mut statement = self.conn.prepare_cached(&sql)?;
        let chain: Vec<(String, Option<String>, String, String)> = statement
            .query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
            .collect::<Result<_, _>>()?;
        let Some((top, top_parent, _, _)) = chain.first() else {
            return Ok(None);
        };
        if top_parent.is_some() || Some(top.as_str()) != root.as_deref() {
            return Ok(None);
        }
        let below = &chain[1..];
        Ok(Some(Located {
            rel: below.iter().map(|(_, _, name, _)| name.as_str()).collect(),
            placed: below.iter().all(|(_, _, _, placement)| Placement::decode(placement) == Placement::Placed),
            depth: below.len(),
        }))
    }

    /// Where each item `start` selects is — a query of `id, parent_id, name,
    /// placement` over `table`'s rows, with `params` from `?2` on (`?1` is
    /// the drive's root) — in one query ([`chains_sql`]). Items whose chain
    /// does not reach the root are left out.
    pub(crate) fn chains(&self, table: Table, start: &str, params: &[&dyn rusqlite::ToSql]) -> Result<Vec<Chain>, TreeError> {
        let Some(root) = self.root_item_id()? else { return Ok(Vec::new()) };
        let sql = chains_sql(self.source(table), start);
        let mut all: Vec<&dyn rusqlite::ToSql> = vec![&root];
        all.extend_from_slice(params);
        let mut statement = self.conn.prepare_cached(&sql)?;
        let chains = statement
            .query_map(all.as_slice(), |r| {
                Ok(Chain { id: r.get(0)?, rel: PathBuf::from(r.get::<_, String>(1)?), above: r.get(2)?, own: r.get(3)? })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(chains)
    }

    /// Every item but the root: what is listed, counted without a walk.
    pub fn listed_count(&self) -> Result<u64, TreeError> {
        let root = self.root_item_id()?.unwrap_or_default();
        let listed: i64 = self.conn.query_row("SELECT count(*) FROM items WHERE id != ?1", [&root], |row| row.get(0))?;
        Ok(listed as u64)
    }

    /// What is listed, placed and skipped in `items`: a walk of the whole
    /// tree, asked for once per cycle that changed it (issue #39).
    pub fn counts(&self) -> Result<Counts, TreeError> {
        let Some(root) = self.root_item_id()? else {
            return Ok(Counts::default());
        };
        let listed = self.listed_count()?;
        let (placed, skipped): (i64, i64) = self.conn.query_row(
            &format!(
                "WITH RECURSIVE placed(id, depth) AS (
                     SELECT ?1, 0
                     UNION ALL
                     SELECT c.id, p.depth + 1 FROM items c JOIN placed p ON c.parent_id = p.id
                      WHERE {own} AND p.depth < {MAX_CHAIN})
                 SELECT (SELECT count(*) - 1 FROM placed),
                        (SELECT count(*) FROM items s JOIN placed p ON s.parent_id = p.id WHERE {not})",
                own = placed("c.placement"),
                not = skipped("s.placement"),
            ),
            [&root],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(Counts { listed, placed: placed as u64, skipped: skipped as u64 })
    }

    /// The skipped items `Skipped()` lists: those whose own folder is in the
    /// folder. What is inside a skipped folder is covered by that folder's
    /// line. One query, from the index of skipped items up to the root
    /// (issue #39).
    pub fn skipped(&self) -> Result<Vec<(PathBuf, SkipReason)>, TreeError> {
        let Some(root) = self.root_item_id()? else {
            return Ok(Vec::new());
        };
        let sql = chains_then(
            Source::Items,
            &format!("SELECT id, parent_id, name, placement FROM items WHERE {}", skipped("placement")),
            "SELECT c.path, i.placement FROM chain c JOIN items i ON i.id = c.start WHERE c.parent_id = ?1 AND c.above",
        );
        let mut statement = self.conn.prepare_cached(&sql)?;
        let mut out = Vec::new();
        for row in statement.query_map([&root], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (path, placement) = row?;
            if let Placement::Skipped(reason) = Placement::decode(&placement) {
                out.push((PathBuf::from(path), reason));
            }
        }
        out.sort();
        Ok(out)
    }
}

/// A row of the tree `source`.
pub(crate) fn get_row(conn: &Connection, source: Source, id: &str) -> Result<Option<Row>, TreeError> {
    let sql = format!("SELECT {ROW_COLUMNS} FROM {} WHERE id = ?1", source.rows());
    Ok(conn.prepare_cached(&sql)?.query_row([id], row_from).optional()?)
}

/// Everything below `id` in the tree `source`.
pub(crate) fn descendants_in(conn: &Connection, source: Source, id: &str) -> Result<Vec<String>, TreeError> {
    let mut statement = conn.prepare_cached(&below_sql(source))?;
    let ids = statement.query_map([id], |row| row.get(0))?.collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}
