//! The outbox (`docs/design/writes.md` §5): what the folder holds that OneDrive does
//! not have yet — intent and progress, never the truth. The disk is the truth
//! about local changes (WR4): losing this table costs restarted uploads and
//! forgotten deletes, never a byte.
//!
//! **One live row per item.** A detection merges into the item's row (the
//! table in §3.5) unless that row is `running`; then one follow-up row waits
//! behind it. An item is its item id; something not uploaded yet is its local
//! object, by file handle (or inode where there is no handle).
//!
//! **Order.** Rows run in `seq` order, the order of first detection, which a
//! merge keeps. Four rules hold a row back ([`TreeStore::outbox_blockers`]):
//! an earlier row of the same item; the `mkdir` of the directory it is in,
//! whose item id it needs; for a folder's `delete` or `move-out`, every row
//! of an item the base has inside the folder; and a row that frees the name
//! in OneDrive a row takes. The last three are structural, whatever the
//! rows' `seq`: a move out of a folder detected after the folder's delete
//! must still run first, or the cloud deletes it with the folder. Where rule
//! 4 closes a circle, its edges in the circle are dropped (see
//! [`TreeStore::outbox_dependencies`]).
//!
//! Also here: `local_skipped` (what is never uploaded, §3.4 rule 2) and the
//! item's local object (`items.local_handle`).

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use konedrive_fs::handle::FileHandle;
use rusqlite::types::{Value, ValueRef};
use rusqlite::{params, Connection, OptionalExtension};

use crate::model::{upsert, Change, Table};
#[cfg(test)]
use crate::model::{Kind, Row};
use crate::source::Source;
use crate::staging::apply;
use crate::{ActivityRow, TreeError, TreeStore, ACTIVITY_KEPT, MAX_CHAIN};

mod changes;
/// What a row's `snapshot` and `target_name` hold.
mod encoded;
#[cfg(test)]
mod dependencies;
mod handles;
/// Which rows run next.
mod pick;
mod record;
mod row;
mod schema;
/// What the outbox holds, summed.
mod sums;
/// The outbox worker's own transactions.
mod worker;

pub use changes::OutboxChanges;
pub(super) use changes::watch;
pub use pick::{due, Pick, Picked, PORTION};
pub use encoded::{place_name, Snapshot};
use handles::set_local_handle;
pub use konedrive_reason::{key_of, known_group, Group, LocalSkip, Reason};
use record::record;
pub use row::{BadItem, Base, Committed, Detection, Inode, LocalSkipped, OutboxApplied, OutboxKind, OutboxOp, OutboxRow, OutboxState, Recorded};
pub use schema::OPENING_LEFT_KEEP;
pub(super) use schema::{upgrade, SCHEMA};
use schema::FREES;
pub use sums::{OutboxGroup, SkippedGroup};

/// A name the outbox worker gives an item in OneDrive while the name its
/// row takes is still another item's (§4.4, F55 (7)).
pub const SWAP_PREFIX: &str = ".konedrive-swap-";

/// The `meta` key counting outbox commits: `items.local_seq` of the row a
/// commit writes (the stale-delta guard, §3.7).
pub const OUTBOX_SEQ: &str = "outbox_seq";
/// The `meta` key of a pause's end, unix seconds; `0` until resumed (§9).
pub const PAUSED_UNTIL: &str = "paused_until";

const OUTBOX_COLUMNS: &str = "seq, kind, item_id, dev, ino, rel, base_etag, base_ctag, base_parent, base_name, \
     target_parent, target_name, state, reason, attempts, next_try, snapshot, session_url, session_expires, session_next, handle, confirmed, size";

/// A path as the store keeps it: text when it is UTF-8, its bytes otherwise
/// (Linux names need not be UTF-8; such a name is blocked, and still has to
/// be listed where it is). One path always gets the same form, so equality
/// in SQL holds.
fn path_value(path: &Path) -> Value {
    match path.to_str() {
        Some(text) => Value::Text(text.to_owned()),
        None => Value::Blob(path.as_os_str().as_bytes().to_vec()),
    }
}

fn path_from(value: ValueRef<'_>) -> PathBuf {
    match value {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => PathBuf::from(OsStr::from_bytes(bytes)),
        _ => PathBuf::new(),
    }
}

