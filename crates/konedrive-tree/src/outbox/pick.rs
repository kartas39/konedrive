//! Which rows run next, without reading the whole queue (issue #38).
//!
//! **Rules 1–3** of the module's doc are asked of one row at a time, by point
//! queries: its item's and its local object's earlier rows (the item and
//! object indexes), the `mkdir` of its directory (the place index), and for
//! a folder's removal, the rows of what the base has inside it (one walk of
//! the base, joined to the outbox through the item index).
//!
//! **Rule 4** needs the whole picture, but only of the rows that free or
//! take a name in OneDrive: removals and moves away from the base place (a
//! partial index), and the rows that take one of the names those free —
//! found through the target-folder index. Its circles are looked for in the
//! graph those rows and what they wait for make, and a rule-4 edge inside
//! one is dropped, as before.
//!
//! **Picking** reads the due rows in `seq` order, a portion at a time, and
//! follows a row that waits to the head of its wait chain, remembering the
//! rows visited. A head that is ready runs; one that runs, waits for a time
//! or waits for the user says why nothing behind it can run. Portions are
//! read until enough rows are found or the queue ends, so a portion where
//! every row waits never stops the worker. **The invariant**: when nothing
//! can run, something runs, something waits for a time, or something waits
//! for the user ([`Picked`]); anything else is a stall, reported.

use std::collections::{HashMap, HashSet};
use rusqlite::{params, Connection};

use konedrive_reason::TOO_BIG_PREFIX;
use super::{circles, frees, path_value, rows_where, takes, OutboxKind, OutboxRow, OutboxState, Reason, FREES};
use crate::{Kind, TreeError, TreeStore, MAX_CHAIN};

/// Due rows read at a time (a guess: large enough that a portion is one
/// query's worth of work, small enough that a pick that finds its rows early
/// reads little).
pub const PORTION: usize = 100;

/// Whether `row` may be taken at `now` as far as its own state goes: `ready`,
/// `running` with nobody holding it (a crash or a stop left it; replayed),
/// `retry` whose time has come, `waiting` whose look is due.
pub fn due(row: &OutboxRow, now: i64) -> bool {
    match row.state {
        OutboxState::Ready | OutboxState::Running => true,
        OutboxState::Retry => row.next_try.is_none_or(|at| at <= now),
        OutboxState::Waiting => row.next_try.is_some_and(|at| at <= now),
        OutboxState::Blocked | OutboxState::Held => false,
    }
}

/// What the worker asks of a pick.
pub struct Pick<'a> {
    pub now: i64,
    /// Rows the worker holds already.
    pub flying: &'a HashSet<i64>,
    /// Whether `move-out` rows can run (a worker built with what they need).
    pub move_outs: bool,
    /// Enough rows: portions stop being read once this many are found.
    pub want: usize,
    /// Whether a row with nothing to wait for may be taken as the space in
    /// OneDrive stands; the store is there for what that needs to look up.
    pub allows: &'a dyn Fn(&TreeStore, &OutboxRow) -> Result<bool, TreeError>,
}

/// What a pick found: the rows to run, in `seq` order, and — the invariant —
/// why the rest cannot run now.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Picked {
    pub rows: Vec<OutboxRow>,
    /// A row the worker holds is in front of others: its end wakes the worker.
    pub running: bool,
    /// The earliest time a row in front of others falls due.
    pub until: Option<i64>,
    /// A row in front of others waits for the user: held, blocked, waiting
    /// for space, or for what a move out needs.
    pub user: bool,
    /// Due rows that wait for nothing of the above: must never happen.
    pub stalled: Vec<i64>,
}

impl Picked {
    fn wait_until(&mut self, at: i64) {
        self.until = Some(self.until.map_or(at, |u| u.min(at)));
    }
}

fn one(conn: &Connection, seq: i64) -> Result<Option<OutboxRow>, TreeError> {
    Ok(rows_where(conn, "WHERE seq = ?1", [seq])?.into_iter().next())
}

