//! Reading the tree: a row, what is below it, where it is, and the counts.

use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension};

use konedrive_reason::WaitsFor;

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
        self.locate_below(root.as_deref(), table, id)
    }

    /// [`Self::locate`] for one who has read the drive's root id already.
    pub(crate) fn locate_below(&self, root: Option<&str>, table: Table, id: &str) -> Result<Option<Located>, TreeError> {
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
        if top_parent.is_some() || Some(top.as_str()) != root {
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

    /// What is listed and placed in `items`, and what [`skipped`](Self::skipped)
    /// lists: a walk of the whole tree, asked for once per cycle that
    /// changed it.
    pub fn counts(&self) -> Result<Counts, TreeError> {
        let Some(root) = self.root_item_id()? else {
            return Ok(Counts::default());
        };
        let listed = self.listed_count()?;
        let placed: i64 = self.conn.query_row(
            &format!(
                "WITH RECURSIVE placed(id, depth) AS (
                     SELECT ?1, 0
                     UNION ALL
                     SELECT c.id, p.depth + 1 FROM items c JOIN placed p ON c.parent_id = p.id
                      WHERE {own} AND p.depth < {MAX_CHAIN})
                 SELECT count(*) - 1 FROM placed",
                own = placed("c.placement"),
            ),
            [&root],
            |row| row.get(0),
        )?;
        // What the list lists, line for line.
        let skipped = self.skipped()?.len() as u64;
        Ok(Counts { listed, placed: placed as u64, skipped })
    }

    /// The skipped items `Skipped()` lists: those OneDrive has and the folder
    /// cannot hold, whose own folder is in the folder. What is inside a
    /// skipped folder is covered by that folder's line. One that is still
    /// here — the base places it, and OneDrive's row of it waits in
    /// `deferred` until nothing in it waits and the disk can let it go — is
    /// listed as OneDrive has it, from the cycle that learnt of it, with
    /// what it waits for ([`Skipped::waits`]). So is one that is still here
    /// while OneDrive has it below a folder that is not placed, on a line of
    /// its own beside that folder's. One query, from the index of skipped
    /// items and from what waits, up to the root, and a look at
    /// each of the other changes that wait.
    pub fn skipped(&self) -> Result<Vec<Skipped>, TreeError> {
        let Some(root) = self.root_item_id()? else {
            return Ok(Vec::new());
        };
        let sql = chains_then(
            Source::Items,
            &not_in_the_folder("id, parent_id, name, placement"),
            &format!(
                "SELECT c.path, c.start_placement, w.id, w.waits, c.start FROM chain c LEFT JOIN ({waiting}) w ON w.id = c.start
                  WHERE c.parent_id = ?1 AND c.above",
                waiting = waiting("d.id, d.waits", &format!("EXISTS (SELECT 1 FROM items i WHERE i.id = d.id AND {})", placed("i.placement")))
            ),
        );
        let mut statement = self.conn.prepare_cached(&sql)?;
        let mut out = Vec::new();
        /// The item's path, its placement, its id if it is still here, what it waits for, its id.
        type Line = (String, String, Option<String>, Option<String>, String);
        let rows: Vec<Line> =
            statement.query_map([&root], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<_, _>>()?;
        let inside = self.inside_what_waits()?;
        for (path, placement, here, waits, id) in rows {
            // What is inside a folder that waits is covered by its line.
            if inside.contains(&id) {
                continue;
            }
            if let Placement::Skipped(reason) = Placement::decode(&placement) {
                // Still here, where the base places it. With nothing said
                // yet: an outbox commit deferred it, and no cycle has looked
                // at it since.
                let at = match &here {
                    Some(id) => self.locate_below(Some(&root), Table::Items, id)?.map(|at| at.rel),
                    None => None,
                };
                let waits = here.map(|_| waits.map_or(WaitsFor::Cycle, |stored| WaitsFor::parse(&stored)));
                out.push(Skipped { rel: PathBuf::from(path), reason, waits, here: at });
            }
        }
        drop(statement);
        out.extend(self.waiting_below_unplaced(&root)?);
        out.sort_by(|a, b| (&a.rel, &a.reason).cmp(&(&b.rel, &b.reason)));
        Ok(out)
    }

    /// Of the items that wait to leave the folder, those the base has
    /// inside another one that waits: the folder's line covers them, as a
    /// skipped folder's covers what is in it. The base still has them below
    /// that folder under its name on disk, so a line of their own would
    /// name a path that is nowhere.
    fn inside_what_waits(&self) -> Result<std::collections::HashSet<String>, TreeError> {
        let waiting: std::collections::HashSet<String> = {
            let mut statement = self.conn.prepare_cached(&waiting("d.id", &skipped("d.placement")))?;
            let ids = statement.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            ids
        };
        let mut inside = std::collections::HashSet::new();
        if waiting.len() < 2 {
            return Ok(inside);
        }
        for id in &waiting {
            let mut at = self.get(Table::Items, id)?.and_then(|row| row.parent_id);
            for _ in 0..MAX_CHAIN {
                let Some(folder) = at.take() else { break };
                if waiting.contains(&folder) {
                    inside.insert(id.clone());
                    break;
                }
                at = self.get(Table::Items, &folder)?.and_then(|row| row.parent_id);
            }
        }
        Ok(inside)
    }

    /// The items that wait to leave the folder because OneDrive has them
    /// below a folder that is not placed (moved there into the Personal
    /// Vault, say): the base still places each where the disk has it, and
    /// its change, which names that folder, waits in `deferred`. Each as a
    /// line of its own, under the path OneDrive has it at, with the reason
    /// of the folder that is not placed. The few changes that wait are
    /// looked at one by one.
    fn waiting_below_unplaced(&self, root: &str) -> Result<Vec<Skipped>, TreeError> {
        let moved: Vec<(String, Option<String>, String, Option<String>)> = {
            let mut statement = self.conn.prepare_cached(&waiting("d.id, d.parent_id, d.name, d.waits", &placed("d.placement")))?;
            let rows = statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?;
            rows
        };
        let mut out = Vec::new();
        for (id, parent, name, waits) in moved {
            let Some(here) = self.locate_below(Some(root), Table::Items, &id)?.filter(|at| at.placed).map(|at| at.rel) else { continue };
            let mut names = vec![name];
            let mut reason = None;
            let mut at = parent;
            let mut reached = false;
            for _ in 0..MAX_CHAIN {
                let Some(folder) = at.take() else { break };
                if folder == root {
                    reached = true;
                    break;
                }
                let Some(row) = self.get(Table::Items, &folder)? else { break };
                if let (None, Placement::Skipped(why)) = (&reason, &row.placement) {
                    reason = Some(*why);
                }
                names.push(row.name);
                at = row.parent_id;
            }
            if let (true, Some(reason)) = (reached, reason) {
                let rel: PathBuf = names.iter().rev().collect();
                out.push(Skipped { rel, reason, waits: Some(waits.map_or(WaitsFor::Cycle, |stored| WaitsFor::parse(&stored))), here: Some(here) });
            }
        }
        Ok(out)
    }
}

/// A line of `Skipped()`: an item OneDrive has and the folder cannot hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// Where OneDrive has it, relative to the root.
    pub rel: PathBuf,
    pub reason: SkipReason,
    /// `None`: it is not on this computer. Otherwise it still is, where the
    /// base places it, and this is what keeps it.
    pub waits: Option<WaitsFor>,
    /// Where it is on this computer, relative to the root, when it still
    /// is: the place the base has, which may be a name it stepped aside to.
    pub here: Option<PathBuf>,
}