/// The sets of rows that wait on one another (strongly connected, more than
/// one row): Tarjan's algorithm, without recursion.
fn circles(graph: &HashMap<i64, Vec<i64>>) -> Vec<HashSet<i64>> {
    let mut index: HashMap<i64, usize> = HashMap::new();
    let mut low: HashMap<i64, usize> = HashMap::new();
    let mut on_stack: HashSet<i64> = HashSet::new();
    let mut stack: Vec<i64> = Vec::new();
    let mut next = 0usize;
    let mut out = Vec::new();
    let mut nodes: Vec<i64> = graph.keys().copied().collect();
    nodes.sort_unstable();
    for start in nodes {
        if index.contains_key(&start) {
            continue;
        }
        index.insert(start, next);
        low.insert(start, next);
        next += 1;
        stack.push(start);
        on_stack.insert(start);
        let mut call: Vec<(i64, usize)> = vec![(start, 0)];
        while let Some(&(v, i)) = call.last() {
            let edges = graph.get(&v).map(Vec::as_slice).unwrap_or_default();
            if i < edges.len() {
                call.last_mut().expect("just read").1 += 1;
                let w = edges[i];
                if !graph.contains_key(&w) {
                    continue;
                }
                if let Some(&w_index) = index.get(&w) {
                    if on_stack.contains(&w) {
                        let v_low = low.get_mut(&v).expect("visited");
                        *v_low = (*v_low).min(w_index);
                    }
                } else {
                    index.insert(w, next);
                    low.insert(w, next);
                    next += 1;
                    stack.push(w);
                    on_stack.insert(w);
                    call.push((w, 0));
                }
                continue;
            }
            call.pop();
            let v_low = low[&v];
            if let Some(&(parent, _)) = call.last() {
                let parent_low = low.get_mut(&parent).expect("visited");
                *parent_low = (*parent_low).min(v_low);
            }
            if v_low == index[&v] {
                let mut circle = HashSet::new();
                loop {
                    let w = stack.pop().expect("v is on the stack");
                    on_stack.remove(&w);
                    circle.insert(w);
                    if w == v {
                        break;
                    }
                }
                if circle.len() > 1 {
                    out.push(circle);
                }
            }
        }
    }
    out
}

/// Whether the row takes the item away from its base place.
fn moves_away(row: &OutboxRow) -> bool {
    row.base.as_ref().is_some_and(|b| (b.parent.as_deref(), b.name.as_deref()) != row.target())
}

/// The (parent, name) in OneDrive a row frees: a removal's base place, or a
/// move's.
pub fn frees(row: &OutboxRow) -> Option<(&str, &str)> {
    let frees = row.kind.removes() || (matches!(row.kind, OutboxKind::Move | OutboxKind::Update) && moves_away(row));
    let base = row.base.as_ref().filter(|_| frees)?;
    Some((base.parent.as_deref()?, base.name.as_deref()?))
}

/// The (parent, name) in OneDrive a row takes: a new folder's or file's, or
/// a move's target. A place inside a folder still to be made takes nothing
/// the cloud has yet.
pub fn takes(row: &OutboxRow) -> Option<(&str, &str)> {
    let takes = matches!(row.kind, OutboxKind::Mkdir | OutboxKind::Create)
        || (matches!(row.kind, OutboxKind::Move | OutboxKind::Update) && moves_away(row));
    if !takes {
        return None;
    }
    Some((row.target_parent.as_deref()?, row.target_name.as_deref()?))
}

/// `path` is strictly below `dir` (`""` being the root).
pub fn is_under(path: &Path, dir: &Path) -> bool {
    path != dir && path.starts_with(dir)
}

/// Where each column of [`OUTBOX_COLUMNS`] is in a row [`outbox_row`] reads.
mod at {
    use super::OUTBOX_COLUMNS;
    use crate::model::column;

    pub(super) const SEQ: usize = column(OUTBOX_COLUMNS, "seq");
    pub(super) const KIND: usize = column(OUTBOX_COLUMNS, "kind");
    pub(super) const ITEM_ID: usize = column(OUTBOX_COLUMNS, "item_id");
    pub(super) const DEV: usize = column(OUTBOX_COLUMNS, "dev");
    pub(super) const INO: usize = column(OUTBOX_COLUMNS, "ino");
    pub(super) const REL: usize = column(OUTBOX_COLUMNS, "rel");
    pub(super) const BASE_ETAG: usize = column(OUTBOX_COLUMNS, "base_etag");
    pub(super) const BASE_CTAG: usize = column(OUTBOX_COLUMNS, "base_ctag");
    pub(super) const BASE_PARENT: usize = column(OUTBOX_COLUMNS, "base_parent");
    pub(super) const BASE_NAME: usize = column(OUTBOX_COLUMNS, "base_name");
    pub(super) const TARGET_PARENT: usize = column(OUTBOX_COLUMNS, "target_parent");
    pub(super) const TARGET_NAME: usize = column(OUTBOX_COLUMNS, "target_name");
    pub(super) const STATE: usize = column(OUTBOX_COLUMNS, "state");
    pub(super) const REASON: usize = column(OUTBOX_COLUMNS, "reason");
    pub(super) const ATTEMPTS: usize = column(OUTBOX_COLUMNS, "attempts");
    pub(super) const NEXT_TRY: usize = column(OUTBOX_COLUMNS, "next_try");
    pub(super) const SNAPSHOT: usize = column(OUTBOX_COLUMNS, "snapshot");
    pub(super) const SESSION_URL: usize = column(OUTBOX_COLUMNS, "session_url");
    pub(super) const SESSION_EXPIRES: usize = column(OUTBOX_COLUMNS, "session_expires");
    pub(super) const SESSION_NEXT: usize = column(OUTBOX_COLUMNS, "session_next");
    pub(super) const HANDLE: usize = column(OUTBOX_COLUMNS, "handle");
    pub(super) const CONFIRMED: usize = column(OUTBOX_COLUMNS, "confirmed");
    pub(super) const SIZE: usize = column(OUTBOX_COLUMNS, "size");
}

