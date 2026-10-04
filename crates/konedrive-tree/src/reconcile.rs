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
//! - **What is leaving.** An item that stopped being placed while OneDrive
//!   still has it (a name too long, the Personal Vault...) keeps its object
//!   on disk while what is inside it waits to be uploaded (issue #104):
//!   where that object is, by item id, until a cycle removes it.
//! - **Tombstones.** An item the outbox deleted in OneDrive has no base row
//!   left to carry its `local_seq`, so the delete's commit count is kept by
//!   id: a delta fetched before the delete must not bring the item back (the
//!   stale-delta guard, §3.7).

use std::collections::HashMap;

use konedrive_fs::handle::FileHandle;
use rusqlite::{params, OptionalExtension};

use crate::model::{at, placed, row_from, Change, NewTree, Row, Table, COLUMNS, ROW_COLUMNS, ROW_WIDTH};
#[cfg(test)]
use crate::model::{Kind, Placement};
use crate::query::get_row;
use crate::source::{Source, UNTOUCHED};
use crate::staging::{apply, swap};
use crate::{TreeError, TreeStore};

/// A read-write cycle's delta, staged ([`TreeStore::stage_rw`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RwStaged {
    /// The ids to reconcile: what the new tree changes, what the outbox
    /// committed since, and what has no local object on record.
    pub ids: Vec<String>,
    /// The deferred changes staged again, which the cycle's commit is done
    /// with ([`Deferrals::consumed`]).
    pub consumed: Vec<String>,
}

/// What a read-write cycle's commit does with the deferred changes
/// ([`TreeStore::commit_staging_deferring`]).
#[derive(Debug, Clone, Copy)]
pub struct Deferrals<'a> {
    /// The deferred changes staged at the start of the cycle: done with.
    pub consumed: &'a [String],
    /// Ids left alone whole: what `staging` holds for each waits as
    /// deferred, and the base keeps what it has.
    pub whole: &'a [String],
    /// Ids whose content is left alone: likewise, but for the place, which
    /// the disk took.
    pub content: &'a [String],
    /// The outbox commit count the cycle's fetch started at.
    pub fetched_at: i64,
}

/// Something leaving ([`TreeStore::leaving_with_handles`]): the item, where
/// its object stays, and that object's file handle (none in a store from
/// before the handle was kept).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaving {
    pub id: String,
    pub rel: std::path::PathBuf,
    pub handle: Option<FileHandle>,
}

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

