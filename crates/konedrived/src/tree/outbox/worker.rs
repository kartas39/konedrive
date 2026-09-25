//! The outbox worker's own transactions (`docs/design/writes.md` §5, §10, §7; task
//! the outbox worker): what it writes besides [`TreeStore::outbox_commit`]. Each is one
//! transaction, so a crash leaves all of it or none, and the row it concerns
//! is replayed from what is left (WR7).

use konedrive_fs::handle::FileHandle;
use rusqlite::{params, Connection, OptionalExtension};

use super::{insert, rewrite, rows_for, rows_where, Base, OutboxKind, OutboxRow, OutboxState, OUTBOX_SEQ, SWAP_PREFIX};
use crate::tree::{apply, upsert, ActivityRow, Change, Placement, Row, Table, TreeError, TreeStore, ACTIVITY_KEPT, MAX_CHAIN};

fn gone(seq: i64) -> TreeError {
    TreeError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, format!("outbox row {seq} is gone")))
}

/// `++outbox_seq`, inside `tx`.
fn next_local_seq(tx: &rusqlite::Transaction<'_>) -> Result<i64, TreeError> {
    let current: Option<Option<String>> = tx.query_row("SELECT value FROM meta WHERE key = ?1", [OUTBOX_SEQ], |r| r.get(0)).optional()?;
    let next = current.flatten().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0) + 1;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![OUTBOX_SEQ, next.to_string()],
    )?;
    Ok(next)
}

fn add_activity(tx: &rusqlite::Transaction<'_>, event: Option<&ActivityRow>) -> Result<(), TreeError> {
    if let Some(event) = event {
        tx.execute(
            "INSERT INTO activity (at, kind, path, detail) VALUES (?1, ?2, ?3, ?4)",
            params![event.at, event.kind, event.path, event.detail],
        )?;
        tx.execute("DELETE FROM activity WHERE id NOT IN (SELECT id FROM activity ORDER BY id DESC LIMIT ?1)", [ACTIVITY_KEPT as i64])?;
    }
    Ok(())
}

/// Forgets the local object of `id` and of everything the base has inside
/// it: until the reconcile places them again, no examination can prove them
/// deleted, so none of them becomes a delete in OneDrive (WR4).
fn forget_local(tx: &rusqlite::Transaction<'_>, id: &str) -> Result<(), TreeError> {
    // In both tables, so that a cycle between staging and swap cannot give
    // them back (the outbox on the bus).
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
    Ok(())
}

fn amend_in(conn: &Connection, seq: i64, amend: impl FnOnce(&mut OutboxRow)) -> Result<bool, TreeError> {
    let Some(mut row) = rows_where(conn, "WHERE seq = ?1", [seq])?.into_iter().next() else { return Ok(false) };
    amend(&mut row);
    rewrite(conn, &row)?;
    Ok(true)
}

impl TreeStore {
    /// Takes row `seq` for the worker: `running` from now on, so that an
    /// examination never merges into it (a follow-up waits behind it
    /// instead). Only if it is still in the state the worker chose it in;
    /// `None` otherwise. Returns the row as it is now.
    pub fn outbox_claim(&mut self, seq: i64, seen: OutboxState) -> Result<Option<OutboxRow>, TreeError> {
        let tx = self.conn.transaction()?;
        let changed = tx.execute("UPDATE outbox SET state = 'running' WHERE seq = ?1 AND state = ?2", params![seq, seen.as_str()])?;
        let row = if changed == 1 { rows_where(&tx, "WHERE seq = ?1", [seq])?.into_iter().next() } else { None };
        tx.commit()?;
        Ok(row)
    }

    /// Changes row `seq` as `amend` says, on the row as it is now — so a
    /// rebase an examination applied meanwhile stays. Whether the row
    /// was there.
    pub fn outbox_amend(&mut self, seq: i64, amend: impl FnOnce(&mut OutboxRow)) -> Result<bool, TreeError> {
        let tx = self.conn.transaction()?;
        let found = amend_in(&tx, seq, amend)?;
        tx.commit()?;
        Ok(found)
    }