/// A row of the outbox, read from a query that selects [`OUTBOX_COLUMNS`].
fn outbox_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutboxRow> {
    let kind: String = row.get(at::KIND)?;
    let state: String = row.get(at::STATE)?;
    let dev: Option<i64> = row.get(at::DEV)?;
    let ino: Option<i64> = row.get(at::INO)?;
    let handle: Option<Vec<u8>> = row.get(at::HANDLE)?;
    let handle = handle.as_deref().and_then(FileHandle::decode);
    let inode = match (dev, ino) {
        (Some(dev), Some(ino)) => Some(Inode { dev: dev as u64, ino: ino as u64, handle }),
        _ => handle.map(|handle| Inode { dev: 0, ino: 0, handle: Some(handle) }),
    };
    let base = Base { etag: row.get(at::BASE_ETAG)?, ctag: row.get(at::BASE_CTAG)?, parent: row.get(at::BASE_PARENT)?, name: row.get(at::BASE_NAME)? };
    let has_base = base != Base::default();
    // A value no konedrive writes fails closed: the row is blocked, never
    // run as a guess.
    let (known_kind, known_state) = (OutboxKind::parse(&kind), OutboxState::parse(&state));
    let unreadable = match (known_kind, known_state) {
        (None, _) => Some(format!("unreadable kind {kind:?}")),
        (_, None) => Some(format!("unreadable state {state:?}")),
        _ => None,
    };
    Ok(OutboxRow {
        seq: row.get(at::SEQ)?,
        kind: known_kind.unwrap_or(OutboxKind::Update),
        item_id: row.get(at::ITEM_ID)?,
        inode,
        rel: path_from(row.get_ref(at::REL)?),
        base: has_base.then_some(base),
        target_parent: row.get(at::TARGET_PARENT)?,
        target_name: row.get(at::TARGET_NAME)?,
        state: if unreadable.is_some() { OutboxState::Blocked } else { known_state.unwrap_or(OutboxState::Blocked) },
        reason: match unreadable {
            Some(why) => Some(Reason::Other(why)),
            None => row.get::<_, Option<String>>(at::REASON)?.map(Reason::from),
        },
        attempts: row.get::<_, i64>(at::ATTEMPTS)? as u32,
        next_try: row.get(at::NEXT_TRY)?,
        snapshot: row.get(at::SNAPSHOT)?,
        session_url: row.get(at::SESSION_URL)?,
        session_expires: row.get(at::SESSION_EXPIRES)?,
        session_next: row.get::<_, Option<i64>>(at::SESSION_NEXT)?.map(|n| n as u64),
        confirmed: row.get::<_, i64>(at::CONFIRMED)? != 0,
        size: row.get::<_, Option<i64>>(at::SIZE)?.map(|n| n.max(0) as u64),
    })
}