/// What one pick has asked already: the `mkdir` row of each directory (many
/// rows share one), and rule 1's answers for a whole portion, read at once.
#[derive(Default)]
struct Memo {
    mkdirs: HashMap<std::path::PathBuf, Option<i64>>,
    earlier: HashMap<i64, Vec<i64>>,
    /// Once many portions were read: the item ids, handles and inodes more
    /// than one row has. A row with none of them waits for no earlier row of
    /// its own (rule 1), and is not asked.
    shared: Option<Shared>,
}

/// The item ids, handles and inodes more than one row has.
type Shared = (HashSet<String>, HashSet<Vec<u8>>, HashSet<(i64, i64)>);

/// Portions read before rule 1 is answered from what is shared ([`Memo::shared`]).
const PORTIONS_ASKED: usize = 8;

impl Memo {
    /// What more than one row has: three walks of the indexes, once per pick.
    fn share(&mut self, conn: &Connection) -> Result<(), TreeError> {
        let ids = conn
            .prepare("SELECT item_id FROM outbox WHERE item_id IS NOT NULL GROUP BY item_id HAVING count(*) > 1")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<HashSet<String>, _>>()?;
        let handles = conn
            .prepare("SELECT handle FROM outbox WHERE handle IS NOT NULL GROUP BY handle HAVING count(*) > 1")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<HashSet<Vec<u8>>, _>>()?;
        let inodes = conn
            .prepare("SELECT dev, ino FROM outbox WHERE dev IS NOT NULL GROUP BY dev, ino HAVING count(*) > 1")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<HashSet<(i64, i64)>, _>>()?;
        self.shared = Some((ids, handles, inodes));
        Ok(())
    }

    /// Rule 1 for every row of `portion`: two queries for all of them — by
    /// item id, and by handle — and one per row known by its inode alone; or,
    /// once what is shared is known, a query only for a row that shares.
    fn portion(&mut self, conn: &Connection, portion: &[OutboxRow]) -> Result<(), TreeError> {
        if let Some((ids, handles, inodes)) = &self.shared {
            for row in portion {
                let shares = row.item_id.as_ref().is_some_and(|id| ids.contains(id))
                    || row.inode.as_ref().is_some_and(|i| match &i.handle {
                        Some(handle) => handles.contains(&handle.encode()),
                        None => inodes.contains(&(i.dev as i64, i.ino as i64)),
                    });
                let mut earlier = Vec::new();
                if shares {
                    rule_one(conn, row, &mut earlier)?;
                }
                self.earlier.insert(row.seq, earlier);
            }
            return Ok(());
        }
        let ids: HashSet<&str> = portion.iter().filter_map(|r| r.item_id.as_deref()).collect();
        let handles: HashSet<Vec<u8>> = portion.iter().filter_map(|r| r.inode.as_ref()?.handle.as_ref().map(|h| h.encode())).collect();
        let mut by_id: HashMap<String, Vec<i64>> = HashMap::new();
        if !ids.is_empty() {
            let sql = format!("SELECT item_id, seq FROM outbox WHERE item_id IN ({})", vec!["?"; ids.len()].join(","));
            let mut statement = conn.prepare(&sql)?;
            let found = statement.query_map(rusqlite::params_from_iter(ids.iter()), |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for pair in found {
                let (id, seq) = pair?;
                by_id.entry(id).or_default().push(seq);
            }
        }
        let mut by_handle: HashMap<Vec<u8>, Vec<i64>> = HashMap::new();
        if !handles.is_empty() {
            let sql = format!("SELECT handle, seq FROM outbox WHERE +item_id IS NULL AND handle IN ({})", vec!["?"; handles.len()].join(","));
            let mut statement = conn.prepare(&sql)?;
            let found = statement.query_map(rusqlite::params_from_iter(handles.iter()), |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)))?;
            for pair in found {
                let (handle, seq) = pair?;
                by_handle.entry(handle).or_default().push(seq);
            }
        }
        for row in portion {
            let mut earlier: Vec<i64> = Vec::new();
            if let Some(id) = &row.item_id {
                earlier.extend(by_id.get(id).into_iter().flatten().copied().filter(|&seq| seq < row.seq));
            }
            match row.inode.as_ref().map(|i| (i, i.handle.as_ref())) {
                Some((_, Some(handle))) => earlier.extend(by_handle.get(&handle.encode()).into_iter().flatten().copied().filter(|&seq| seq < row.seq)),
                Some((inode, None)) => earlier.extend(same_inode_before(conn, inode, row.seq)?),
                None => {}
            }
            self.earlier.insert(row.seq, earlier);
        }
        Ok(())
    }
}