/// `columns` of the deferred changes (`d`) whose row is `such`, and that
/// no outbox commit made after them supersedes: such a one is dropped when
/// the next cycle stages what waits ([`TreeStore::live_deferred`]).
fn waiting(columns: &str, such: &str) -> String {
    format!(
        "SELECT {columns} FROM deferred d
          WHERE d.gone = 0 AND {such}
            AND d.seq >= COALESCE((SELECT i.local_seq FROM items i WHERE i.id = d.id), 0)
            AND d.seq >= COALESCE((SELECT g.local_seq FROM outbox_gone g WHERE g.id = d.id), 0)"
    )
}

/// The rows of what OneDrive has and the folder cannot hold, as `columns`
/// (of `id, parent_id, name, placement`): each as its deferred change has
/// it, where one waits that says so, and as the base has it otherwise.
fn not_in_the_folder(columns: &str) -> String {
    format!(
        "{waiting}
         UNION ALL
         SELECT {columns} FROM items
          WHERE {out} AND id NOT IN ({waiting_ids})",
        waiting = waiting(&columns.split(", ").map(|c| format!("d.{c}")).collect::<Vec<_>>().join(", "), &skipped("d.placement")),
        waiting_ids = waiting("d.id", &skipped("d.placement")),
        out = skipped("placement"),
    )
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