fn rows_where(conn: &Connection, filter: &str, params: impl rusqlite::Params) -> Result<Vec<OutboxRow>, TreeError> {
    let order = if filter.contains("ORDER BY") { "" } else { " ORDER BY seq" };
    let mut statement = conn.prepare_cached(&format!("SELECT {OUTBOX_COLUMNS} FROM outbox {filter}{order}"))?;
    let rows = statement.query_map(params, outbox_row)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn all_rows(conn: &Connection) -> Result<Vec<OutboxRow>, TreeError> {
    rows_where(conn, "", [])
}

/// Whether `items` (the base) has no row for `id` any more. A query that
/// fails counts as not gone: the row is kept rather than dropped on an
/// ambiguous answer.
fn item_gone(tx: &rusqlite::Transaction<'_>, id: &str) -> bool {
    tx.query_row("SELECT 1 FROM items WHERE id = ?1", [id], |_| Ok(())).optional().map(|found| found.is_none()).unwrap_or(false)
}

/// The live rows of the item or local object `d` is about, oldest first.
/// An object is found through its inode or its handle, both indexed, and
/// then compared as [`Inode::same_object`] does.
fn rows_for(conn: &Connection, item_id: Option<&str>, inode: Option<&Inode>) -> Result<Vec<OutboxRow>, TreeError> {
    match (item_id, inode) {
        (Some(id), _) => rows_where(conn, "WHERE item_id = ?1", [id]),
        (None, Some(inode)) => Ok(rows_where(
            conn,
            // `+item_id`: not the item index, which every row without an id shares.
            "WHERE +item_id IS NULL AND ((dev = ?1 AND ino = ?2) OR handle = ?3)",
            params![inode.dev as i64, inode.ino as i64, inode.handle.as_ref().map(FileHandle::encode)],
        )?
        .into_iter()
        .filter(|row| row.inode.as_ref().is_some_and(|i| i.same_object(inode)))
        .collect()),
        (None, None) => Ok(Vec::new()),
    }
}

/// The rows whose place is strictly below `dir`, oldest first: a range of the
/// `rel` index — the text form, and the bytes form of a name that is not UTF-8
/// ([`path_value`]) — checked again with [`is_under`].
fn rows_under(conn: &Connection, dir: &Path) -> Result<Vec<OutboxRow>, TreeError> {
    if dir.as_os_str().is_empty() {
        return Ok(all_rows(conn)?.into_iter().filter(|row| is_under(&row.rel, dir)).collect());
    }
    let bytes = dir.as_os_str().as_bytes();
    let (mut low, mut high) = (bytes.to_vec(), bytes.to_vec());
    low.push(b'/');
    high.push(b'/' + 1);
    let mut rows = rows_where(conn, "WHERE rel >= ?1 AND rel < ?2", params![Value::Blob(low.clone()), Value::Blob(high.clone())])?;
    if let (Ok(low), Ok(high)) = (String::from_utf8(low), String::from_utf8(high)) {
        rows.extend(rows_where(conn, "WHERE rel >= ?1 AND rel < ?2", params![low, high])?);
    }
    rows.retain(|row| is_under(&row.rel, dir));
    rows.sort_by_key(|row| row.seq);
    Ok(rows)
}

fn insert(conn: &Connection, row: &OutboxRow) -> Result<i64, TreeError> {
    let base = row.base.clone().unwrap_or_default();
    let (dev, ino, handle) = match &row.inode {
        Some(inode) => (Some(inode.dev as i64), Some(inode.ino as i64), inode.handle.as_ref().map(FileHandle::encode)),
        None => (None, None, None),
    };
    conn.execute(
        "INSERT INTO outbox (kind, item_id, dev, ino, rel, base_etag, base_ctag, base_parent, base_name,
                             target_parent, target_name, state, reason, attempts, next_try, snapshot,
                             session_url, session_expires, session_next, handle, confirmed, size)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
        params![
            row.kind.as_str(),
            row.item_id,
            dev,
            ino,
            path_value(&row.rel),
            base.etag,
            base.ctag,
            base.parent,
            base.name,
            row.target_parent,
            row.target_name,
            row.state.as_str(),
            row.reason_text(),
            row.attempts as i64,
            row.next_try,
            row.snapshot,
            row.session_url,
            row.session_expires,
            row.session_next.map(|n| n as i64),
            handle,
            row.confirmed as i64,
            row.size.map(|n| n as i64),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn rewrite(conn: &Connection, row: &OutboxRow) -> Result<(), TreeError> {
    let base = row.base.clone().unwrap_or_default();
    let (dev, ino, handle) = match &row.inode {
        Some(inode) => (Some(inode.dev as i64), Some(inode.ino as i64), inode.handle.as_ref().map(FileHandle::encode)),
        None => (None, None, None),
    };
    conn.execute(
        "UPDATE outbox SET kind = ?2, item_id = ?3, dev = ?4, ino = ?5, rel = ?6, base_etag = ?7, base_ctag = ?8,
                base_parent = ?9, base_name = ?10, target_parent = ?11, target_name = ?12, state = ?13, reason = ?14,
                attempts = ?15, next_try = ?16, snapshot = ?17, session_url = ?18, session_expires = ?19,
                session_next = ?20, handle = ?21, confirmed = ?22, size = ?23
          WHERE seq = ?1",
        params![
            row.seq,
            row.kind.as_str(),
            row.item_id,
            dev,
            ino,
            path_value(&row.rel),
            base.etag,
            base.ctag,
            base.parent,
            base.name,
            row.target_parent,
            row.target_name,
            row.state.as_str(),
            row.reason_text(),
            row.attempts as i64,
            row.next_try,
            row.snapshot,
            row.session_url,
            row.session_expires,
            row.session_next.map(|n| n as i64),
            handle,
            row.confirmed as i64,
            row.size.map(|n| n as i64),
        ],
    )?;
    Ok(())
}

fn rebase(conn: &Connection, from: &Path, to: &Path) -> Result<(), TreeError> {
    let mut update = conn.prepare_cached("UPDATE outbox SET rel = ?2 WHERE seq = ?1")?;
    for row in rows_under(conn, from)? {
        if let Ok(rest) = row.rel.strip_prefix(from) {
            update.execute(params![row.seq, path_value(&to.join(rest))])?;
        }
    }
    rebase_leaving(conn, from, to)
}

/// What is leaving at or below `from` is at `to` now, with the same path
/// below it (issue #104).
pub(super) fn rebase_leaving(conn: &Connection, from: &Path, to: &Path) -> Result<(), TreeError> {
    let moved: Vec<(String, PathBuf)> = {
        let mut statement = conn.prepare_cached("SELECT id, rel FROM leaving")?;
        let rows = statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, PathBuf::from(OsStr::from_bytes(&r.get::<_, Vec<u8>>(1)?)))))?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    let mut update = conn.prepare_cached("UPDATE leaving SET rel = ?2 WHERE id = ?1")?;
    for (id, rel) in moved {
        if let Ok(rest) = rel.strip_prefix(from) {
            let to = if rest.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rest) };
            update.execute(params![id, to.as_os_str().as_bytes()])?;
        }
    }
    Ok(())
}