/// Earlier rows with no item id and no handle, on `inode`.
fn same_inode_before(conn: &Connection, inode: &super::Inode, seq: i64) -> Result<Vec<i64>, TreeError> {
    let mut statement = conn.prepare_cached("SELECT seq FROM outbox WHERE +item_id IS NULL AND handle IS NULL AND dev = ?1 AND ino = ?2 AND seq < ?3")?;
    let seqs = statement.query_map(params![inode.dev as i64, inode.ino as i64, seq], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
    Ok(seqs)
}

/// Rules 1–3 for `row`, by point queries: the live rows it waits for.
/// `memo` holds what this pick asked already.
fn structural(conn: &Connection, row: &OutboxRow, mut memo: Option<&mut Memo>) -> Result<Vec<i64>, TreeError> {
    let mut out: Vec<i64> = Vec::new();
    // 1. An earlier row of the same item, or of the same local object with no
    // id yet, the object being its handle or, without one, its inode.
    if let Some(known) = memo.as_deref_mut().and_then(|m| m.earlier.remove(&row.seq)) {
        out.extend(known);
    } else {
        rule_one(conn, row, &mut out)?;
    }
    rule_two_three(conn, row, memo.map(|m| &mut m.mkdirs), out)
}

fn rule_one(conn: &Connection, row: &OutboxRow, out: &mut Vec<i64>) -> Result<(), TreeError> {
    if let Some(id) = &row.item_id {
        let mut statement = conn.prepare_cached("SELECT seq FROM outbox WHERE item_id = ?1 AND seq < ?2")?;
        out.extend(statement.query_map(params![id, row.seq], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?);
    }
    if let Some(inode) = &row.inode {
        // The same handle's bytes, or no handle and the same inode (`same_key`).
        let earlier: Vec<i64> = match &inode.handle {
            Some(handle) => {
                let mut statement = conn.prepare_cached("SELECT seq FROM outbox WHERE +item_id IS NULL AND handle = ?1 AND seq < ?2")?;
                let seqs = statement.query_map(params![handle.encode(), row.seq], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
                seqs
            }
            None => same_inode_before(conn, inode, row.seq)?,
        };
        out.extend(earlier);
    }
    Ok(())
}

fn rule_two_three(
    conn: &Connection,
    row: &OutboxRow,
    mkdirs: Option<&mut HashMap<std::path::PathBuf, Option<i64>>>,
    mut out: Vec<i64>,
) -> Result<Vec<i64>, TreeError> {
    // 2. The mkdir of the directory it is in.
    if !row.kind.removes() {
        if let Some(parent) = row.rel.parent() {
            let mkdir = match mkdirs.as_ref().and_then(|m| m.get(parent).copied()) {
                Some(known) => known,
                None => {
                    let mut statement = conn.prepare_cached("SELECT seq FROM outbox WHERE rel = ?1 AND kind = 'mkdir' ORDER BY seq DESC LIMIT 1")?;
                    let found: Option<i64> = statement.query_map([path_value(parent)], |r| r.get(0))?.next().transpose()?;
                    if let Some(m) = mkdirs {
                        m.insert(parent.to_path_buf(), found);
                    }
                    found
                }
            };
            if let Some(mkdir) = mkdir.filter(|&m| m != row.seq) {
                out.push(mkdir);
            }
        }
    }
    // 3. A folder leaving OneDrive waits for every row of what the base has
    // inside it: by item id, not by path.
    if row.kind.removes() {
        if let Some(id) = &row.item_id {
            let folder: Option<String> =
                conn.prepare_cached("SELECT kind FROM items WHERE id = ?1")?.query_map([id], |r| r.get(0))?.next().transpose()?;
            if folder.as_deref() == Some(Kind::Folder.as_str()) {
                let sql = format!(
                    "WITH RECURSIVE below(id, depth) AS (
                         SELECT id, 1 FROM items WHERE parent_id = ?1
                         UNION ALL
                         SELECT c.id, b.depth + 1 FROM items c JOIN below b ON c.parent_id = b.id WHERE b.depth < {MAX_CHAIN})
                     SELECT o.seq FROM below b JOIN outbox o ON o.item_id = b.id WHERE o.seq != ?2"
                );
                let mut statement = conn.prepare_cached(&sql)?;
                out.extend(statement.query_map(params![id, row.seq], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Rule 4's edges, taker → freers, with those inside a circle dropped; built
/// from the rows that free or take a name only.
fn name_edges(conn: &Connection) -> Result<HashMap<i64, Vec<i64>>, TreeError> {
    let freers: Vec<OutboxRow> = rows_where(conn, &format!("WHERE {FREES}"), [])?.into_iter().filter(|r| frees(r).is_some()).collect();
    if freers.is_empty() {
        return Ok(HashMap::new());
    }
    let mut freeing: HashMap<(String, String), Vec<i64>> = HashMap::new();
    for row in &freers {
        let (parent, name) = frees(row).expect("filtered above");
        freeing.entry((parent.to_owned(), name.to_lowercase())).or_default().push(row.seq);
    }
    let parents: HashSet<String> = freeing.keys().map(|(parent, _)| parent.clone()).collect();
    let mut rows: HashMap<i64, OutboxRow> = freers.into_iter().map(|r| (r.seq, r)).collect();
    let mut by_name: Vec<(i64, i64)> = Vec::new();
    for parent in &parents {
        for row in rows_where(conn, "WHERE target_parent = ?1", [parent])? {
            let Some((p, name)) = takes(&row) else { continue };
            let Some(freers) = freeing.get(&(p.to_owned(), name.to_lowercase())) else { continue };
            for &freer in freers {
                if freer != row.seq {
                    by_name.push((row.seq, freer));
                }
            }
            rows.entry(row.seq).or_insert(row);
        }
    }
    if by_name.is_empty() {
        return Ok(HashMap::new());
    }
    // The graph the name rows and what they wait for make (rules 1–3), for
    // the circles: every circle through a rule-4 edge lies in it.
    let mut union: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut queue: Vec<i64> = rows.keys().copied().collect();
    while let Some(seq) = queue.pop() {
        if union.contains_key(&seq) {
            continue;
        }
        let row = match rows.get(&seq) {
            Some(row) => row.clone(),
            None => match one(conn, seq)? {
                Some(row) => row,
                None => continue,
            },
        };
        let edges = structural(conn, &row, None)?;
        queue.extend(edges.iter().copied().filter(|b| !union.contains_key(b)));
        union.insert(seq, edges);
    }
    let mut kept: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut with_names = union.clone();
    for &(taker, freer) in &by_name {
        with_names.entry(taker).or_default().push(freer);
    }
    let circles = circles(&with_names);
    let circle_of: HashMap<i64, usize> = circles.iter().enumerate().flat_map(|(n, c)| c.iter().map(move |&seq| (seq, n))).collect();
    for (taker, freer) in by_name {
        let inside_one_circle = circle_of.get(&taker).is_some_and(|n| circle_of.get(&freer) == Some(n));
        if !inside_one_circle {
            kept.entry(taker).or_default().push(freer);
        }
    }
    Ok(kept)
}

impl TreeStore {
    /// The rows `row` waits for (the module's four rules).
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn outbox_blockers_of(&self, row: &OutboxRow) -> Result<Vec<i64>, TreeError> {
        let mut out = structural(&self.conn, row, None)?;
        out.extend(name_edges(&self.conn)?.remove(&row.seq).unwrap_or_default());
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// The rows `seq` waits for.
    #[cfg(any(test, feature = "testing"))]
    pub fn outbox_blockers(&self, seq: i64) -> Result<Vec<i64>, TreeError> {
        match one(&self.conn, seq)? {
            Some(row) => self.outbox_blockers_of(&row),
            None => Ok(Vec::new()),
        }
    }

    /// The rows that can run now, in `seq` order: `ready`, or `retry` whose
    /// time has come, with nothing to wait for. Which of them run at once is
    /// the worker's (metadata rows one at a time, §3.5).
    #[cfg(any(test, feature = "testing"))]
    pub fn outbox_runnable(&self, now: i64) -> Result<Vec<OutboxRow>, TreeError> {
        let names = name_edges(&self.conn)?;
        let mut out = Vec::new();
        for row in rows_where(&self.conn, "WHERE state IN ('ready', 'retry')", [])? {
            let ready = match row.state {
                OutboxState::Ready => true,
                OutboxState::Retry => row.next_try.is_none_or(|at| at <= now),
                _ => false,
            };
            if ready && structural(&self.conn, &row, None)?.is_empty() && !names.contains_key(&row.seq) {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// Due rows after `seq`, in `seq` order, at most `limit`.
    fn outbox_due_after(&self, now: i64, after: i64, limit: usize) -> Result<Vec<OutboxRow>, TreeError> {
        Ok(rows_where(
            &self.conn,
            // `+state`: along `seq`, not the due index, whose order would have to be sorted.
            "WHERE seq > ?1 AND (+state IN ('ready', 'running') OR (+state = 'retry' AND (next_try IS NULL OR next_try <= ?2))
                                 OR (+state = 'waiting' AND next_try <= ?2)) ORDER BY seq LIMIT ?3",
            params![after, now, limit as i64],
        )?
        .into_iter()
        .filter(|row| due(row, now))
        .collect())
    }

    /// The next rows to run ([`Pick`]), and why the others cannot (the module's doc).
    pub fn outbox_pick(&self, pick: &Pick<'_>) -> Result<Picked, TreeError> {
        let names = name_edges(&self.conn)?;
        let mut out = Picked::default();
        let mut seen: HashSet<i64> = HashSet::new();
        let mut memo = Memo::default();
        let mut cursor = 0;
        let mut portions = 0;
        let mut candidates = false;
        while out.rows.len() < pick.want {
            // The query's own order: `seq`, strictly after the last row read.
            let portion = self.outbox_due_after(pick.now, cursor, PORTION)?;
            let Some(last) = portion.last() else { break };
            cursor = last.seq;
            portions += 1;
            if portions == PORTIONS_ASKED {
                memo.share(&self.conn)?;
            }
            memo.portion(&self.conn, &portion)?;
            for row in portion {
                candidates = true;
                if !seen.contains(&row.seq) {
                    self.follow(row, &names, pick, &mut seen, &mut memo, &mut out)?;
                }
            }
        }
        out.rows.sort_by_key(|row| row.seq);
        if candidates && out.rows.is_empty() && !out.running && out.until.is_none() && !out.user {
            out.stalled = seen.into_iter().collect();
            out.stalled.sort_unstable();
        }
        Ok(out)
    }

    /// Follows `start` to the heads of its wait chains: a head that can run
    /// is taken; any other says what the chain waits for.
    fn follow(
        &self,
        start: OutboxRow,
        names: &HashMap<i64, Vec<i64>>,
        pick: &Pick<'_>,
        seen: &mut HashSet<i64>,
        memo: &mut Memo,
        out: &mut Picked,
    ) -> Result<(), TreeError> {
        let mut stack = vec![start];
        while let Some(row) = stack.pop() {
            if !seen.insert(row.seq) {
                continue;
            }
            if pick.flying.contains(&row.seq) {
                out.running = true;
                continue;
            }
            if !due(&row, pick.now) {
                match row.next_try.filter(|_| matches!(row.state, OutboxState::Retry | OutboxState::Waiting)) {
                    Some(at) => out.wait_until(at),
                    None => out.user = true,
                }
                continue;
            }
            if row.kind == OutboxKind::MoveOut && !pick.move_outs {
                out.user = true;
                continue;
            }
            let mut blockers = structural(&self.conn, &row, Some(memo))?;
            blockers.extend(names.get(&row.seq).into_iter().flatten().copied());
            if blockers.is_empty() {
                if (pick.allows)(self, &row)? {
                    out.rows.push(row);
                } else {
                    out.user = true;
                }
                continue;
            }
            for blocker in blockers {
                if !seen.contains(&blocker) {
                    if let Some(next) = one(&self.conn, blocker)? {
                        stack.push(next);
                    }
                }
            }
        }
        Ok(())
    }

    /// When the next row waiting for a time falls due after `now`.
    pub fn outbox_next_due(&self, now: i64) -> Result<Option<i64>, TreeError> {
        let mut statement =
            self.conn.prepare_cached("SELECT min(next_try) FROM outbox WHERE state IN ('retry', 'waiting', 'blocked') AND next_try > ?1")?;
        Ok(statement.query_row([now], |r| r.get(0))?)
    }

    /// Whether a removal of `row`'s object is recorded behind it (issue #27).
    pub fn outbox_removed_behind(&self, row: &OutboxRow) -> Result<bool, TreeError> {
        let Some(inode) = &row.inode else { return Ok(false) };
        let behind = rows_where(
            &self.conn,
            "WHERE kind = 'delete' AND seq > ?1 AND ((dev = ?2 AND ino = ?3) OR handle = ?4)",
            params![row.seq, inode.dev as i64, inode.ino as i64, inode.handle.as_ref().map(|h| h.encode())],
        )?;
        Ok(behind.iter().any(|r| r.inode.as_ref() == Some(inode)))
    }

    /// The rows with these `seq`s that are still there.
    pub fn outbox_rows_of(&self, seqs: &[i64]) -> Result<Vec<OutboxRow>, TreeError> {
        let mut out = Vec::with_capacity(seqs.len());
        for chunk in seqs.chunks(500) {
            let list = chunk.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
            out.extend(rows_where(&self.conn, &format!("WHERE seq IN ({list})"), [])?);
        }
        out.sort_by_key(|row| row.seq);
        Ok(out)
    }

    /// The `move-out` rows, through the kind index.
    pub fn outbox_move_outs(&self) -> Result<Vec<OutboxRow>, TreeError> {
        rows_where(&self.conn, "WHERE kind = 'move-out'", [])
    }

    /// The first `limit` rows, in `seq` order (`Changes(limit)`).
    pub fn outbox_first(&self, limit: usize) -> Result<Vec<OutboxRow>, TreeError> {
        rows_where(&self.conn, "ORDER BY seq LIMIT ?1", [limit.min(i64::MAX as usize) as i64])
    }


    /// The ready rows waiting for space in OneDrive (`waiting-for-space`,
    /// `too-big:…`): what a quota read may let go.
    pub fn outbox_waiting_for_space(&self) -> Result<Vec<OutboxRow>, TreeError> {
        rows_where(
            &self.conn,
            "WHERE state = 'ready' AND (reason = ?1 OR substr(reason, 1, length(?2)) = ?2)",
            [Reason::WaitingForSpace.key(), TOO_BIG_PREFIX],
        )
    }

    /// The blocked rows (`NotUploaded()`), through the due index.
    pub fn outbox_blocked(&self) -> Result<Vec<OutboxRow>, TreeError> {
        Ok(rows_where(&self.conn, "WHERE state = 'blocked' OR kind NOT IN ('create', 'mkdir', 'update', 'move', 'delete', 'move-out')
                                     OR state NOT IN ('waiting', 'ready', 'running', 'retry', 'blocked', 'held')", [])?
        .into_iter()
        .filter(|row| row.state == OutboxState::Blocked)
        .collect())
    }

    /// How many rows there are.
    pub fn outbox_len(&self) -> Result<usize, TreeError> {
        let n: i64 = self.conn.prepare_cached("SELECT count(*) FROM outbox")?.query_row([], |r| r.get(0))?;
        Ok(n.max(0) as usize)
    }
}

#[cfg(test)]
mod tests;
