//! The steps that bring an older store to today's schema, in order. Each
//! starts from one version and ends at the next, and runs with the change
//! of the version in one transaction (`at_version`), so a store is never
//! left between two.
//!
//! A step is written once and not changed afterwards: its SQL is its own,
//! not the constants the rest of the crate uses today, so that a later
//! change of the schema is a new step and never a different old one.
//!
//! Versions 1 and 2 have no step: they had no outbox, so nothing waits to
//! be uploaded in them, and they are rebuilt from a full listing.

use rusqlite::{params, Connection};

use crate::TreeError;

pub(super) struct Step {
    from: &'static str,
    pub(super) to: &'static str,
    /// For the journal: what the step did.
    pub(super) what: &'static str,
    pub(super) run: fn(&Connection) -> Result<(), TreeError>,
}

const STEPS: [Step; 5] = [
    Step { from: "3", to: "4", what: "what was below a folder not placed forgot its local objects", run: forget_below_unplaced },
    Step { from: "4", to: "5", what: "the tables, columns and indexes added to version 4 without a number are part of it", run: unnumbered_additions },
    Step { from: "5", to: "6", what: "a row's snapshot is in columns of its own, and an opening is left behind without a trigger", run: snapshot_columns },
    Step { from: "6", to: "7", what: "the indexes of placed and skipped rows read a placement as the decoder does", run: placement_indexes },
    Step { from: "7", to: "8", what: "what was leaving the folder waits as a deferred change, and no row that is not placed records a local object", run: leaving_waits },
];

/// The step that starts from `version`.
pub(super) fn from(version: Option<&str>) -> Option<&'static Step> {
    STEPS.iter().find(|step| Some(step.from) == version)
}

fn has_table(conn: &Connection, table: &str) -> Result<bool, TreeError> {
    Ok(conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?.exists([table])?)
}

fn add_column(conn: &Connection, table: &str, column: &str, kind: &str) -> Result<(), TreeError> {
    let has = conn.prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"))?.exists([column])?;
    if !has {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
    }
    Ok(())
}

/// Version 3 to 4 (issue #104): every row below a row that is not placed
/// forgets its local object, in `items` and `staging`, each by its own
/// tree. A build before #104 kept them when a folder stopped being placed,
/// and once the folder was placed again they read as objects gone —
/// deletes in OneDrive.
fn forget_below_unplaced(conn: &Connection) -> Result<(), TreeError> {
    for table in ["items", "staging"] {
        conn.execute_batch(&format!(
            "WITH RECURSIVE below(id, depth) AS (
                 SELECT c.id, 1 FROM {table} c JOIN {table} p ON c.parent_id = p.id WHERE p.placement != 'placed'
                 UNION
                 SELECT c.id, b.depth + 1 FROM {table} c JOIN below b ON c.parent_id = b.id WHERE b.depth < 130)
             UPDATE {table} SET local_handle = NULL WHERE local_handle IS NOT NULL AND id IN (SELECT id FROM below);"
        ))?;
    }
    Ok(())
}