impl TreeStore {
    /// Applies an examination's result in one transaction: a crash leaves
    /// all of it or none, and the next examination finds the rest on disk.
    pub fn outbox_apply(&mut self, ops: &[OutboxOp], now: i64) -> Result<OutboxApplied, TreeError> {
        let tx = self.conn.transaction()?;
        let mut out = OutboxApplied::default();
        for op in ops {
            match op {
                OutboxOp::Record(d) => match record(&tx, d)? {
                    Recorded::Inserted(seq) | Recorded::Merged(seq) => out.queued.push(seq),
                    Recorded::Removed(seq) => out.removed.push(seq),
                    Recorded::Nothing => {}
                },
                OutboxOp::Rebase { from, to } => rebase(&tx, from, to)?,
                OutboxOp::Remove(seq) => {
                    if tx.execute("DELETE FROM outbox WHERE seq = ?1", [seq])? > 0 {
                        out.removed.push(*seq);
                    }
                }
                OutboxOp::SetHandle { item_id, handle } => set_local_handle(&tx, item_id, handle.as_ref())?,
                OutboxOp::Skip { rel, reason, size } => {
                    tx.execute(
                        "INSERT INTO local_skipped (rel, reason, at, size) VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(rel) DO UPDATE SET reason = excluded.reason, size = excluded.size",
                        params![path_value(rel), reason.to_string(), now, *size as i64],
                    )?;
                }
                OutboxOp::Hold { seq, reason } => {
                    tx.execute(
                        "UPDATE outbox SET state = 'held', reason = ?2, next_try = NULL WHERE seq = ?1 AND state != 'running'",
                        params![seq, reason.to_string()],
                    )?;
                }
                OutboxOp::Unskip(rel) => {
                    tx.execute("DELETE FROM local_skipped WHERE rel = ?1", [path_value(rel)])?;
                }
            }
        }
        out.queued.sort_unstable();
        out.queued.dedup();
        let removed: HashSet<i64> = out.removed.iter().copied().collect();
        out.queued.retain(|seq| !removed.contains(seq));
        tx.commit()?;
        Ok(out)
    }

    /// Records one detection (see [`OutboxOp::Record`]).
    pub fn outbox_record(&mut self, d: &Detection) -> Result<Recorded, TreeError> {
        let tx = self.conn.transaction()?;
        let recorded = record(&tx, d)?;
        tx.commit()?;
        Ok(recorded)
    }

    /// Every row, in `seq` order.
    pub fn outbox_rows(&self) -> Result<Vec<OutboxRow>, TreeError> {
        all_rows(&self.conn)
    }

    pub fn outbox_row(&self, seq: i64) -> Result<Option<OutboxRow>, TreeError> {
        Ok(rows_where(&self.conn, "WHERE seq = ?1", [seq])?.into_iter().next())
    }

    /// The live rows of item `id`: at most one, and a follow-up behind a
    /// running one.
    pub fn outbox_for_item(&self, id: &str) -> Result<Vec<OutboxRow>, TreeError> {
        rows_where(&self.conn, "WHERE item_id = ?1", [id])
    }