/// The change of item `id` waits as deferred, dated commit count `seq`: the
/// row OneDrive has of it now, or, with none, its removal there.
pub(super) fn wait(tx: &rusqlite::Transaction<'_>, id: &str, row: Option<&Row>, seq: i64) -> Result<(), TreeError> {
    match row {
        Some(row) => tx.execute(
            "INSERT OR REPLACE INTO deferred (id, seq, gone, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement)
             VALUES (?1, ?2, 0, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                id,
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
    Ok(())
}

/// What a deferred change's query selects: the tree's row, read as every
/// row is ([`row_from`]), then the change's own columns.
fn deferred_columns() -> String {
    format!("{ROW_COLUMNS}, seq, gone")
}

const SEQ: usize = ROW_WIDTH;
const GONE: usize = ROW_WIDTH + 1;

fn deferred_change(r: &rusqlite::Row<'_>) -> rusqlite::Result<(Change, i64)> {
    let seq: i64 = r.get(SEQ)?;
    if r.get::<_, i64>(GONE)? != 0 {
        // A removal keeps the id alone: the row's other columns are null.
        return Ok((Change::Delete(r.get(at::ID)?), seq));
    }
    // The drive's root never waits here: it is the folder itself.
    Ok((Change::Upsert(row_from(r)?), seq))
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
    pub(crate) fn live_deferred(&mut self) -> Result<Vec<Change>, TreeError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM deferred
              WHERE seq < COALESCE((SELECT i.local_seq FROM items i WHERE i.id = deferred.id), 0)
                 OR seq < COALESCE((SELECT g.local_seq FROM outbox_gone g WHERE g.id = deferred.id), 0)",
            [],
        )?;
        let changes = {
            let mut statement = tx.prepare(&format!("SELECT {} FROM deferred ORDER BY id", deferred_columns()))?;
            let rows = statement.query_map([], deferred_change)?.collect::<Result<Vec<_>, _>>()?;
            rows.into_iter().map(|(change, _)| change).collect()
        };
        tx.commit()?;
        Ok(changes)
    }

    /// A folder that is read-only now (a switch back, or a daemon that
    /// starts so): what waits is the base's at once — the read phase's cycle
    /// knows no deferred change, and the delta cursor will not send it again.
    /// A row placed again by it carries no local object (issue #104).
    /// The first cycle, a Full reconcile, makes the folder match. Nothing to
    /// do, and nothing done, for a folder that never was read-write.
    pub fn apply_deferred(&mut self) -> Result<usize, TreeError> {
        let changes = self.live_deferred()?;
        let tx = self.conn.transaction()?;
        apply(&tx, Source::Items, &changes)?;
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
            .query_row(&format!("SELECT {} FROM deferred WHERE id = ?1", deferred_columns()), [id], deferred_change)
            .optional()?
            .map(|(change, _)| change))
    }

    /// Items `table` places — the item and every folder above it placed —
    /// with no local object recorded: never placed here, or forgotten by the
    /// outbox (F82 (8)) or a restore of held deletes. The root is not one.
    /// Read from those with no local object alone (an index of `items`, and
    /// what a delta staged), each placed or not by one query for the lot
    /// (issue #39): no walk of the whole tree.
    pub fn unplaced(&self, table: Table) -> Result<Vec<String>, TreeError> {
        let own = placed("placement");
        let start = match self.source(table) {
            Source::Items => format!("SELECT id, parent_id, name, placement FROM items WHERE local_handle IS NULL AND {own}"),
            Source::Whole => format!("SELECT id, parent_id, name, placement FROM staging WHERE local_handle IS NULL AND {own}"),
            Source::Overlay => format!(
                "SELECT id, parent_id, name, placement FROM staging WHERE local_handle IS NULL AND {own}
                 UNION ALL
                 SELECT id, parent_id, name, placement FROM items p
                  WHERE local_handle IS NULL AND {own} AND {UNTOUCHED}"
            ),
        };
        Ok(self.chains(table, &start, &[])?.into_iter().filter(|c| c.above && c.own).map(|c| c.id).collect())
    }

    /// Items the outbox wrote after commit count `seq` (`local_seq`): a read-write
    /// cycle looks at them again, so that the disk follows what the outbox
    /// committed (F82 (7): a move adopted with a newer cTag).
    pub(crate) fn committed_items_since(&self, seq: i64) -> Result<Vec<String>, TreeError> {
        let mut statement = self.conn.prepare_cached("SELECT id FROM items WHERE local_seq > ?1")?;
        let ids = statement.query_map([seq], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// [`commit_staging`](Self::commit_staging) for a read-write folder, in
    /// one transaction with its deferrals ([`Deferrals`]): the deferred
    /// changes staged at the start of the cycle are done with; each id left
    /// alone whole has what `staging` holds for it kept as deferred, and
    /// `staging` takes back what `items` has, so the base keeps the version
    /// the disk holds; each id whose content is left alone likewise, but for
    /// its place, which the disk took.
    /// A deferral is dated the fetch's start, or the item's last
    /// outbox commit when that is later: whatever is staged under the tree
    /// lock is at least as new as every commit on record (the stale-delta
    /// guard read the later ones again), so only a later commit supersedes it.
    /// Tombstones up to the fetch's start are dropped: a fetch that started after
    /// them already carries the deletes.
    pub fn commit_staging_deferring(&mut self, delta_link: &str, deferrals: &Deferrals<'_>) -> Result<(), TreeError> {
        let Deferrals { consumed, whole: defer, content, fetched_at: seq } = *deferrals;
        let source = self.source(Table::Staging);
        let whole = self.whole;
        {
            let tx = self.conn.transaction()?;
            for id in consumed {
                tx.execute("DELETE FROM deferred WHERE id = ?1", [id])?;
            }
            for (id, all) in defer.iter().map(|id| (id, true)).chain(content.iter().map(|id| (id, false))) {
                // The item's last commit, a delete's tombstone included.
                let committed: i64 = tx.query_row(
                    "SELECT MAX(COALESCE((SELECT local_seq FROM items WHERE id = ?1), 0),
                                COALESCE((SELECT local_seq FROM outbox_gone WHERE id = ?1), 0))",
                    [id],
                    |r| r.get(0),
                )?;
                let seq = seq.max(committed);
                wait(&tx, id, get_row(&tx, source, id)?.as_ref(), seq)?;
                if all && source == Source::Overlay {
                    // Staged over `items`: what it has shows through again.
                    tx.execute("DELETE FROM staging WHERE id = ?1", [id])?;
                    tx.execute("DELETE FROM staging_gone WHERE id = ?1", [id])?;
                } else if all {
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
            // The swap in the same transaction (TR1): a swap that fails
            // leaves the deferrals, and the tombstones, as they were.
            swap(&tx, whole, delta_link)?;
            tx.commit()?;
        }
        self.whole = false;
        Ok(())
    }

    /// A replacement landed (`docs/design/writes.md` §9): the new version of `id` is
    /// in place as `ctag`, on the inode `handle`. Its deferred change becomes
    /// the base if it is that very version; the handle is recorded either
    /// way. Whether the base moved.
    pub fn land_deferred(&mut self, id: &str, ctag: Option<&str>, handle: Option<&FileHandle>) -> Result<bool, TreeError> {
        let tx = self.conn.transaction()?;
        let waiting = tx
            .query_row(&format!("SELECT {} FROM deferred WHERE id = ?1", deferred_columns()), [id], deferred_change)
            .optional()?;
        let landed = match waiting {
            Some((Change::Upsert(row), _)) if row.ctag.is_some() && row.ctag.as_deref() == ctag => {
                // Through `apply`, so that a row placed again carries no
                // local object but the one landed here (issue #104).
                apply(&tx, Source::Items, &[Change::Upsert(row)])?;
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
            crate::forget::forget_unplaced(&tx, [id])?;
        }
        tx.commit()?;
        Ok(landed)
    }

    /// Rows that were to go into one of `folders` — folders gone from
    /// OneDrive, whose local directory stays, holding local work, and is
    /// made again there (F116) — wait for that directory's `mkdir`
    /// instead, and find its new id by their place.
    pub fn outbox_detach_parents(&self, folders: &[String]) -> Result<usize, TreeError> {
        let mut n = 0;
        for id in folders {
            n += self.conn.execute("UPDATE outbox SET target_parent = NULL WHERE target_parent = ?1", [id])?;
        }
        Ok(n)
    }

    /// A read-write cycle's delta, staged (`remote::listing::rw`): what waits
    /// is staged again before `changes`. The ids to reconcile — what the new
    /// tree changes, what the outbox committed after commit count `since`,
    /// and what has no local object on record — and the deferred changes
    /// consumed ([`RwStaged`]); `None`, with nothing staged, when there is
    /// nothing to do and no `full` reconcile is asked for.
    ///
    /// An idle cycle reads nothing whole (issue #39): the deferred changes,
    /// the outbox by item id, `items` by `local_seq` and by what has no
    /// local object, each through an index.
    pub fn stage_rw(&mut self, changes: &[Change], since: i64, full: bool) -> Result<Option<RwStaged>, TreeError> {
        let deferred = self.live_deferred()?;
        let waiting = {
            let mut row_of = self.conn.prepare_cached("SELECT 1 FROM outbox WHERE item_id = ?1 LIMIT 1")?;
            let mut waiting = true;
            for change in &deferred {
                if !row_of.exists([change.id()])? {
                    waiting = false;
                    break;
                }
            }
            waiting
        };
        let revisit = self.committed_items_since(since)?;
        let unplaced = self.unplaced(Table::Items)?;
        let leaving = self.leaving()?;
        if !full && changes.is_empty() && waiting && revisit.is_empty() && unplaced.is_empty() && leaving.is_empty() {
            return Ok(None);
        }
        let consumed: Vec<String> = deferred.iter().map(|c| c.id().to_owned()).collect();
        self.begin_staging(NewTree::Delta)?;
        self.stage(&deferred)?;
        self.stage(changes)?;
        let mut ids: std::collections::BTreeSet<String> = self.changed_ids()?.into_iter().collect();
        ids.extend(revisit);
        ids.extend(self.unplaced(Table::Staging)?);
        Ok(Some(RwStaged { ids: ids.into_iter().collect(), consumed }))
    }

    /// Item `id` stopped being placed, and its object stays at `rel` for now
    /// (issue #104).
    /// The items the base has at and below it are remembered with it: one of
    /// them found inside it once the base no longer has it was removed in
    /// OneDrive, and is never uploaded as new (review fixes, round 2).
    /// Its object's file handle is kept too: it finds the object again when
    /// its path is gone (a parent renamed here, not examined yet).
    pub fn leaving_add(&mut self, id: &str, rel: &std::path::Path, handle: Option<&FileHandle>) -> Result<(), TreeError> {
        use std::os::unix::ffi::OsStrExt;
        let mut items = self.descendants(Table::Items, id)?;
        items.push(id.to_owned());
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO leaving (id, rel, handle) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET rel = excluded.rel, handle = COALESCE(excluded.handle, leaving.handle)",
            params![id, rel.as_os_str().as_bytes(), handle.map(FileHandle::encode)],
        )?;
        {
            let mut had = tx.prepare_cached("INSERT OR REPLACE INTO leaving_items (id, leaving) VALUES (?1, ?2)")?;
            for item in &items {
                had.execute(params![item, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// What is leaving, each with its object's place and file handle.
    pub fn leaving_with_handles(&self) -> Result<Vec<Leaving>, TreeError> {
        use std::os::unix::ffi::OsStrExt;
        let mut statement = self.conn.prepare_cached("SELECT id, rel, handle FROM leaving ORDER BY id")?;
        let rows = statement
            .query_map([], |r| {
                let rel: Vec<u8> = r.get(1)?;
                let handle: Option<Vec<u8>> = r.get(2)?;
                Ok(Leaving { id: r.get(0)?, rel: std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&rel)), handle: handle.as_deref().and_then(FileHandle::decode) })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The items the base has now at and below the leaving item `id` are
    /// remembered with it too: one moved in and committed there since, once
    /// OneDrive removes it, goes as what was in it does (issue #104).
    pub fn leaving_refresh_items(&mut self, id: &str) -> Result<(), TreeError> {
        let items = self.descendants(Table::Items, id)?;
        let tx = self.conn.transaction()?;
        {
            let mut had = tx.prepare_cached("INSERT OR IGNORE INTO leaving_items (id, leaving) VALUES (?1, ?2)")?;
            for item in &items {
                had.execute(params![item, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The leaving object of item `id` is now the inode `handle` — an
    /// editor's save by rename at its place (issue #104).
    pub fn leaving_set_handle(&self, id: &str, handle: &FileHandle) -> Result<(), TreeError> {
        self.conn.execute("UPDATE leaving SET handle = ?2 WHERE id = ?1", params![id, handle.encode()])?;
        Ok(())
    }

    /// The inodes `handles` are no longer followed as leaving objects: a
    /// file with other names that is about to lose the name it leaves
    /// under, whose other names are the user's own and must never be taken
    /// for it. Its row stays, followed by its path only.
    pub fn leaving_forget_handles(&mut self, handles: &[FileHandle]) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        {
            let mut forget = tx.prepare_cached("UPDATE leaving SET handle = NULL WHERE handle = ?1")?;
            for handle in handles {
                forget.execute([handle.encode()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Whether item `id` is placed — by the base or by the new tree — at
    /// another place than `rel`: then an object carrying its id at `rel` may
    /// be the user's (the copy placed again, moved there), and only the
    /// leaving object's handle tells (issue #104).
    pub fn placed_elsewhere(&self, id: &str, rel: &std::path::Path) -> Result<bool, TreeError> {
        for table in [Table::Items, Table::Staging] {
            if self.locate(table, id)?.is_some_and(|l| l.placed && l.rel != rel) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The file handle of the leaving object of item `id`, if one was taken.
    pub fn leaving_handle(&self, id: &str) -> Result<Option<FileHandle>, TreeError> {
        let stored: Option<Option<Vec<u8>>> = self.conn.query_row("SELECT handle FROM leaving WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        Ok(stored.flatten().as_deref().and_then(FileHandle::decode))
    }

    /// Whether item `id` was at or below something leaving when it began to
    /// leave.
    pub fn leaving_had(&self, id: &str) -> Result<bool, TreeError> {
        Ok(self.conn.query_row("SELECT 1 FROM leaving_items WHERE id = ?1", [id], |_| Ok(())).optional()?.is_some())
    }

    /// What is leaving: each item id with where its object stays.
    pub fn leaving(&self) -> Result<Vec<(String, std::path::PathBuf)>, TreeError> {
        use std::os::unix::ffi::OsStrExt;
        let mut statement = self.conn.prepare_cached("SELECT id, rel FROM leaving ORDER BY id")?;
        let rows = statement
            .query_map([], |r| {
                let rel: Vec<u8> = r.get(1)?;
                Ok((r.get::<_, String>(0)?, std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&rel))))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Item `id` left, or is placed again: nothing of it is leaving now.
    pub fn leaving_drop(&mut self, id: &str) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM leaving WHERE id = ?1", [id])?;
        tx.execute("DELETE FROM leaving_items WHERE leaving = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// The object of item `id`, leaving, is at `rel` now: found there by its
    /// id, wherever a move took it (issue #104).
    pub fn leaving_set_rel(&self, id: &str, rel: &std::path::Path) -> Result<(), TreeError> {
        use std::os::unix::ffi::OsStrExt;
        self.conn.execute("UPDATE leaving SET rel = ?2 WHERE id = ?1", params![id, rel.as_os_str().as_bytes()])?;
        Ok(())
    }

    /// Whatever was moved from `from` to `to` takes what is leaving at or
    /// below it along (issue #104): a parent renamed in OneDrive and moved by
    /// the reconcile, or renamed here and seen by the examination.
    pub fn leaving_rebase(&self, from: &std::path::Path, to: &std::path::Path) -> Result<(), TreeError> {
        crate::outbox::rebase_leaving(&self.conn, from, to)
    }

    /// Stages `changes` on top of what `staging` holds: the fresh versions a
    /// stale-delta guard fetched (§3.7).
    pub fn stage_over(&mut self, changes: &[Change]) -> Result<(), TreeError> {
        let source = self.source(Table::Staging);
        let tx = self.conn.transaction()?;
        apply(&tx, source, changes)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
