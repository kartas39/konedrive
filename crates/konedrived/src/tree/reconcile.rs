//! What a read-write folder's cycle keeps between one cycle and the next
//! (`docs/design/writes.md` §9).
//!
//! - **Deferred changes.** For an item the reconcile leaves alone — one with a
//!   live outbox row, one below a folder a local move is taking elsewhere, a
//!   local change not examined yet, a replacement that has not landed — the
//!   base keeps the version the disk holds, and the delta's entry waits here,
//!   with the outbox commit count its fetch started at. The delta cursor never
//!   sends that entry again, so every cycle stages what waits here before its
//!   own delta, until the disk agrees; an outbox commit made after the fetch
//!   that brought it supersedes it (Graph's answer to the commit is newer).
//! - **Tombstones.** An item the outbox deleted in OneDrive has no base row
//!   left to carry its `local_seq`, so the delete's commit count is kept by
//!   id: a delta fetched before the delete must not bring the item back (the
//!   stale-delta guard, §3.7).

use std::collections::HashMap;

use konedrive_fs::handle::FileHandle;
use rusqlite::{params, OptionalExtension};

use super::{apply, row_from, upsert, Change, Kind, Placement, Row, Table, TreeError, TreeStore, COLUMNS, MAX_CHAIN, ROW_COLUMNS};

/// Created on every open (`IF NOT EXISTS`), so a schema-3 store made before
/// the read-write reconcile gains them without a rebuild.
pub(super) const TABLES: &str = "
    CREATE TABLE IF NOT EXISTS deferred (
        id TEXT PRIMARY KEY, seq INTEGER NOT NULL, gone INTEGER NOT NULL,
        parent_id TEXT, name TEXT, kind TEXT, size INTEGER, mtime INTEGER, etag TEXT, ctag TEXT,
        quickxor TEXT, mime TEXT, placement TEXT);
    CREATE TABLE IF NOT EXISTS outbox_gone (id TEXT PRIMARY KEY, local_seq INTEGER NOT NULL);";

/// What the outbox committed for an item after some commit count: Graph's
/// answer (its eTag), or a delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    pub etag: Option<String>,
    pub gone: bool,
}

/// Records that the outbox deleted `ids` in OneDrive at commit `local_seq`
/// (the stale-delta guard's tombstones).
pub(super) fn tombstone(tx: &rusqlite::Transaction<'_>, ids: &[&str], local_seq: i64) -> Result<(), TreeError> {
    for id in ids {
        tx.execute(
            "INSERT INTO outbox_gone (id, local_seq) VALUES (?1, ?2)
             ON CONFLICT(id) DO UPDATE SET local_seq = MAX(local_seq, excluded.local_seq)",
            params![id, local_seq],
        )?;
    }
    Ok(())
}

fn deferred_change(r: &rusqlite::Row<'_>) -> rusqlite::Result<(Change, i64)> {
    let id: String = r.get(0)?;
    let seq: i64 = r.get(1)?;
    if r.get::<_, i64>(2)? != 0 {
        return Ok((Change::Delete(id), seq));
    }
    let kind: String = r.get(5)?;
    let placement: String = r.get(12)?;
    let row = Row {
        id,
        parent_id: r.get(3)?,
        name: r.get(4)?,
        kind: if kind == "folder" { Kind::Folder } else { Kind::File },
        size: r.get::<_, i64>(6)? as u64,
        mtime: r.get(7)?,
        etag: r.get(8)?,
        ctag: r.get(9)?,
        quickxor: r.get(10)?,
        mime: r.get(11)?,
        placement: Placement::decode(&placement),
    };
    // The drive's root never waits here: it is the folder itself.
    Ok((Change::Upsert(row), seq))
}