    /// The live rows of a local object with no item id yet.
    pub fn outbox_for_inode(&self, inode: &Inode) -> Result<Vec<OutboxRow>, TreeError> {
        rows_for(&self.conn, None, Some(inode))
    }

    /// The row whose local object has `handle`: for the watcher, which maps an
    /// event's object to what it concerns.
    pub fn outbox_by_handle(&self, handle: &FileHandle) -> Result<Option<OutboxRow>, TreeError> {
        Ok(rows_where(&self.conn, "WHERE handle = ?1", [handle.encode()])?.into_iter().next())
    }

    /// Rows strictly below `rel`.
    pub fn outbox_under(&self, rel: &Path) -> Result<Vec<OutboxRow>, TreeError> {
        rows_under(&self.conn, rel)
    }

    /// What is leaving at `rel` is never moved or deleted in OneDrive by the
    /// daemon (issue #104): the `move` and `delete` rows whose local path is
    /// `rel` or below it go, but for one the worker is running. A row the
    /// user's own move out of it made — its path elsewhere — stays, and is
    /// carried out. What went.
    pub fn outbox_drop_moves(&mut self, rel: &Path) -> Result<Vec<OutboxRow>, TreeError> {
        let rows: Vec<OutboxRow> = self
            .outbox_at_or_under(rel)?
            .into_iter()
            .filter(|row| row.state != OutboxState::Running && matches!(row.kind, OutboxKind::Move | OutboxKind::Delete))
            // An item that was not in it when it began to leave — a placed
            // file the user moved in — is the user's to move or delete.
            .filter(|row| row.item_id.as_deref().is_some_and(|id| self.leaving_had(id).unwrap_or(true)))
            .collect();
        let tx = self.conn.transaction()?;
        for row in &rows {
            tx.execute("DELETE FROM outbox WHERE seq = ?1", [row.seq])?;
        }
        tx.commit()?;
        Ok(rows)
    }

    /// Items `ids` were removed in OneDrive while their objects waited inside
    /// something leaving (issue #104, decision 2): their rows go, but for
    /// one the worker is running. What went.
    pub fn outbox_drop_items(&mut self, ids: &[String]) -> Result<Vec<OutboxRow>, TreeError> {
        let ids: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let rows: Vec<OutboxRow> = all_rows(&self.conn)?
            .into_iter()
            .filter(|row| row.state != OutboxState::Running && row.item_id.as_deref().is_some_and(|id| ids.contains(id)))
            .collect();
        let tx = self.conn.transaction()?;
        for row in &rows {
            tx.execute("DELETE FROM outbox WHERE seq = ?1", [row.seq])?;
        }
        tx.commit()?;
        Ok(rows)
    }

    /// The changes blocked because OneDrive answered `404` for their item
    /// while it was leaving (`leaving-not-found`, issue #104), settled by
    /// this cycle's listing, read from the new tree before the swap (where
    /// the item's removal would wait behind the row itself): one whose item
    /// the listing removed has nothing left to send and goes, whatever it was
    /// in; one whose item it lists again — this cycle's delta brought it
    /// (`ids`), or, `whole`, a whole listing of the drive has it — is tried
    /// again. What went, and how many are tried again.
    pub fn outbox_settle_not_found(&mut self, ids: &[String], whole: bool) -> Result<(usize, usize), TreeError> {
        let blocked: Vec<(i64, String)> = all_rows(&self.conn)?
            .into_iter()
            .filter(|r| r.state == OutboxState::Blocked && r.reason == Some(Reason::LeavingNotFound))
            .filter_map(|r| Some((r.seq, r.item_id?)))
            .collect();
        let brought: HashSet<&str> = ids.iter().map(String::as_str).collect();
        let (mut gone, mut again) = (0, 0);
        for (seq, id) in blocked {
            if self.get(Table::Staging, &id)?.is_none() {
                gone += self.conn.execute("DELETE FROM outbox WHERE seq = ?1", [seq])?;
            } else if whole || brought.contains(id.as_str()) {
                again += self.conn.execute("UPDATE outbox SET state = 'ready', reason = NULL, next_try = NULL WHERE seq = ?1", [seq])?;
            }
        }
        Ok((gone, again))
    }

    /// Rows at `rel` or below it.
    pub fn outbox_at_or_under(&self, rel: &Path) -> Result<Vec<OutboxRow>, TreeError> {
        let mut rows = rows_under(&self.conn, rel)?;
        rows.extend(all_rows(&self.conn)?.into_iter().filter(|row| row.rel == rel));
        rows.sort_by_key(|row| row.seq);
        Ok(rows)
    }

