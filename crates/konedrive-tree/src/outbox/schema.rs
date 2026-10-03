//! The write phase's tables and indexes, and what a schema-3 store gains on open.

use rusqlite::Connection;

use crate::TreeError;

/// The write phase's tables, created with the rest of schema 3.
/// `AUTOINCREMENT`: a `seq` is never handed out twice, so a row removed at
/// commit can never be mistaken for a new one by a worker still holding it.
pub(crate) const SCHEMA: &str = "
    CREATE TABLE outbox (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        kind TEXT NOT NULL,
        item_id TEXT,
        dev INTEGER, ino INTEGER,
        rel TEXT NOT NULL,
        base_etag TEXT, base_ctag TEXT, base_parent TEXT, base_name TEXT,
        target_parent TEXT, target_name TEXT,
        state TEXT NOT NULL,
        reason TEXT, attempts INTEGER NOT NULL DEFAULT 0, next_try INTEGER,
        snapshot TEXT,
        session_url TEXT, session_expires INTEGER, session_next INTEGER,
        handle BLOB,
        confirmed INTEGER NOT NULL DEFAULT 0);
    CREATE INDEX outbox_item ON outbox(item_id);
    CREATE TABLE local_skipped (rel TEXT PRIMARY KEY, reason TEXT NOT NULL, at INTEGER NOT NULL);";

/// The outbox's lookups (issue #38): by local object, by handle, by place, by
/// what is due, by the folder a row goes into, by kind, and the rows that free a name
/// in OneDrive (a partial index: removals, and moves away from the base place).
/// Created on every open (`IF NOT EXISTS`), so a store made before them gains
/// them without a rebuild.
const INDEXES: &str = "
    CREATE INDEX IF NOT EXISTS outbox_object ON outbox(dev, ino);
    CREATE INDEX IF NOT EXISTS outbox_handle ON outbox(handle);
    CREATE INDEX IF NOT EXISTS outbox_rel ON outbox(rel);
    CREATE INDEX IF NOT EXISTS outbox_due ON outbox(state, next_try, seq);
    CREATE INDEX IF NOT EXISTS outbox_target_parent ON outbox(target_parent);
    CREATE INDEX IF NOT EXISTS outbox_kind ON outbox(kind);
    CREATE INDEX IF NOT EXISTS outbox_frees ON outbox(seq) WHERE FREES;";

/// The upload sessions opened and not yet completed, cancelled or found gone
/// (issue #47), with the place a new file's session holds in OneDrive with its
/// empty placeholder until then. A row points at its session
/// (`session_url`); one no row points at any more was given up, and is
/// cancelled ([`TreeStore::upload_sessions_given_up`]). Created on every open,
/// like the indexes; a session a store of an earlier version persisted is
/// listed then, with no place.
///
/// And the openings (issue #84): the place a new file's session is about to
/// take, recorded before the request that opens it, so that a stop before its
/// URL is persisted still knows the placeholder it may have left. One per row
/// (`at` its first time, `last` the latest attempt whose outcome is not
/// known; an older store's rows have no `last`, read as `at`); the URL
/// replaces it (`outbox_open_session`). A record exists only while an
/// attempt's outcome is unknown, so one whose row leaves, or moves to another
/// place, is kept without a row (`upload_openings_left`, issue #89) until a
/// `409` there resolves it, or for [`OPENING_LEFT_KEEP`].
const SESSIONS: &str = "
    CREATE TABLE IF NOT EXISTS upload_sessions (url TEXT PRIMARY KEY, parent TEXT, name TEXT, opened INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS upload_openings (seq INTEGER PRIMARY KEY, parent TEXT NOT NULL, name TEXT NOT NULL, at INTEGER NOT NULL, last INTEGER);
    CREATE INDEX IF NOT EXISTS upload_openings_parent ON upload_openings(parent);
    CREATE TABLE IF NOT EXISTS upload_openings_left (parent TEXT NOT NULL, name TEXT NOT NULL, at INTEGER NOT NULL, last INTEGER NOT NULL, left_at INTEGER NOT NULL);
    CREATE INDEX IF NOT EXISTS upload_openings_left_parent ON upload_openings_left(parent);
    DROP TRIGGER IF EXISTS upload_openings_leave;
    CREATE TRIGGER IF NOT EXISTS upload_openings_left_behind AFTER DELETE ON outbox
        BEGIN
            INSERT INTO upload_openings_left (parent, name, at, last, left_at)
                SELECT parent, name, at, COALESCE(last, at), CAST(strftime('%s', 'now') AS INTEGER) FROM upload_openings WHERE seq = OLD.seq;
            DELETE FROM upload_openings WHERE seq = OLD.seq;
        END;
    CREATE INDEX IF NOT EXISTS upload_sessions_parent ON upload_sessions(parent);
    CREATE INDEX IF NOT EXISTS outbox_session ON outbox(session_url) WHERE session_url IS NOT NULL;
    INSERT OR IGNORE INTO upload_sessions (url, parent, name, opened)
        SELECT session_url, NULL, NULL, 0 FROM outbox WHERE session_url IS NOT NULL;";

/// How long a record of an opening whose row left is kept (issue #89): a
/// guess, longer than an abandoned placeholder was seen to live (a day).
pub const OPENING_LEFT_KEEP: i64 = 7 * 24 * 3600;

/// The rows the partial index `outbox_frees` holds: those with a base place
/// they leave. [`frees`] decides among them.
pub(super) const FREES: &str = "base_parent IS NOT NULL AND base_name IS NOT NULL AND (base_parent IS NOT target_parent OR base_name IS NOT target_name)";

/// Columns added to schema 3 without a rebuild: the size of what a row
/// sends, and of what is never uploaded, as the examination saw it — so
/// that counts and sums never read the disk (issue #38); and the item a new
/// file's upload left in OneDrive with other content ([`TreeStore::outbox_bad_item`]).
const ADDED: [(&str, &str, &str); 5] = [
    ("outbox", "size", "INTEGER"),
    ("local_skipped", "size", "INTEGER"),
    ("outbox", "bad_item", "TEXT"),
    ("outbox", "bad_item_ctag", "TEXT"),
    ("outbox", "bad_item_etag", "TEXT"),
];

/// A row an earlier version wrote while a bad item waited to be deleted kept
/// its id in the reason, `hash-mismatch:<item id>`: the id moves to its own
/// column, and the reason is the plain key. Such a row has no tag: the
/// worker deletes its item as that version did, with the tag it reads then.
/// A row that already has a bad item is not touched (limitations log F200).
const BAD_ITEM_FROM_REASON: &str = "
    UPDATE outbox SET bad_item = substr(reason, 15), reason = 'hash-mismatch'
     WHERE reason LIKE 'hash-mismatch:_%' AND bad_item IS NULL;";

/// Brings a schema-3 store up to what this daemon uses: the added columns
/// and the indexes.
pub(crate) fn upgrade(conn: &Connection) -> Result<(), TreeError> {
    for (table, column, kind) in ADDED {
        let has = conn.prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"))?.exists([column])?;
        if !has {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
        }
    }
    conn.execute_batch(BAD_ITEM_FROM_REASON)?;
    conn.execute_batch(&INDEXES.replace("FREES", FREES))?;
    let has_last = conn.prepare("SELECT 1 FROM pragma_table_info('upload_openings') WHERE name = 'last'")?.exists([])?;
    let has_table = conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'upload_openings'")?.exists([])?;
    if has_table && !has_last {
        conn.execute_batch("ALTER TABLE upload_openings ADD COLUMN last INTEGER")?;
    }
    conn.execute_batch(SESSIONS)?;
    Ok(())
}
