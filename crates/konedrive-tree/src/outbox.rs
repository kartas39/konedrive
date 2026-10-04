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
use std::path::Path;

use konedrive_fs::handle::FileHandle;
use rusqlite::types::Value;
use rusqlite::{params, Connection, OptionalExtension};

use crate::forget::{base_places, forget_subtrees, forget_unplaced};
use crate::meta::next_outbox_seq;
#[cfg(test)]
use crate::model::Kind;
use crate::model::{upsert, Change, Placement, Row, Table};
use crate::query::get_row;
use crate::reconcile::wait;
use crate::source::Source;
use crate::staging::apply;
use crate::{ActivityRow, TreeError, TreeStore, ACTIVITY_KEPT};

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
/// A row in the database.
mod stored;
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
pub use row::{BadItem, Base, Committed, Detection, Inode, LocalSkipped, OutboxApplied, OutboxKind, OutboxOp, OutboxRow, OutboxState, Recorded, SessionUrl};
use stored::{all_rows, insert, path_from, path_value, remove, rewrite, rows_where, set_snapshot};
pub use sums::{OutboxGroup, SkippedGroup};
pub use worker::ConflictCopy;

/// A name the outbox worker gives an item in OneDrive while the name its
/// row takes is still another item's (§4.4, F55 (7)).
pub const SWAP_PREFIX: &str = ".konedrive-swap-";

/// How long a record of an opening whose row left is kept (issue #89): a
/// guess, longer than an abandoned placeholder was seen to live (a day).
pub const OPENING_LEFT_KEEP: i64 = 7 * 24 * 3600;

/// The rows the partial index `outbox_frees` holds: those with a base place
/// they leave. [`frees`] decides among them.
pub(crate) const FREES: &str = "base_parent IS NOT NULL AND base_name IS NOT NULL AND (base_parent IS NOT target_parent OR base_name IS NOT target_name)";

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

fn rebase(conn: &Connection, from: &Path, to: &Path) -> Result<(), TreeError> {
    let mut update = conn.prepare_cached("UPDATE outbox SET rel = ?2 WHERE seq = ?1")?;
    for row in rows_under(conn, from)? {
        if let Ok(rest) = row.rel.strip_prefix(from) {
            update.execute(params![row.seq, path_value(&to.join(rest))])?;
        }
    }
    Ok(())
}

/// Where the object of the item `base` stands once `committed` is carried
/// out: the folder the row takes it to, or, where the row names none, the
/// base's; and the name the object has on disk.
pub(super) fn local_place(committed: &OutboxRow, base: &Row) -> (Option<String>, String) {
    let parent = committed.target_parent.clone().or_else(|| base.parent_id.clone());
    let name = committed.rel.file_name().and_then(OsStr::to_str).map_or_else(|| base.name.clone(), str::to_owned);
    (parent, name)
}