    /// What OneDrive removed at `rel` was taken off the disk (issue #104):
    /// the rows that would upload, create or move something there or below
    /// it have nothing left to send, and go — not one the worker is running,
    /// whose commit meets OneDrive's answer, nor a removal, which the cycle's
    /// [`outbox_drop_removed`](Self::outbox_drop_removed) settles. What went.
    pub fn outbox_drop_under(&mut self, rel: &Path) -> Result<Vec<OutboxRow>, TreeError> {
        let rows: Vec<OutboxRow> =
            self.outbox_at_or_under(rel)?.into_iter().filter(|row| row.state != OutboxState::Running && !row.kind.removes()).collect();
        let tx = self.conn.transaction()?;
        for row in &rows {
            tx.execute("DELETE FROM outbox WHERE seq = ?1", [row.seq])?;
        }
        tx.commit()?;
        Ok(rows)
    }

    pub fn outbox_set_state(&self, seq: i64, state: OutboxState, reason: Option<&Reason>, next_try: Option<i64>) -> Result<(), TreeError> {
        self.conn.execute(
            "UPDATE outbox SET state = ?2, reason = ?3, next_try = ?4 WHERE seq = ?1",
            params![seq, state.as_str(), reason.map(Reason::to_string), next_try],
        )?;
        Ok(())
    }

    /// Counts one more failed attempt; the count after it.
    pub fn outbox_count_attempt(&self, seq: i64) -> Result<u32, TreeError> {
        self.conn.execute("UPDATE outbox SET attempts = attempts + 1 WHERE seq = ?1", [seq])?;
        let attempts: Option<i64> = self.conn.query_row("SELECT attempts FROM outbox WHERE seq = ?1", [seq], |r| r.get(0)).optional()?;
        Ok(attempts.unwrap_or(0) as u32)
    }

    pub fn outbox_set_snapshot(&self, seq: i64, snapshot: Option<Snapshot>) -> Result<(), TreeError> {
        self.conn.execute("UPDATE outbox SET snapshot = ?2 WHERE seq = ?1", params![seq, snapshot.map(|s| s.to_string())])?;
        Ok(())
    }

    /// An upload session's progress, persisted before the first byte and
    /// after each fragment (§4.8).
    pub fn outbox_set_session(&self, seq: i64, url: Option<&str>, expires: Option<i64>, next: Option<u64>) -> Result<(), TreeError> {
        self.conn.execute(
            "UPDATE outbox SET session_url = ?2, session_expires = ?3, session_next = ?4 WHERE seq = ?1",
            params![seq, url, expires, next.map(|n| n as i64)],
        )?;
        Ok(())
    }

    /// `ConfirmDeletes`: the held removals may go, and are marked confirmed,
    /// so that the guard neither counts nor holds them again while the
    /// worker gets through them. How many.
    pub fn outbox_release_held(&self) -> Result<usize, TreeError> {
        Ok(self.conn.execute("UPDATE outbox SET state = 'ready', reason = NULL, confirmed = 1 WHERE state = 'held'", [])?)
    }