/// Version 4 to 5. Builds that wrote version 3 and 4 added tables, columns
/// and indexes on every open without a version of their own, so a store
/// marked 4 has any part of them, depending on the last build that opened
/// it: each is added here only where it is missing, and a store that has
/// them all (every build since the leaving mechanism of issue #104 leaves
/// one) is changed in nothing but the trigger of the build before issue
/// #89.
///
/// What they were, oldest first: the read-write cycle's tables; the tables
/// and indexes of a staged delta (issue #39); the outbox's sizes and
/// indexes (issue #38); the listed upload sessions (issue #47), with a
/// session a row already had listed without a place; the openings (issues
/// #84, #89), whose first trigger deleted a record with its row;
/// `leaving.handle` (issue #104); a bad item's columns, with the id an
/// earlier build kept in the reason (`hash-mismatch:<item id>`) moved to
/// its column — such a row has no tag (limitations log F200).
fn unnumbered_additions(conn: &Connection) -> Result<(), TreeError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS deferred (
             id TEXT PRIMARY KEY, seq INTEGER NOT NULL, gone INTEGER NOT NULL,
             parent_id TEXT, name TEXT, kind TEXT, size INTEGER, mtime INTEGER, etag TEXT, ctag TEXT,
             quickxor TEXT, mime TEXT, placement TEXT);
         CREATE TABLE IF NOT EXISTS outbox_gone (id TEXT PRIMARY KEY, local_seq INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS leaving (id TEXT PRIMARY KEY, rel BLOB NOT NULL, handle BLOB);
         CREATE TABLE IF NOT EXISTS leaving_items (id TEXT PRIMARY KEY, leaving TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS staging_gone (id TEXT PRIMARY KEY);
         CREATE INDEX IF NOT EXISTS items_seq ON items(local_seq);
         CREATE INDEX IF NOT EXISTS items_unplaced ON items(id) WHERE local_handle IS NULL AND placement = 'placed';
         CREATE INDEX IF NOT EXISTS items_skipped ON items(id) WHERE placement != 'placed';",
    )?;
    add_column(conn, "leaving", "handle", "BLOB")?;
    for (table, column, kind) in [
        ("outbox", "size", "INTEGER"),
        ("local_skipped", "size", "INTEGER"),
        ("outbox", "bad_item", "TEXT"),
        ("outbox", "bad_item_ctag", "TEXT"),
        ("outbox", "bad_item_etag", "TEXT"),
    ] {
        add_column(conn, table, column, kind)?;
    }
    if has_table(conn, "upload_openings")? {
        add_column(conn, "upload_openings", "last", "INTEGER")?;
    }
    conn.execute_batch(
        "UPDATE outbox SET bad_item = substr(reason, 15), reason = 'hash-mismatch'
          WHERE reason LIKE 'hash-mismatch:_%' AND bad_item IS NULL;
         CREATE INDEX IF NOT EXISTS outbox_object ON outbox(dev, ino);
         CREATE INDEX IF NOT EXISTS outbox_handle ON outbox(handle);
         CREATE INDEX IF NOT EXISTS outbox_rel ON outbox(rel);
         CREATE INDEX IF NOT EXISTS outbox_due ON outbox(state, next_try, seq);
         CREATE INDEX IF NOT EXISTS outbox_target_parent ON outbox(target_parent);
         CREATE INDEX IF NOT EXISTS outbox_kind ON outbox(kind);
         CREATE INDEX IF NOT EXISTS outbox_frees ON outbox(seq)
             WHERE base_parent IS NOT NULL AND base_name IS NOT NULL AND (base_parent IS NOT target_parent OR base_name IS NOT target_name);
         CREATE INDEX IF NOT EXISTS outbox_session ON outbox(session_url) WHERE session_url IS NOT NULL;
         CREATE TABLE IF NOT EXISTS upload_sessions (url TEXT PRIMARY KEY, parent TEXT, name TEXT, opened INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS upload_sessions_parent ON upload_sessions(parent);
         CREATE TABLE IF NOT EXISTS upload_openings (seq INTEGER PRIMARY KEY, parent TEXT NOT NULL, name TEXT NOT NULL, at INTEGER NOT NULL, last INTEGER);
         CREATE INDEX IF NOT EXISTS upload_openings_parent ON upload_openings(parent);
         CREATE TABLE IF NOT EXISTS upload_openings_left (parent TEXT NOT NULL, name TEXT NOT NULL, at INTEGER NOT NULL, last INTEGER NOT NULL, left_at INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS upload_openings_left_parent ON upload_openings_left(parent);
         DROP TRIGGER IF EXISTS upload_openings_leave;
         INSERT OR IGNORE INTO upload_sessions (url, parent, name, opened)
             SELECT session_url, NULL, NULL, 0 FROM outbox WHERE session_url IS NOT NULL;",
    )?;
    Ok(())
}

/// What version 5 kept in `outbox.snapshot`, one text for three things:
/// `<size> <mtime in nanoseconds>` of the content being sent, or one of a
/// `move-out` row's two markers.
enum Version5Snapshot {
    Content { size: i64, mtime: i64, mtime_nsec: i64 },
    MovedOut(&'static str),
}

fn version_5_snapshot(stored: &str) -> Option<Version5Snapshot> {
    match stored {
        "moved-out:local" => Some(Version5Snapshot::MovedOut("local")),
        "moved-out:trash" => Some(Version5Snapshot::MovedOut("trash")),
        _ => {
            let (size, ns) = stored.split_once(' ')?;
            let (size, ns) = (size.parse::<u64>().ok()?, ns.parse::<i128>().ok()?);
            Some(Version5Snapshot::Content {
                size: i64::try_from(size).ok()?,
                mtime: i64::try_from(ns.div_euclid(1_000_000_000)).ok()?,
                mtime_nsec: ns.rem_euclid(1_000_000_000) as i64,
            })
        }
    }
}

/// Version 5 to 6. The content a row sends is `snapshot_size`,
/// `snapshot_mtime` and `snapshot_mtime_nsec`, a `move-out` row's marker
/// is `moved_out`, and the text column that held either goes. A text that
/// was neither (no build wrote one) leaves the row without a snapshot: the
/// worker takes one again, and gives up the session opened for the old one.
///
/// And the trigger that kept a row's opening when the row was deleted
/// goes: the function that removes a row does it.
fn snapshot_columns(conn: &Connection) -> Result<(), TreeError> {
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS upload_openings_left_behind;
         ALTER TABLE outbox ADD COLUMN snapshot_size INTEGER;
         ALTER TABLE outbox ADD COLUMN snapshot_mtime INTEGER;
         ALTER TABLE outbox ADD COLUMN snapshot_mtime_nsec INTEGER;
         ALTER TABLE outbox ADD COLUMN moved_out TEXT;",
    )?;
    let old: Vec<(i64, String)> = conn
        .prepare("SELECT seq, snapshot FROM outbox WHERE snapshot IS NOT NULL")?
        // What is not a text says as little as a text nobody wrote.
        .query_map([], |row| Ok((row.get(0)?, row.get_ref(1)?.as_str().unwrap_or_default().to_owned())))?
        .collect::<Result<_, _>>()?;
    for (seq, stored) in old {
        match version_5_snapshot(&stored) {
            Some(Version5Snapshot::Content { size, mtime, mtime_nsec }) => conn.execute(
                "UPDATE outbox SET snapshot_size = ?2, snapshot_mtime = ?3, snapshot_mtime_nsec = ?4 WHERE seq = ?1",
                params![seq, size, mtime, mtime_nsec],
            )?,
            Some(Version5Snapshot::MovedOut(word)) => conn.execute("UPDATE outbox SET moved_out = ?2 WHERE seq = ?1", params![seq, word])?,
            None => {
                tracing::warn!("outbox row {seq} had a snapshot that says nothing ({stored:?}); it is taken again");
                0
            }
        };
    }
    conn.execute_batch("ALTER TABLE outbox DROP COLUMN snapshot;")?;
    Ok(())
}

/// Version 6 to 7 (quality finding `TR6`): the two partial indexes of
/// `items` asked `placement = 'placed'`, which takes a word nobody can read
/// for a row that is not placed, where the decoder reads it as placed. They
/// ask what the decoder asks: a placement that does not begin as a skipped
/// one is placed. No row changes.
fn placement_indexes(conn: &Connection) -> Result<(), TreeError> {
    conn.execute_batch(
        "DROP INDEX IF EXISTS items_unplaced;
         DROP INDEX IF EXISTS items_skipped;
         CREATE INDEX items_unplaced ON items(id) WHERE local_handle IS NULL AND substr(placement, 1, 8) != 'skipped:';
         CREATE INDEX items_skipped ON items(id) WHERE substr(placement, 1, 8) = 'skipped:';",
    )?;
    Ok(())
}

/// Version 7 to 8 (`docs/design/writes.md` §9, "What can no longer be
/// placed"). Until version 7 an item OneDrive still had and the folder could
/// not hold any more was not placed by the base at once, and its object
/// stayed on disk, followed by a row of `leaving`, until what waited in it
/// was uploaded. From version 8 such an item stays placed where the disk has
/// it, and OneDrive's row of it waits in `deferred`.
///
/// Each row of `leaving` is carried over: the item's row of `items` (as
/// OneDrive has it) becomes its deferred change, dated so that no commit on
/// record supersedes it, and `items` places the item again where its object
/// is — the folder `leaving.rel` names and the name it has there — with the
/// object recorded. A content row of that very item is one against that
/// place again, so it sends no name. What the base has below it is placed
/// again with it, with no object recorded: an examination records what it
/// finds in place.
///
/// A row that cannot be carried is only dropped: its path is not UTF-8, the
/// folder it names is not placed by `items`, another item is placed at
/// that name, or the item is placed elsewhere again. Its object is then
/// nobody's item (a copy, as the examination takes it): a downloaded file
/// in it goes up as new and one not downloaded is listed. So that such an
/// object is not also taken for the item, the content rows at or below it
/// go (their content goes up as new instead), and a new file's row there
/// asks the directory for its folder again.
///
/// A row blocked as `leaving-not-found` is ready again, and is uploaded as
/// new if OneDrive still answers `404`. The two skips that only said what
/// kept a leaving folder go: `mounted-inside` is `other-device`, which is
/// what it is, and `unknown-state` is dropped. And every row of `items`
/// the base does not place forgets its local object (invariant I1), which
/// builds before version 8 only did as they wrote a row.
///
/// Nothing on disk is read or changed, and no row of the outbox that could
/// still be sent is lost.
fn leaving_waits(conn: &Connection) -> Result<(), TreeError> {
    use rusqlite::OptionalExtension;
    conn.execute_batch("ALTER TABLE deferred ADD COLUMN waits TEXT;")?;
    let root: Option<String> = conn.query_row("SELECT value FROM meta WHERE key = 'root_item_id'", [], |r| r.get(0)).optional()?.flatten();
    let commits: i64 = conn
        .query_row("SELECT value FROM meta WHERE key = 'outbox_seq'", [], |r| r.get::<_, Option<String>>(0))
        .optional()?
        .flatten()
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    let leaving: Vec<(String, Vec<u8>, Option<Vec<u8>>)> =
        conn.prepare("SELECT id, rel, handle FROM leaving ORDER BY id")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
    // The item `items` places at `name` in `parent`, if any.
    let placed_child = |parent: &str, name: &str| -> Result<Option<String>, TreeError> {
        Ok(conn
            .query_row("SELECT id FROM items WHERE parent_id = ?1 AND name = ?2 AND substr(placement, 1, 8) != 'skipped:'", [parent, name], |r| r.get(0))
            .optional()?)
    };
    // The folder `items` places at the path `names`, from the root down.
    let folder_at = |names: &[&str]| -> Result<Option<String>, TreeError> {
        let Some(mut at) = root.clone() else { return Ok(None) };
        for name in names {
            match placed_child(&at, name)? {
                Some(child) => at = child,
                None => return Ok(None),
            }
        }
        Ok(Some(at))
    };
    // Whether `items` places item `id`: itself and every folder above it.
    let placed = |id: &str| -> Result<bool, TreeError> {
        let mut at = id.to_owned();
        for _ in 0..130 {
            if Some(&at) == root.as_ref() {
                return Ok(true);
            }
            let row: Option<(Option<String>, String)> = conn.query_row("SELECT parent_id, placement FROM items WHERE id = ?1", [&at], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            match row {
                Some((Some(parent), placement)) if !placement.starts_with("skipped:") => at = parent,
                _ => return Ok(false),
            }
        }
        Ok(false)
    };
    let mut dropped: Vec<Vec<u8>> = Vec::new();
    for (id, rel, handle) in leaving {
        let carried = (|| -> Result<bool, TreeError> {
            let Ok(path) = std::str::from_utf8(&rel) else { return Ok(false) };
            let mut names: Vec<&str> = path.split('/').collect();
            let Some(name) = names.pop().filter(|name| !name.is_empty()) else { return Ok(false) };
            let has_row = conn.prepare("SELECT 1 FROM items WHERE id = ?1")?.exists([&id])?;
            if !has_row || placed(&id)? {
                return Ok(false);
            }
            let Some(parent) = folder_at(&names)? else { return Ok(false) };
            if !placed(&parent)? || placed_child(&parent, name)?.is_some_and(|other| other != id) {
                return Ok(false);
            }
            let (last, was_parent, was_name): (i64, Option<String>, String) =
                conn.query_row("SELECT local_seq, parent_id, name FROM items WHERE id = ?1", [&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            conn.execute(
                "INSERT OR REPLACE INTO deferred (id, seq, gone, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement, waits)
                 SELECT id, MAX(?2, COALESCE((SELECT g.local_seq FROM outbox_gone g WHERE g.id = items.id), 0)), 0,
                        parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement, NULL
                   FROM items WHERE id = ?1",
                params![id, commits.max(last)],
            )?;
            // A handle is at least its kind: anything shorter was never one.
            let object = handle.filter(|stored| stored.len() >= 4);
            conn.execute("UPDATE items SET parent_id = ?2, name = ?3, placement = 'placed', local_handle = ?4 WHERE id = ?1", params![id, parent, name, object])?;
            // Its own content row was recorded against OneDrive's place.
            conn.execute(
                "UPDATE outbox SET base_parent = ?2, base_name = ?3, target_parent = ?2, target_name = ?3
                  WHERE item_id = ?1 AND kind = 'update' AND base_parent IS ?4 AND base_name IS ?5",
                params![id, parent, name, was_parent, was_name],
            )?;
            Ok(true)
        })()?;
        if !carried {
            dropped.push(rel);
        }
    }
    if !dropped.is_empty() {
        let below = |rel: &[u8]| dropped.iter().any(|left| rel == left.as_slice() || (rel.starts_with(left) && rel.get(left.len()) == Some(&b'/')));
        let rows: Vec<(i64, String, Vec<u8>)> = conn
            .prepare("SELECT seq, kind, CAST(rel AS BLOB) FROM outbox")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        for (seq, kind, _) in rows.into_iter().filter(|(_, _, rel)| below(rel)) {
            match kind.as_str() {
                "update" => {
                    tracing::warn!("outbox row {seq} sent the content of a file in a folder that was leaving and cannot be carried over; the file goes up as new instead");
                    // As a row leaves the outbox in version 7: the record
                    // of an opening it made is kept without it.
                    conn.execute(
                        "INSERT INTO upload_openings_left (parent, name, at, last, left_at)
                         SELECT parent, name, at, COALESCE(last, at), CAST(strftime('%s', 'now') AS INTEGER) FROM upload_openings WHERE seq = ?1",
                        [seq],
                    )?;
                    conn.execute("DELETE FROM upload_openings WHERE seq = ?1", [seq])?;
                    conn.execute("DELETE FROM outbox WHERE seq = ?1", [seq])?;
                }
                "create" | "mkdir" => {
                    conn.execute("UPDATE outbox SET target_parent = NULL WHERE seq = ?1", [seq])?;
                }
                _ => {}
            }
        }
    }
    conn.execute_batch(
        "DROP TABLE leaving;
         DROP TABLE leaving_items;
         UPDATE outbox SET state = 'ready', reason = NULL, next_try = NULL WHERE reason = 'leaving-not-found';
         DELETE FROM local_skipped WHERE reason = 'unknown-state';
         UPDATE local_skipped SET reason = 'other-device' WHERE reason = 'mounted-inside';",
    )?;
    // I1, for every row at once: one walk down from the root.
    if let Some(root) = &root {
        conn.execute(
            "WITH RECURSIVE placed(id, depth) AS (
                 SELECT ?1, 0
                 UNION ALL
                 SELECT c.id, p.depth + 1 FROM items c JOIN placed p ON c.parent_id = p.id
                  WHERE substr(c.placement, 1, 8) != 'skipped:' AND p.depth < 130)
             UPDATE items SET local_handle = NULL WHERE local_handle IS NOT NULL AND id NOT IN (SELECT id FROM placed)",
            [root],
        )?;
    }
    Ok(())
}