impl TreeStore {
    /// What the outbox committed after commit count `seq`: items whose
    /// `local_seq` is newer, and tombstones.
    pub fn committed_since(&self, seq: i64) -> Result<HashMap<String, Committed>, TreeError> {
        let mut out = HashMap::new();
        let mut statement = self.conn.prepare("SELECT id, etag FROM items WHERE local_seq > ?1")?;
        for row in statement.query_map([seq], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))? {
            let (id, etag) = row?;
            out.insert(id, Committed { etag, gone: false });
        }
        let mut statement = self.conn.prepare("SELECT id FROM outbox_gone WHERE local_seq > ?1")?;
        for id in statement.query_map([seq], |r| r.get::<_, String>(0))? {
            out.entry(id?).or_insert(Committed { etag: None, gone: true });
        }
        Ok(out)
    }

    /// The deferred changes still worth staging, oldest first by id: those an
    /// outbox commit made after their fetch supersedes are dropped here.
    pub fn live_deferred(&mut self) -> Result<Vec<Change>, TreeError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM deferred
              WHERE seq < COALESCE((SELECT i.local_seq FROM items i WHERE i.id = deferred.id), 0)
                 OR seq < COALESCE((SELECT g.local_seq FROM outbox_gone g WHERE g.id = deferred.id), 0)",
            [],
        )?;
        let changes = {
            let mut statement = tx.prepare(
                "SELECT id, seq, gone, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement
                   FROM deferred ORDER BY id",
            )?;
            let rows = statement.query_map([], deferred_change)?.collect::<Result<Vec<_>, _>>()?;
            rows.into_iter().map(|(change, _)| change).collect()
        };
        tx.commit()?;
        Ok(changes)
    }

    /// A folder that is read-only now (a switch back, or a daemon that
    /// starts so): what waits is the base's at once — the read phase's cycle
    /// knows no deferred change, and the delta cursor will not send it again.
    /// The first cycle, a Full reconcile, makes the folder match. Nothing to
    /// do, and nothing done, for a folder that never was read-write.
    pub fn apply_deferred(&mut self) -> Result<usize, TreeError> {
        let changes = self.live_deferred()?;
        let tx = self.conn.transaction()?;
        apply(&tx, Table::Items, &changes)?;
        tx.execute("DELETE FROM deferred", [])?;
        tx.execute("DELETE FROM outbox_gone", [])?;
        tx.commit()?;
        Ok(changes.len())
    }

    /// The ids of every deferred change, whatever its age.
    pub fn deferred_ids(&self) -> Result<Vec<String>, TreeError> {
        let mut statement = self.conn.prepare("SELECT id FROM deferred ORDER BY id")?;
        let ids = statement.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// The deferred change of `id`, if any.
    pub fn deferred(&self, id: &str) -> Result<Option<Change>, TreeError> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, seq, gone, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement
                   FROM deferred WHERE id = ?1",
                [id],
                deferred_change,
            )
            .optional()?
            .map(|(change, _)| change))
    }

    /// Items `table` places — the item and every folder above it placed —
    /// with no local object recorded: never placed here, or forgotten by the
    /// outbox (F82 (8)) or a restore of held deletes. The root is not one.
    pub fn unplaced(&self, table: Table) -> Result<Vec<String>, TreeError> {
        let Some(root) = self.root_item_id()? else { return Ok(Vec::new()) };
        let t = table.name();
        let sql = format!(
            "WITH RECURSIVE placed(id, depth) AS (
                 SELECT ?1, 0
                 UNION ALL
                 SELECT c.id, p.depth + 1 FROM {t} c JOIN placed p ON c.parent_id = p.id
                  WHERE c.placement = 'placed' AND p.depth < {MAX_CHAIN})
             SELECT s.id FROM {t} s JOIN placed p ON s.id = p.id WHERE s.local_handle IS NULL AND s.id != ?1"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let ids = statement.query_map([&root], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Items the outbox wrote after commit count `seq` (`local_seq`): a read-write
    /// cycle looks at them again, so that the disk follows what the outbox
    /// committed (F82 (7): a move adopted with a newer cTag).
    pub fn committed_items_since(&self, seq: i64) -> Result<Vec<String>, TreeError> {
        let mut statement = self.conn.prepare("SELECT id FROM items WHERE local_seq > ?1")?;
        let ids = statement.query_map([seq], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// [`commit_staging`](Self::commit_staging) for a read-write folder, in
    /// one transaction with its deferrals: the deferred changes staged at the
    /// start of the cycle (`consumed`) are done with; each id of `defer` has
    /// what `staging` holds for it kept as deferred, and `staging` takes back
    /// what `items` has, so the base keeps the version the disk holds; each
    /// id of `content` likewise, but for its place, which the disk took.
    /// A deferral is dated `seq`, the fetch's start, or the item's last
    /// outbox commit when that is later: whatever is staged under the tree
    /// lock is at least as new as every commit on record (the stale-delta
    /// guard read the later ones again), so only a later commit supersedes it.
    /// Tombstones up to `seq` are dropped: a fetch that started after
    /// them already carries the deletes.
    pub fn commit_staging_deferring(&mut self, delta_link: &str, consumed: &[String], defer: &[String], content: &[String], seq: i64) -> Result<(), TreeError> {
        {
            let tx = self.conn.transaction()?;
            for id in consumed {
                tx.execute("DELETE FROM deferred WHERE id = ?1", [id])?;
            }
            for (id, whole) in defer.iter().map(|id| (id, true)).chain(content.iter().map(|id| (id, false))) {
                // The item's last commit, a delete's tombstone included.
                let committed: i64 = tx.query_row(
                    "SELECT MAX(COALESCE((SELECT local_seq FROM items WHERE id = ?1), 0),
                                COALESCE((SELECT local_seq FROM outbox_gone WHERE id = ?1), 0))",
                    [id],
                    |r| r.get(0),
                )?;
                let seq = seq.max(committed);
                let staged = tx.query_row(&format!("SELECT {ROW_COLUMNS} FROM staging WHERE id = ?1"), [id], row_from).optional()?;
                match staged {
                    Some(row) => tx.execute(
                        "INSERT OR REPLACE INTO deferred (id, seq, gone, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement)
                         VALUES (?1, ?2, 0, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                        params![
                            row.id,
                            seq,
                            row.parent_id,
                            row.name,
                            row.kind.as_str(),
                            row.size as i64,
                            row.mtime,
                            row.etag,
                            row.ctag,
                            row.quickxor,
                            row.mime,
                            row.placement.encode()
                        ],
                    )?,
                    None => tx.execute("INSERT OR REPLACE INTO deferred (id, seq, gone) VALUES (?1, ?2, 1)", params![id, seq])?,
                };
                if whole {
                    tx.execute("DELETE FROM staging WHERE id = ?1", [id])?;
                    tx.execute(&format!("INSERT INTO staging ({COLUMNS}) SELECT {COLUMNS} FROM items WHERE id = ?1"), [id])?;
                } else {
                    tx.execute(
                        "UPDATE staging SET (size, mtime, etag, ctag, quickxor, mime) =
                           (SELECT i.size, i.mtime, i.etag, i.ctag, i.quickxor, i.mime FROM items i WHERE i.id = staging.id)
                          WHERE id = ?1 AND EXISTS (SELECT 1 FROM items i WHERE i.id = staging.id)",
                        [id],
                    )?;
                }
            }
            tx.execute("DELETE FROM outbox_gone WHERE local_seq <= ?1", [seq])?;
            tx.commit()?;
        }
        self.commit_staging(delta_link)
    }

    /// A replacement landed (`docs/design/writes.md` §9): the new version of `id` is
    /// in place as `ctag`, on the inode `handle`. Its deferred change becomes
    /// the base if it is that very version; the handle is recorded either
    /// way. Whether the base moved.
    pub fn land_deferred(&mut self, id: &str, ctag: Option<&str>, handle: Option<&FileHandle>) -> Result<bool, TreeError> {
        let tx = self.conn.transaction()?;
        let waiting = tx
            .query_row(
                "SELECT id, seq, gone, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement
                   FROM deferred WHERE id = ?1",
                [id],
                deferred_change,
            )
            .optional()?;
        let landed = match waiting {
            Some((Change::Upsert(row), _)) if row.ctag.is_some() && row.ctag.as_deref() == ctag => {
                upsert(&tx, Table::Items, &row)?;
                tx.execute("DELETE FROM deferred WHERE id = ?1", [id])?;
                true
            }
            _ => false,
        };
        if let Some(handle) = handle {
            let stored = handle.encode();
            for table in [Table::Items, Table::Staging] {
                tx.execute(&format!("UPDATE {} SET local_handle = ?2 WHERE id = ?1", table.name()), params![id, stored])?;
            }
        }
        tx.commit()?;
        Ok(landed)
    }

    /// Rows that were to go into one of `folders` — folders gone from
    /// OneDrive, whose local directory is made again (F82 (4)) — wait for
    /// that directory's `mkdir` instead, and find its new id by their place.
    pub fn outbox_detach_parents(&self, folders: &[String]) -> Result<usize, TreeError> {
        let mut n = 0;
        for id in folders {
            n += self.conn.execute("UPDATE outbox SET target_parent = NULL WHERE target_parent = ?1", [id])?;
        }
        Ok(n)
    }

    /// Stages `changes` on top of what `staging` holds: the fresh versions a
    /// stale-delta guard fetched (§3.7).
    pub fn stage_over(&mut self, changes: &[Change]) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        apply(&tx, Table::Staging, changes)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str, parent: &str, name: &str, ctag: &str) -> Row {
        Row {
            id: id.into(),
            parent_id: Some(parent.into()),
            name: name.into(),
            kind: Kind::File,
            size: 3,
            mtime: 0,
            etag: Some(format!("e-{ctag}")),
            ctag: Some(ctag.into()),
            quickxor: None,
            mime: None,
            placement: Placement::Placed,
        }
    }

    fn root() -> Row {
        Row { id: "R".into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed }
    }

    /// A deferred change waits in `deferred` while the base keeps its row, is
    /// staged again later, and goes once an outbox commit after its fetch
    /// supersedes it.
    #[test]
    fn a_deferred_change_waits_and_a_later_commit_supersedes_it() {
        let mut s = TreeStore::in_memory().unwrap();
        s.begin_staging(false).unwrap();
        s.stage(&[Change::Root(root()), Change::Upsert(file("X", "R", "x", "c1"))]).unwrap();
        s.commit_staging("L1").unwrap();

        s.begin_staging(true).unwrap();
        s.stage(&[Change::Upsert(file("X", "R", "x", "c2"))]).unwrap();
        s.commit_staging_deferring("L2", &[], &["X".to_owned()], &[], 5).unwrap();
        assert_eq!(s.get(Table::Items, "X").unwrap().unwrap().ctag.as_deref(), Some("c1"), "the base keeps the disk's version");
        assert_eq!(s.live_deferred().unwrap(), vec![Change::Upsert(file("X", "R", "x", "c2"))]);

        // A commit at 6 (after the fetch at 5): the deferred change is older.
        s.conn.execute("UPDATE items SET local_seq = 6 WHERE id = 'X'", []).unwrap();
        assert!(s.live_deferred().unwrap().is_empty());
        assert!(s.deferred_ids().unwrap().is_empty(), "dropped for good");
    }

    /// A replacement that landed with the deferred version makes it the base,
    /// and records the new inode; another version leaves the base alone.
    #[test]
    fn a_landed_replacement_takes_its_deferred_version_into_the_base() {
        let mut s = TreeStore::in_memory().unwrap();
        s.begin_staging(false).unwrap();
        s.stage(&[Change::Root(root()), Change::Upsert(file("X", "R", "x", "c1"))]).unwrap();
        s.commit_staging("L1").unwrap();
        s.begin_staging(true).unwrap();
        s.stage(&[Change::Upsert(file("X", "R", "x", "c2"))]).unwrap();
        s.commit_staging_deferring("L2", &[], &["X".to_owned()], &[], 1).unwrap();

        assert!(!s.land_deferred("X", Some("c3"), None).unwrap(), "another version");
        assert_eq!(s.get(Table::Items, "X").unwrap().unwrap().ctag.as_deref(), Some("c1"));
        assert!(s.land_deferred("X", Some("c2"), None).unwrap());
        assert_eq!(s.get(Table::Items, "X").unwrap().unwrap().ctag.as_deref(), Some("c2"));
        assert!(s.deferred_ids().unwrap().is_empty());
    }

    /// Tombstones say what the outbox deleted after a commit count, and go
    /// with the first cycle whose fetch started after them.
    #[test]
    fn a_tombstone_is_committed_since_until_a_later_fetch_commits() {
        let mut s = TreeStore::in_memory().unwrap();
        s.begin_staging(false).unwrap();
        s.stage(&[Change::Root(root())]).unwrap();
        s.commit_staging("L1").unwrap();
        {
            let tx = s.conn.transaction().unwrap();
            tombstone(&tx, &["X"], 4).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(s.committed_since(3).unwrap().get("X"), Some(&Committed { etag: None, gone: true }));
        assert!(s.committed_since(4).unwrap().is_empty());
        s.begin_staging(true).unwrap();
        s.commit_staging_deferring("L2", &[], &[], &[], 4).unwrap();
        assert!(s.committed_since(0).unwrap().is_empty(), "pruned");
    }
}