    /// `RestoreDeletes`: the held rows are dropped, and returned so that
    /// their items can be placed again from the cloud. Their items, and what
    /// is inside them, forget their local inode in the same transaction:
    /// until the reconcile places them again, an examination cannot prove
    /// them deleted (they are unproven), so nothing deletes them in smaller
    /// batches the guard would let through.
    pub fn outbox_drop_held(&mut self) -> Result<Vec<OutboxRow>, TreeError> {
        let tx = self.conn.transaction()?;
        let held = rows_where(&tx, "WHERE state = 'held'", [])?;
        for id in held.iter().filter_map(|row| row.item_id.as_deref()) {
            // In both tables, so that a cycle between staging and swap cannot
            // give the items their local objects back (the outbox on the bus).
            for table in ["items", "staging"] {
                tx.execute(
                    &format!(
                        "WITH RECURSIVE below(id, depth) AS (
                             SELECT ?1, 0
                             UNION ALL
                             SELECT c.id, b.depth + 1 FROM items c JOIN below b ON c.parent_id = b.id WHERE b.depth < {MAX_CHAIN})
                         UPDATE {table} SET local_handle = NULL WHERE id IN (SELECT id FROM below)"
                    ),
                    [id],
                )?;
            }
        }
        tx.execute("DELETE FROM outbox WHERE state = 'held'", [])?;
        tx.commit()?;
        Ok(held)
    }

    /// A held or pending `delete` or `move-out` row whose item the delta or
    /// a Full reconcile just found gone from OneDrive — `items` (already
    /// swapped in for this cycle) holds no row for it — has nothing left to
    /// send: dropped without a request. Not a `running` row: its own commit
    /// meets the `404` itself and drops it there ([`Committed::Gone`]).
    /// Returns what was dropped, so that a dropped `move-out`'s placeholder
    /// outside the folder can be tidied the way a dropped `move-out` always
    /// is (`Tidy::dropped`, `upload::move_out`), and the outbox's
    /// counts and signals can be refreshed.
    pub fn outbox_drop_removed(&mut self) -> Result<Vec<OutboxRow>, TreeError> {
        let tx = self.conn.transaction()?;
        let gone: Vec<OutboxRow> = rows_where(&tx, "WHERE kind IN ('delete', 'move-out') AND state != 'running'", [])?
            .into_iter()
            .filter(|row| row.item_id.as_deref().is_some_and(|id| item_gone(&tx, id)))
            .collect();
        for row in &gone {
            tx.execute("DELETE FROM outbox WHERE seq = ?1", [row.seq])?;
        }
        tx.commit()?;
        Ok(gone)
    }

    /// The outbox commits so far (`meta` [`OUTBOX_SEQ`]).
    pub fn outbox_seq(&self) -> Result<i64, TreeError> {
        Ok(self.meta(OUTBOX_SEQ)?.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    /// Commit step 2 (§3.5), after the attributes are on the file: in one
    /// transaction, the base takes Graph's answer and the local object, with
    /// `local_seq = ++outbox_seq`; follow-ups behind a create learn its item
    /// id and base; the row goes; the activity event is written. Returns the
    /// commit's `local_seq`.
    pub fn outbox_commit(&mut self, seq: i64, committed: Committed<'_>, activity: Option<&ActivityRow>) -> Result<i64, TreeError> {
        let tx = self.conn.transaction()?;
        let local_seq = tx
            .query_row("SELECT value FROM meta WHERE key = ?1", [OUTBOX_SEQ], |r| r.get::<_, Option<String>>(0))
            .optional()?
            .flatten()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
            + 1;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![OUTBOX_SEQ, local_seq.to_string()],
        )?;
        let Some(committed_row) = rows_where(&tx, "WHERE seq = ?1", [seq])?.into_iter().next() else {
            return Err(TreeError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("outbox row {seq} is gone; nothing to commit"),
            )));
        };
        match committed {
            Committed::Item { row, handle } => {
                upsert(&tx, Table::Items, row)?;
                tx.execute(
                    "UPDATE items SET local_handle = ?2, local_seq = ?3 WHERE id = ?1",
                    params![row.id, handle.map(FileHandle::encode), local_seq],
                )?;
                // The follow-up behind it was detected against what this
                // commit made: that is its base now — and, behind a create,
                // its item id.
                let mut followers = rows_for(&tx, Some(&row.id), None)?;
                if let Some(inode) = committed_row.inode.clone() {
                    followers.extend(rows_for(&tx, None, Some(&inode))?);
                }
                for mut follower in followers.into_iter().filter(|r| r.seq != seq) {
                    follower.item_id = Some(row.id.clone());
                    follower.base = Some(Base {
                        etag: row.etag.clone(),
                        ctag: row.ctag.clone(),
                        parent: row.parent_id.clone(),
                        name: Some(row.name.clone()),
                    });
                    rewrite(&tx, &follower)?;
                }
            }
            Committed::Gone { item_id } => {
                apply(&tx, Source::Items, &[Change::Delete(item_id.to_owned())])?;
                // A delta fetched before this delete must not bring it back.
                crate::reconcile::tombstone(&tx, &[item_id], local_seq)?;
            }
        }
        tx.execute("DELETE FROM outbox WHERE seq = ?1", [seq])?;
        if let Some(event) = activity {
            tx.execute(
                "INSERT INTO activity (at, kind, path, detail) VALUES (?1, ?2, ?3, ?4)",
                params![event.at, event.kind, event.path, event.detail],
            )?;
            tx.execute(
                "DELETE FROM activity WHERE id NOT IN (SELECT id FROM activity ORDER BY id DESC LIMIT ?1)",
                [ACTIVITY_KEPT as i64],
            )?;
        }
        tx.commit()?;
        Ok(local_seq)
    }

    /// What is never uploaded, by path (`NotUploaded()` adds the blocked rows).
    pub fn local_skipped(&self) -> Result<Vec<LocalSkipped>, TreeError> {
        let mut statement = self.conn.prepare("SELECT rel, reason, at FROM local_skipped ORDER BY rel")?;
        let rows = statement
            .query_map([], |row| Ok(LocalSkipped { rel: path_from(row.get_ref(0)?), reason: row.get::<_, String>(1)?.into(), at: row.get(2)? }))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests;