/// Whether the base would place `row`, written into `items`: its own
/// placement, and the folder it names placed up to the root.
pub(super) fn would_place(tx: &rusqlite::Transaction<'_>, row: &Row) -> Result<bool, TreeError> {
    if row.placement != Placement::Placed {
        return Ok(false);
    }
    match &row.parent_id {
        Some(parent) => base_places(tx, parent),
        None => Ok(false),
    }
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
                    if remove(&tx, *seq)? {
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
    #[cfg(any(test, feature = "testing"))]
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
    #[cfg(any(test, feature = "testing"))]
    pub fn outbox_under(&self, rel: &Path) -> Result<Vec<OutboxRow>, TreeError> {
        rows_under(&self.conn, rel)
    }

    /// The object of item `id`, which can no longer be placed and waits,
    /// stepped aside in its directory for another item that takes its name:
    /// it stands at `to` now, under `name`, where it stood at `from`. In one
    /// transaction the base follows — the same folder, the new name, the
    /// version as it was; OneDrive's place of the item is in its deferred
    /// change alone — and so do the rows: those of the item itself are made
    /// against the new name, as if recorded there, so that none of them
    /// sends it, and those below it follow as below any directory renamed.
    ///
    /// `repair`: the rename was made by a cycle that stopped before this
    /// was written, and an examination since took it for a rename made
    /// here: that row goes, since nobody made it.
    pub fn step_aside(&mut self, id: &str, from: &Path, to: &Path, name: &str, repair: bool) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        let was: Option<String> = tx.query_row("SELECT name FROM items WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        tx.execute("UPDATE items SET name = ?2 WHERE id = ?1", params![id, name])?;
        for mut row in rows_where(&tx, "WHERE item_id = ?1", [id])? {
            if repair && row.kind == OutboxKind::Move && row.rel == to && row.state != OutboxState::Running {
                remove(&tx, row.seq)?;
                continue;
            }
            if row.rel != from && row.rel != to {
                continue;
            }
            row.rel = to.to_path_buf();
            if row.target_name.is_some() && row.target_name == was {
                row.target_name = Some(name.to_owned());
            }
            if let Some(base) = &mut row.base {
                if base.name == was {
                    base.name = Some(name.to_owned());
                }
            }
            rewrite(&tx, &row)?;
        }
        rebase(&tx, from, to)?;
        tx.commit()?;
        Ok(())
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
            remove(&tx, row.seq)?;
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
        set_snapshot(&self.conn, seq, snapshot)
    }

    /// An upload session's progress, persisted before the first byte and
    /// after each fragment (§4.8).
    pub fn outbox_set_session(&self, seq: i64, url: Option<&SessionUrl>, expires: Option<i64>, next: Option<u64>) -> Result<(), TreeError> {
        self.conn.execute(
            "UPDATE outbox SET session_url = ?2, session_expires = ?3, session_next = ?4 WHERE seq = ?1",
            params![seq, url.map(SessionUrl::as_str), expires, next.map(|n| n as i64)],
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
        // In both tables, and by both trees, so that a cycle between staging
        // and swap cannot give the items their local objects back (the
        // outbox on the bus).
        let items: Vec<String> = held.iter().filter_map(|row| row.item_id.clone()).collect();
        forget_subtrees(&tx, &items, true, &[])?;
        for row in &held {
            remove(&tx, row.seq)?;
        }
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
            remove(&tx, row.seq)?;
        }
        tx.commit()?;
        Ok(gone)
    }

    /// Commit step 2 (§3.5), after the attributes are on the file: in one
    /// transaction, the base takes Graph's answer and the local object, with
    /// `local_seq = ++outbox_seq`; follow-ups behind a create learn its item
    /// id and base; the row goes; the activity event is written. Returns the
    /// commit's `local_seq`.
    ///
    /// An answer the folder cannot hold — by its own name or kind, or by the
    /// folder it names — never takes a placed item's place away: the base
    /// keeps the place and takes the version, and the answer waits in
    /// `deferred` (see the arm below).
    pub fn outbox_commit(&mut self, seq: i64, committed: Committed<'_>, activity: Option<&ActivityRow>) -> Result<i64, TreeError> {
        let tx = self.conn.transaction()?;
        let local_seq = next_outbox_seq(&tx)?;
        let Some(committed_row) = rows_where(&tx, "WHERE seq = ?1", [seq])?.into_iter().next() else {
            return Err(TreeError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("outbox row {seq} is gone; nothing to commit"),
            )));
        };
        match committed {
            Committed::Item { row, handle } => {
                // The answer names the item as OneDrive has it now. Where
                // the folder cannot hold it there (a name too long, a
                // reserved one, a folder that is not placed here, such as
                // the Personal Vault), that is no place the row sent it to:
                // while the base places the item, it stays where the disk
                // has it, with the version just committed, and the answer
                // waits as its deferred change, for the reconcile to take
                // it off the disk once nothing in it waits. Dated this
                // commit, which therefore does not supersede it.
                let stays = match get_row(&tx, Source::Items, &row.id)? {
                    Some(base) if base_places(&tx, &row.id)? && !would_place(&tx, row)? => Some(base),
                    _ => None,
                };
                // The place the base has from now on, which is also what
                // the rows behind this one were detected against. Kept, it
                // is the place the disk has after this row: where the row
                // took the item, in the fields the user changed, and where
                // the base had it in the others. Never the place OneDrive
                // gave the item, which lives only in the deferred change:
                // a row made against it would send a name or a folder of
                // the disk's side back.
                let place = match &stays {
                    Some(base) => local_place(&committed_row, base),
                    None => (row.parent_id.clone(), row.name.clone()),
                };
                match stays {
                    Some(base) => {
                        upsert(&tx, Table::Items, &Row { parent_id: place.0.clone(), name: place.1.clone(), placement: base.placement, ..row.clone() })?;
                        wait(&tx, &row.id, Some(row), local_seq, None)?;
                    }
                    None => {
                        upsert(&tx, Table::Items, row)?;
                    }
                }
                tx.execute(
                    "UPDATE items SET local_handle = ?2, local_seq = ?3 WHERE id = ?1",
                    params![row.id, handle.map(FileHandle::encode), local_seq],
                )?;
                // A row the base does not place records no object (I1).
                forget_unplaced(&tx, [row.id.as_str()])?;
                // The follow-up behind it was detected against what this
                // commit made: that is its base now — and, behind a create,
                // its item id.
                let mut followers = rows_for(&tx, Some(&row.id), None)?;
                if let Some(inode) = committed_row.inode.clone() {
                    followers.extend(rows_for(&tx, None, Some(&inode))?);
                }
                for mut follower in followers.into_iter().filter(|r| r.seq != seq) {
                    follower.item_id = Some(row.id.clone());
                    follower.base = Some(Base { etag: row.etag.clone(), ctag: row.ctag.clone(), parent: place.0.clone(), name: Some(place.1.clone()) });
                    rewrite(&tx, &follower)?;
                }
            }
            Committed::Gone { item_id } => {
                apply(&tx, Source::Items, &[Change::Delete(item_id.to_owned())])?;
                // A delta fetched before this delete must not bring it back.
                crate::reconcile::tombstone(&tx, &[item_id], local_seq)?;
            }
        }
        remove(&tx, seq)?;
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