    /// The content row `seq` sends is now `snapshot`. An upload session
    /// opened for other content goes with it, in the same transaction, so a
    /// session is never resumed with other bytes. Returns the session
    /// URL it dropped, to be cancelled.
    pub fn outbox_take_snapshot(&mut self, seq: i64, snapshot: &str) -> Result<Option<String>, TreeError> {
        let tx = self.conn.transaction()?;
        let dropped: Option<Option<String>> = tx
            .query_row("SELECT session_url FROM outbox WHERE seq = ?1 AND snapshot IS NOT ?2", params![seq, snapshot], |r| r.get(0))
            .optional()?;
        tx.execute(
            "UPDATE outbox SET
                 session_url = CASE WHEN snapshot IS ?2 THEN session_url END,
                 session_expires = CASE WHEN snapshot IS ?2 THEN session_expires END,
                 session_next = CASE WHEN snapshot IS ?2 THEN session_next END,
                 snapshot = ?2
               WHERE seq = ?1",
            params![seq, snapshot],
        )?;
        tx.commit()?;
        Ok(dropped.flatten())
    }

    /// Where a taking row sends its item: saved before the request that
    /// uses it, so that a replay asks for the same name (WR7).
    pub fn outbox_set_target(&self, seq: i64, parent: Option<&str>, name: Option<&str>) -> Result<(), TreeError> {
        self.conn.execute("UPDATE outbox SET target_parent = ?2, target_name = ?3 WHERE seq = ?1", params![seq, parent, name])?;
        Ok(())
    }

    /// Commit step 2 of a temporary step (F55 (7)): the item landed under a
    /// temporary name because the one it takes is still another's. In one
    /// transaction: the base takes the answer (placed, although its name is
    /// the daemon's own), `local_seq` goes up, the rows behind this one
    /// learn the item and its temporary place, and — unless one of those
    /// already takes the item on — a live `move` row takes it to
    /// `final_parent`/`final_name`. That row frees the temporary place and
    /// takes the final one, so it waits for whatever still frees that name.
    pub fn outbox_commit_temporary(
        &mut self,
        seq: i64,
        answer: &Row,
        handle: Option<&FileHandle>,
        final_parent: &str,
        final_name: &str,
        activity: Option<&ActivityRow>,
    ) -> Result<i64, TreeError> {
        let tx = self.conn.transaction()?;
        let committed = rows_where(&tx, "WHERE seq = ?1", [seq])?.into_iter().next().ok_or_else(|| gone(seq))?;
        let local_seq = next_local_seq(&tx)?;
        upsert(&tx, Table::Items, &Row { placement: Placement::Placed, ..answer.clone() })?;
        tx.execute(
            "UPDATE items SET local_handle = ?2, local_seq = ?3 WHERE id = ?1",
            params![answer.id, handle.map(FileHandle::encode), local_seq],
        )?;
        let base = Base { etag: answer.etag.clone(), ctag: answer.ctag.clone(), parent: answer.parent_id.clone(), name: Some(answer.name.clone()) };
        let mut followers = rows_for(&tx, Some(&answer.id), None)?;
        if let Some(inode) = committed.inode.clone() {
            followers.extend(rows_for(&tx, None, Some(&inode))?);
        }
        followers.retain(|r| r.seq != seq);
        for mut follower in followers.clone() {
            follower.item_id = Some(answer.id.clone());
            follower.base = Some(base.clone());
            rewrite(&tx, &follower)?;
        }
        if followers.is_empty() {
            insert(
                &tx,
                &OutboxRow {
                    seq: 0,
                    kind: OutboxKind::Move,
                    item_id: Some(answer.id.clone()),
                    inode: committed.inode.clone(),
                    rel: committed.rel.clone(),
                    base: Some(base),
                    target_parent: Some(final_parent.to_owned()),
                    target_name: Some(final_name.to_owned()),
                    state: OutboxState::Ready,
                    reason: None,
                    attempts: 0,
                    next_try: None,
                    snapshot: None,
                    session_url: None,
                    session_expires: None,
                    session_next: None,
                    confirmed: false,
                },
            )?;
        }
        tx.execute("DELETE FROM outbox WHERE seq = ?1", [seq])?;
        add_activity(&tx, activity)?;
        tx.commit()?;
        Ok(local_seq)
    }

