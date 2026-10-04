//! The outbox worker's own transactions (`docs/design/writes.md` §5, §10, §7; task
//! the outbox worker): what it writes besides [`TreeStore::outbox_commit`]. Each is one
//! transaction, so a crash leaves all of it or none, and the row it concerns
//! is replayed from what is left (WR7).

use konedrive_fs::handle::FileHandle;
use rusqlite::{params, Connection, OptionalExtension};

use super::{insert, remove, rewrite, rows_for, rows_where, set_snapshot, BadItem, Base, OutboxKind, OutboxRow, OutboxState, Reason, SessionUrl, Snapshot, SWAP_PREFIX};
use crate::conflicts::ConflictKind;
use crate::forget::{forget_subtrees, forget_unplaced};
use crate::meta::next_outbox_seq;
use crate::model::{upsert, Change, Placement, Row, Table};
use crate::source::Source;
use crate::staging::apply;
use crate::{ActivityRow, TreeError, TreeStore, ACTIVITY_KEPT};

/// A conflict copy, as [`TreeStore::outbox_copied`] records it.
#[derive(Debug, Clone, Copy)]
pub struct ConflictCopy<'a> {
    /// The item the copy came from, if it has one.
    pub forget: Option<&'a str>,
    /// When, unix seconds.
    pub at: i64,
    /// The full path the cloud's version keeps.
    pub original: &'a str,
    /// The full path the local version was renamed to.
    pub copy: &'a str,
}

fn gone(seq: i64) -> TreeError {
    TreeError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, format!("outbox row {seq} is gone")))
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

