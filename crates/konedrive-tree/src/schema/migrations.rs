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

const STEPS: [Step; 3] = [
    Step { from: "3", to: "4", what: "what was below a folder not placed forgot its local objects", run: forget_below_unplaced },
    Step { from: "4", to: "5", what: "the tables, columns and indexes added to version 4 without a number are part of it", run: unnumbered_additions },
    Step { from: "5", to: "6", what: "a row's snapshot is in columns of its own, and an opening is left behind without a trigger", run: snapshot_columns },
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