    /// Row `seq` goes without a commit: OneDrive decided otherwise (§6: a
    /// delete of something changed there). In one transaction: the base takes
    /// `base` if given; `forget` (an item) and what is inside it lose their
    /// local object, so that what is missing here is placed again rather
    /// than deleted in OneDrive; the activity is written.
    pub fn outbox_drop(&mut self, seq: i64, base: Option<&Row>, forget: Option<&str>, activity: Option<&ActivityRow>) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        if let Some(row) = base {
            upsert(&tx, Table::Items, row)?;
        }
        if let Some(id) = forget {
            forget_local(&tx, id)?;
        }
        tx.execute("DELETE FROM outbox WHERE seq = ?1", [seq])?;
        add_activity(&tx, activity)?;
        tx.commit()?;
        Ok(())
    }

    /// Item `id` is gone from OneDrive while this machine still holds its
    /// content (§6: edit/delete, move/delete): the base forgets it and what
    /// was inside it, and its row becomes, through `amend`, the create or
    /// mkdir that uploads the local object as new — in one transaction.
    pub fn outbox_orphan(&mut self, id: &str, seq: i64, amend: impl FnOnce(&mut OutboxRow), activity: Option<&ActivityRow>) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        apply(&tx, Table::Items, &[Change::Delete(id.to_owned())])?;
        let local_seq = next_local_seq(&tx)?;
        crate::tree::reconcile::tombstone(&tx, &[id], local_seq)?;
        amend_in(&tx, seq, amend)?;
        add_activity(&tx, activity)?;
        tx.commit()?;
        Ok(())
    }

    /// A conflict copy (§6): the local version renamed beside the cloud's,
    /// recorded as a conflict of kind `copy` (full paths). The row becomes,
    /// through `amend`, the copy's create; `forget`, the item the copy came from,
    /// loses its local object, so that its name is placed again from the
    /// cloud rather than deleted there — in one transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn outbox_copied(
        &mut self,
        seq: i64,
        amend: impl FnOnce(&mut OutboxRow),
        forget: Option<&str>,
        at: i64,
        original: &str,
        copy: &str,
        activity: Option<&ActivityRow>,
    ) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        amend_in(&tx, seq, amend)?;
        if let Some(id) = forget {
            forget_local(&tx, id)?;
        }
        tx.execute(
            "INSERT INTO conflicts (rescued, at, original, kind) VALUES (?1, ?2, ?3, 'copy')
             ON CONFLICT(rescued) DO UPDATE SET at = excluded.at, original = excluded.original, kind = 'copy'",
            params![copy, at, original],
        )?;
        add_activity(&tx, activity)?;
        tx.commit()?;
        Ok(())
    }

    /// The kind of the conflict whose copy (or rescued file) is at `rescued`,
    /// a full path: `rescued` or `copy`.
    pub fn conflict_kind(&self, rescued: &str) -> Result<Option<String>, TreeError> {
        Ok(self.conn.query_row("SELECT kind FROM conflicts WHERE rescued = ?1", [rescued], |r| r.get(0)).optional()?)
    }

    /// Drops every row (`docs/design/writes.md` §2: a forced switch to
    /// read-only). The rows, for whatever marks their files carry. A rename
    /// half-done under a temporary name stays — a row sending its item to
    /// one, or one whose item the base has under one: dropped, the item
    /// would stay under that name in OneDrive, which no listing places, and
    /// its local object would go with the next reconcile.
    pub fn outbox_drop_all(&mut self) -> Result<Vec<OutboxRow>, TreeError> {
        let tx = self.conn.transaction()?;
        let swapping = format!("{SWAP_PREFIX}%");
        let dropped = "WHERE NOT (COALESCE(target_name, '') LIKE ?1 \
                       OR COALESCE(item_id, '') IN (SELECT id FROM items WHERE name LIKE ?1))";
        let rows = rows_where(&tx, dropped, [&swapping])?;
        tx.execute(&format!("DELETE FROM outbox {dropped}"), [&swapping])?;
        tx.commit()?;
        Ok(rows)
    }

    /// Blocked rows whose reason is one of `reasons` are ready again: the
    /// quota changed, the account signed in again.
    pub fn outbox_unblock(&self, reasons: &[&str]) -> Result<usize, TreeError> {
        let mut n = 0;
        for reason in reasons {
            n += self
                .conn
                .execute("UPDATE outbox SET state = 'ready', next_try = NULL WHERE state = 'blocked' AND reason = ?1", [reason])?;
        }
        Ok(n)
    }

    /// Rows in backoff are due now (`Refresh()`).
    pub fn outbox_retry_now(&self) -> Result<usize, TreeError> {
        Ok(self.conn.execute("UPDATE outbox SET next_try = 0 WHERE state IN ('retry', 'waiting')", [])?)
    }

    /// Every item of the base, for tests that seed a fake OneDrive from it.
    #[cfg(test)]
    pub fn all_items(&self) -> Result<Vec<Row>, TreeError> {
        let sql = format!("SELECT {} FROM items ORDER BY id", crate::tree::ROW_COLUMNS);
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map([], crate::tree::row_from)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