/// Forgets the local object of `id` and of everything the base or the new
/// tree has inside it: until the reconcile places them again, no examination
/// can prove them deleted, so none of them becomes a delete in OneDrive (WR4).
/// In both tables, so that a cycle between staging and swap cannot give them
/// back (the outbox on the bus).
fn forget_local(tx: &rusqlite::Transaction<'_>, id: &str) -> Result<(), TreeError> {
    forget_subtrees(tx, &[id.to_owned()], true, &[])
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
    pub fn outbox_take_snapshot(&mut self, seq: i64, snapshot: Snapshot) -> Result<Option<SessionUrl>, TreeError> {
        let tx = self.conn.transaction()?;
        let Some(row) = rows_where(&tx, "WHERE seq = ?1", [seq])?.into_iter().next() else { return Ok(None) };
        if row.snapshot_is(snapshot) {
            return Ok(None);
        }
        set_snapshot(&tx, seq, Some(snapshot))?;
        tx.execute("UPDATE outbox SET session_url = NULL, session_expires = NULL, session_next = NULL WHERE seq = ?1", [seq])?;
        tx.commit()?;
        Ok(row.session_url)
    }

    /// The item a new file's upload left in OneDrive with other content
    /// than was sent, still to be deleted before the file goes again
    /// ([`BadItem`]). Kept in columns of their own, which no change of the
    /// row's state or reason and no merge of an examination writes: they go
    /// only with [`outbox_set_bad_item`](Self::outbox_set_bad_item), or with
    /// the row.
    pub fn outbox_bad_item(&self, seq: i64) -> Result<Option<BadItem>, TreeError> {
        let bad: Option<(Option<String>, Option<String>, Option<String>)> = self
            .conn
            .query_row("SELECT bad_item, bad_item_ctag, bad_item_etag FROM outbox WHERE seq = ?1", [seq], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        Ok(bad.and_then(|(id, ctag, etag)| Some(BadItem { id: id?, ctag, etag })))
    }

    /// Remembers the bad item of row `seq`, or forgets it (`None`): deleted,
    /// found gone, changed in OneDrive since, or adopted.
    pub fn outbox_set_bad_item(&self, seq: i64, item: Option<&BadItem>) -> Result<(), TreeError> {
        let (id, ctag, etag) = (item.map(|i| &i.id), item.and_then(|i| i.ctag.as_ref()), item.and_then(|i| i.etag.as_ref()));
        self.conn.execute("UPDATE outbox SET bad_item = ?2, bad_item_ctag = ?3, bad_item_etag = ?4 WHERE seq = ?1", params![seq, id, ctag, etag])?;
        Ok(())
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
        let local_seq = next_outbox_seq(&tx)?;
        upsert(&tx, Table::Items, &Row { placement: Placement::Placed, ..answer.clone() })?;
        crate::reconcile::joins_leaving(&tx, &answer.id, answer.parent_id.as_deref())?;
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
                    size: None,
                },
            )?;
        }
        remove(&tx, seq)?;
        add_activity(&tx, activity)?;
        tx.commit()?;
        Ok(local_seq)
    }

    /// Row `seq` opened the upload session `url` (issue #47): listed, with
    /// the place `place` (parent id, name) a new file's session holds, and the
    /// row's session from now on, at offset 0 — in one transaction, before
    /// any byte is sent. A row gone meanwhile leaves the session listed and
    /// pointed at by nothing: given up, and cancelled.
    pub fn outbox_open_session(&mut self, seq: i64, url: &SessionUrl, expires: Option<i64>, place: Option<(&str, &str)>, now: i64) -> Result<(), TreeError> {
        let url = url.as_str();
        let tx = self.conn.transaction()?;
        let (parent, name) = place.unzip();
        tx.execute(
            "INSERT OR REPLACE INTO upload_sessions (url, parent, name, opened) VALUES (?1, ?2, ?3, ?4)",
            params![url, parent, name, now],
        )?;
        tx.execute(
            "UPDATE outbox SET session_url = ?2, session_expires = ?3, session_next = 0 WHERE seq = ?1",
            params![seq, url, expires],
        )?;
        tx.execute("DELETE FROM upload_openings WHERE seq = ?1", [seq])?;
        tx.commit()?;
        Ok(())
    }

    /// Row `seq` is about to open a new file's session at (`parent`, `name`)
    /// (issue #84): recorded before the request, so that a stop before its
    /// URL is persisted still knows the empty placeholder it may leave there.
    /// Recorded again at the same place (the name without case, as OneDrive
    /// compares), it keeps its first time: the placeholder of an earlier
    /// opening is as much this row's. The attempt's time is `last` until its
    /// answer says otherwise. `Some(last)`: carried — a record of an earlier
    /// attempt here, its latest unknown outcome at `last`; `None`: this call
    /// made it. A record at another place is kept without a row.
    pub fn outbox_record_opening(&self, seq: i64, parent: &str, name: &str, now: i64) -> Result<Option<i64>, TreeError> {
        // In Rust, since SQLite's `lower` is ASCII only.
        let earlier: Option<(String, String, i64, i64)> = self
            .conn
            .query_row("SELECT parent, name, at, COALESCE(last, at) FROM upload_openings WHERE seq = ?1", [seq], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        let (at, carried) = match earlier {
            Some((p, n, at, last)) if p == parent && n.to_lowercase() == name.to_lowercase() => (at, Some(last)),
            Some((p, n, at, last)) => {
                self.conn.execute(
                    "INSERT INTO upload_openings_left (parent, name, at, last, left_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![p, n, at, last, now],
                )?;
                (now, None)
            }
            None => (now, None),
        };
        self.conn.execute(
            "INSERT INTO upload_openings (seq, parent, name, at, last) VALUES (?1, ?2, ?3, ?4, ?5)
               ON CONFLICT(seq) DO UPDATE SET parent = excluded.parent, name = excluded.name, at = excluded.at, last = excluded.last",
            params![seq, parent, name, at, now],
        )?;
        Ok(carried)
    }

    /// Row `seq`'s opening was answered for certain: no placeholder of it.
    /// A record this attempt made goes; a carried one (`carried`, its last
    /// unknown outcome) is kept as it was before the attempt.
    pub fn outbox_opening_answered(&self, seq: i64, carried: Option<i64>) -> Result<(), TreeError> {
        match carried {
            Some(last) => self.conn.execute("UPDATE upload_openings SET last = ?2 WHERE seq = ?1", params![seq, last])?,
            None => self.conn.execute("DELETE FROM upload_openings WHERE seq = ?1", [seq])?,
        };
        Ok(())
    }

    /// The openings recorded at `name` (without case) in `parent`, with a
    /// row or without, are resolved — their placeholder deleted, the name
    /// free, or its holder not theirs: every one goes.
    pub fn upload_openings_clear_at(&self, parent: &str, name: &str) -> Result<(), TreeError> {
        let lower = name.to_lowercase();
        for table in ["upload_openings", "upload_openings_left"] {
            let mut statement = self.conn.prepare(&format!("SELECT rowid, name FROM {table} WHERE parent = ?1"))?;
            let rows = statement.query_map([parent], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            for (rowid, _) in rows.into_iter().filter(|(_, n)| n.to_lowercase() == lower) {
                self.conn.execute(&format!("DELETE FROM {table} WHERE rowid = ?1"), [rowid])?;
            }
        }
        Ok(())
    }

    /// Records without a row older than [`OPENING_LEFT_KEEP`](super::OPENING_LEFT_KEEP) go.
    pub fn upload_openings_expire(&self, now: i64) -> Result<(), TreeError> {
        self.conn.execute("DELETE FROM upload_openings_left WHERE left_at < ?1", [now - super::OPENING_LEFT_KEEP])?;
        Ok(())
    }

    /// The windows of the openings recorded at `name` (without case) in
    /// `parent`, with a row or without, whose URL never came: each record's
    /// first time and its latest attempt whose outcome is not known. Only
    /// within one of them may an empty placeholder there be this folder's —
    /// never between two (issue #89).
    pub fn upload_opening_windows(&self, parent: &str, name: &str) -> Result<Vec<(i64, i64)>, TreeError> {
        let lower = name.to_lowercase();
        let mut windows = Vec::new();
        for sql in [
            "SELECT name, at, COALESCE(last, at) FROM upload_openings WHERE parent = ?1",
            "SELECT name, at, last FROM upload_openings_left WHERE parent = ?1",
        ] {
            let mut statement = self.conn.prepare(sql)?;
            let rows = statement
                .query_map([parent], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            windows.extend(rows.into_iter().filter(|(n, _, _)| n.to_lowercase() == lower).map(|(_, at, last)| (at, last)));
        }
        Ok(windows)
    }

    /// The earliest time of the openings recorded at `name` (without case)
    /// in `parent`.
    #[cfg(any(test, feature = "testing"))]
    pub fn upload_opening_at(&self, parent: &str, name: &str) -> Result<Option<i64>, TreeError> {
        Ok(self.upload_opening_windows(parent, name)?.into_iter().map(|(at, _)| at).min())
    }

    /// Row `seq`'s session completed, or is gone: the row and the list
    /// forget it, with nothing to cancel.
    pub fn outbox_session_ended(&mut self, seq: i64) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        let url: Option<Option<String>> = tx.query_row("SELECT session_url FROM outbox WHERE seq = ?1", [seq], |r| r.get(0)).optional()?;
        if let Some(url) = url.flatten() {
            tx.execute("DELETE FROM upload_sessions WHERE url = ?1", [&url])?;
        }
        tx.execute("UPDATE outbox SET session_url = NULL, session_expires = NULL, session_next = NULL WHERE seq = ?1", [seq])?;
        tx.commit()?;
        Ok(())
    }

    /// Session `url` was cancelled, or found gone: off the list.
    pub fn upload_session_closed(&self, url: &SessionUrl) -> Result<(), TreeError> {
        self.conn.execute("DELETE FROM upload_sessions WHERE url = ?1", [url.as_str()])?;
        Ok(())
    }

    /// Up to `limit` listed sessions no row points at any more: given up —
    /// the content changed, the file went, the row left the outbox, or a
    /// cancel failed — and so to be cancelled.
    pub fn upload_sessions_given_up(&self, limit: usize) -> Result<Vec<SessionUrl>, TreeError> {
        let mut statement = self.conn.prepare(
            "SELECT url FROM upload_sessions u
               WHERE NOT EXISTS (SELECT 1 FROM outbox o WHERE o.session_url = u.url)
               ORDER BY opened LIMIT ?1",
        )?;
        let urls = statement.query_map([limit as i64], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
        Ok(urls.into_iter().map(SessionUrl::new).collect())
    }

    /// The listed sessions of a new file named `name` (without case) in
    /// `parent`, each with the row that points at it, if any: what holds
    /// that name in OneDrive with an empty placeholder.
    pub fn upload_sessions_at(&self, parent: &str, name: &str) -> Result<Vec<(SessionUrl, Option<i64>)>, TreeError> {
        let mut statement = self.conn.prepare(
            "SELECT url, name, (SELECT seq FROM outbox o WHERE o.session_url = u.url LIMIT 1)
               FROM upload_sessions u WHERE parent = ?1",
        )?;
        let lower = name.to_lowercase();
        let rows = statement
            .query_map([parent], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows.into_iter().filter(|(_, n, _)| n.as_deref().is_some_and(|n| n.to_lowercase() == lower)).map(|(url, _, seq)| (SessionUrl::new(url), seq)).collect())
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
            forget_unplaced(&tx, [row.id.as_str()])?;
        }
        if let Some(id) = forget {
            forget_local(&tx, id)?;
        }
        remove(&tx, seq)?;
        add_activity(&tx, activity)?;
        tx.commit()?;
        Ok(())
    }

    /// A `create` or `mkdir` whose local object is gone before it landed
    /// (issue #27): in one transaction, row `seq` goes, and so do the rows
    /// `behind` it of the same object that never got an item id — nothing
    /// of it reached OneDrive. A row of that object recorded since the worker
    /// looked (the object back in a place it was not looked for) is not
    /// dropped: an `update` or `move` becomes the upload of the object as new,
    /// the kind of row `seq` was; a removal of it is left to leave on its own.
    pub fn outbox_drop_unsent(&mut self, seq: i64, behind: &[i64], activity: Option<&ActivityRow>) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        let Some(dropped) = rows_where(&tx, "WHERE seq = ?1", [seq])?.into_iter().next() else {
            return Ok(());
        };
        let later = match &dropped.inode {
            Some(inode) => rows_for(&tx, None, Some(inode))?,
            None => Vec::new(),
        };
        for mut row in later.into_iter().filter(|r| r.seq != seq) {
            if behind.contains(&row.seq) {
                remove(&tx, row.seq)?;
            } else if matches!(row.kind, OutboxKind::Update | OutboxKind::Move) {
                row.kind = dropped.kind;
                row.base = None;
                row.snapshot = None;
                row.session_url = None;
                row.session_expires = None;
                row.session_next = None;
                rewrite(&tx, &row)?;
            }
        }
        remove(&tx, seq)?;
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
        apply(&tx, Source::Items, &[Change::Delete(id.to_owned())])?;
        let local_seq = next_outbox_seq(&tx)?;
        crate::reconcile::tombstone(&tx, &[id], local_seq)?;
        amend_in(&tx, seq, amend)?;
        add_activity(&tx, activity)?;
        tx.commit()?;
        Ok(())
    }

    /// A conflict copy (§6): the local version renamed beside the cloud's,
    /// recorded as a conflict of kind `copy` ([`ConflictCopy`]). The row
    /// becomes, through `amend`, the copy's create; the item the copy came
    /// from loses its local object, so that its name is placed again from
    /// the cloud rather than deleted there — in one transaction.
    pub fn outbox_copied(&mut self, seq: i64, amend: impl FnOnce(&mut OutboxRow), copied: &ConflictCopy<'_>, activity: Option<&ActivityRow>) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        amend_in(&tx, seq, amend)?;
        if let Some(id) = copied.forget {
            forget_local(&tx, id)?;
        }
        tx.execute(
            &format!(
                "INSERT INTO conflicts (rescued, at, original, kind) VALUES (?1, ?2, ?3, '{kind}')
             ON CONFLICT(rescued) DO UPDATE SET at = excluded.at, original = excluded.original, kind = '{kind}'",
                kind = ConflictKind::Copy.as_str()
            ),
            params![copied.copy, copied.at, copied.original],
        )?;
        add_activity(&tx, activity)?;
        tx.commit()?;
        Ok(())
    }

    /// The kind of the conflict whose copy (or rescued file) is at `rescued`,
    /// a full path: `rescued` or `copy`.
    #[cfg(any(test, feature = "testing"))]
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
        for row in &rows {
            remove(&tx, row.seq)?;
        }
        tx.commit()?;
        Ok(rows)
    }

    /// Blocked rows whose reason is one of `reasons` are ready again: the
    /// quota changed, the account signed in again.
    pub fn outbox_unblock(&self, reasons: &[Reason]) -> Result<usize, TreeError> {
        let mut n = 0;
        for reason in reasons {
            n += self
                .conn
                .execute("UPDATE outbox SET state = 'ready', next_try = NULL WHERE state = 'blocked' AND reason = ?1", [reason.to_string()])?;
        }
        Ok(n)
    }

    /// Rows blocked with reason `from` are ready again with reason `to`, in
    /// their places: rows an earlier version blocked on a full OneDrive wait
    /// for space now (issue #2). How many.
    pub fn outbox_space_convert(&self, from: &Reason, to: &Reason) -> Result<usize, TreeError> {
        Ok(self.conn.execute(
            "UPDATE outbox SET state = 'ready', reason = ?2, next_try = NULL WHERE state = 'blocked' AND reason = ?1",
            [from.to_string(), to.to_string()],
        )?)
    }

    /// Rows in backoff are due now (`Refresh()`).
    pub fn outbox_retry_now(&self) -> Result<usize, TreeError> {
        Ok(self.conn.execute("UPDATE outbox SET next_try = 0 WHERE state IN ('retry', 'waiting')", [])?)
    }

    /// Rows written as they are, without merging: the bench seeds a large outbox fast.
    #[cfg(any(test, feature = "testing"))]
    pub fn bench_insert(&mut self, rows: &[OutboxRow]) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        for row in rows {
            insert(&tx, row)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every item of the base, for tests that seed a fake OneDrive from it.
    #[cfg(any(test, feature = "testing"))]
    pub fn all_items(&self) -> Result<Vec<Row>, TreeError> {
        let sql = format!("SELECT {} FROM items ORDER BY id", crate::model::ROW_COLUMNS);
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map([], crate::model::row_from)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
