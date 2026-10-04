//! The store's schema: its version, what a new store is created with, how
//! an older one is brought to it ([`migrations`]), and when a store is
//! rebuilt.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

use crate::conflicts::ConflictKind;
use crate::model::{placed, skipped};
use crate::{meta, outbox, TreeError, TreeStore};

mod migrations;

/// The schema this daemon reads and writes, kept in `meta` as
/// `schema_version`. A store of an older version is brought to it in place,
/// one numbered step at a time ([`migrations`]); one of a version no step
/// starts from — older than the outbox, or a newer daemon's — is rebuilt
/// from a full listing.
///
/// A change of what a table holds, a column, an index, a trigger or a
/// stored word is a new version and a new step: nothing is added to a store
/// outside one.
pub const SCHEMA_VERSION: &str = "8";

/// Every table and index of a store at [`SCHEMA_VERSION`]: what a new store
/// is created with, and what every migrated store ends as.
///
/// - `items`, `staging`: the tree and the tree a cycle builds.
///   `local_handle` is the file handle of the inode the item was placed or
///   adopted as, `local_seq` the outbox commit that last wrote the row
///   (`docs/design/writes.md` §5). `staging_gone`: what a delta removes
///   while it is staged (issue #39).
/// - `deferred`, `outbox_gone`: what a read-write cycle keeps between
///   cycles ([`crate::reconcile`]). `deferred.waits`: what keeps an item
///   that is to leave the folder, as the last cycle found it.
/// - `outbox`, `local_skipped`: the write phase ([`crate::outbox`]).
///   `AUTOINCREMENT`: a `seq` is never handed out twice, so a row removed
///   at commit can never be mistaken for a new one by a worker still
///   holding it. The content being sent is `snapshot_size`,
///   `snapshot_mtime` (whole seconds) and `snapshot_mtime_nsec`; a
///   `move-out` row's marker is `moved_out`.
/// - `upload_sessions`: the upload sessions opened and not yet completed,
///   cancelled or found gone (issue #47), with the place a new file's
///   session holds in OneDrive with its empty placeholder until then. A row
///   points at its session (`session_url`); one no row points at any more
///   was given up, and is cancelled
///   ([`TreeStore::upload_sessions_given_up`]).
/// - `upload_openings`: the place a new file's session is about to take
///   (issue #84), recorded before the request that opens it, so that a stop
///   before its URL is persisted still knows the placeholder it may have
///   left. One per row (`at` its first time, `last` the latest attempt
///   whose outcome is not known, read as `at` when a store older than it
///   left none); the URL replaces it. `upload_openings_left`: a record
///   whose row left, or moved to another place (issue #89), kept until a
///   `409` there resolves it, or for [`outbox::OPENING_LEFT_KEEP`].
/// - `items_unplaced` and `items_skipped` ask whether a row is placed as
///   every query and the decoder do ([`placed`]), so that a query uses them.
/// - The indexes keep a cycle and the outbox's lookups from reading a whole
///   table (issues #38, #39).
fn schema() -> String {
    let tree = |table: &str| {
        format!(
            "CREATE TABLE {table} (
                 id TEXT PRIMARY KEY, parent_id TEXT, name TEXT NOT NULL, kind TEXT NOT NULL,
                 size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL DEFAULT 0,
                 etag TEXT, ctag TEXT, quickxor TEXT, mime TEXT,
                 placement TEXT NOT NULL, thumb_key TEXT,
                 local_handle BLOB, local_seq INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX {table}_parent ON {table}(parent_id);
             CREATE INDEX {table}_handle ON {table}(local_handle);"
        )
    };
    format!(
        "{items}
         {staging}
         CREATE TABLE staging_gone (id TEXT PRIMARY KEY);
         CREATE INDEX items_seq ON items(local_seq);
         CREATE INDEX items_unplaced ON items(id) WHERE local_handle IS NULL AND {placed};
         CREATE INDEX items_skipped ON items(id) WHERE {skipped};
         CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
         CREATE TABLE activity (id INTEGER PRIMARY KEY, at INTEGER NOT NULL, kind TEXT NOT NULL,
                                path TEXT NOT NULL, detail TEXT NOT NULL);
         CREATE TABLE conflicts (rescued TEXT PRIMARY KEY, at INTEGER NOT NULL, original TEXT NOT NULL,
                                 kind TEXT NOT NULL DEFAULT '{rescued}');
         CREATE TABLE deferred (
             id TEXT PRIMARY KEY, seq INTEGER NOT NULL, gone INTEGER NOT NULL,
             parent_id TEXT, name TEXT, kind TEXT, size INTEGER, mtime INTEGER, etag TEXT, ctag TEXT,
             quickxor TEXT, mime TEXT, placement TEXT, waits TEXT);
         CREATE TABLE outbox_gone (id TEXT PRIMARY KEY, local_seq INTEGER NOT NULL);
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
             snapshot_size INTEGER, snapshot_mtime INTEGER, snapshot_mtime_nsec INTEGER,
             moved_out TEXT,
             session_url TEXT, session_expires INTEGER, session_next INTEGER,
             handle BLOB,
             confirmed INTEGER NOT NULL DEFAULT 0,
             size INTEGER,
             bad_item TEXT, bad_item_ctag TEXT, bad_item_etag TEXT);
         CREATE INDEX outbox_item ON outbox(item_id);
         CREATE INDEX outbox_object ON outbox(dev, ino);
         CREATE INDEX outbox_handle ON outbox(handle);
         CREATE INDEX outbox_rel ON outbox(rel);
         CREATE INDEX outbox_due ON outbox(state, next_try, seq);
         CREATE INDEX outbox_target_parent ON outbox(target_parent);
         CREATE INDEX outbox_kind ON outbox(kind);
         CREATE INDEX outbox_frees ON outbox(seq) WHERE {frees};
         CREATE INDEX outbox_session ON outbox(session_url) WHERE session_url IS NOT NULL;
         CREATE TABLE local_skipped (rel TEXT PRIMARY KEY, reason TEXT NOT NULL, at INTEGER NOT NULL, size INTEGER);
         CREATE TABLE upload_sessions (url TEXT PRIMARY KEY, parent TEXT, name TEXT, opened INTEGER NOT NULL);
         CREATE INDEX upload_sessions_parent ON upload_sessions(parent);
         CREATE TABLE upload_openings (seq INTEGER PRIMARY KEY, parent TEXT NOT NULL, name TEXT NOT NULL, at INTEGER NOT NULL, last INTEGER);
         CREATE INDEX upload_openings_parent ON upload_openings(parent);
         CREATE TABLE upload_openings_left (parent TEXT NOT NULL, name TEXT NOT NULL, at INTEGER NOT NULL, last INTEGER NOT NULL, left_at INTEGER NOT NULL);
         CREATE INDEX upload_openings_left_parent ON upload_openings_left(parent);",
        items = tree("items"),
        staging = tree("staging"),
        rescued = ConflictKind::Rescued.as_str(),
        frees = outbox::FREES,
        placed = placed("placement"),
        skipped = skipped("placement"),
    )
}

/// Runs `work` and sets the store's version to `version`, in one
/// transaction: a crash or an error leaves the store as it was.
fn at_version(conn: &Connection, version: &str, work: impl FnOnce(&Connection) -> Result<(), TreeError>) -> Result<(), TreeError> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    work(&tx)?;
    meta::set(&tx, meta::SCHEMA_VERSION, Some(version))?;
    tx.commit()?;
    Ok(())
}

/// Whether `TreeStore::open` should discard what is on disk and rebuild empty:
/// only for an unknown schema version, or a file SQLite itself reports as not
/// a database or corrupt. Permission errors, other I/O failures,
/// and anything else are returned unchanged — the store on disk is untouched.
fn should_rebuild(error: &TreeError) -> bool {
    match error {
        TreeError::Schema(_) => true,
        TreeError::Sql(rusqlite::Error::SqliteFailure(e, _)) => {
            matches!(e.code, rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt)
        }
        _ => false,
    }
}

impl TreeStore {
    /// Opens the store at `path`, creating it — and rebuilding it empty when it
    /// is missing, unreadable as a database, or of an unknown schema version
    ///. Every other error — permission denied, disk I/O, brief lock
    /// contention that outlasts the busy timeout — is returned unchanged, and
    /// nothing on disk is touched: a transient failure is not corruption, and
    /// must never cost the user their tree.
    pub fn open(path: &Path) -> Result<Self, TreeError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        match Self::open_file(path) {
            Ok(store) => Ok(store),
            Err(e) if should_rebuild(&e) => {
                tracing::warn!("{} cannot be used ({e}); it is rebuilt from a full listing", path.display());
                for suffix in ["", "-wal", "-shm"] {
                    match std::fs::remove_file(format!("{}{suffix}", path.display())) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                Self::open_file(path)
            }
            Err(e) => Err(e),
        }
    }

    /// A store in memory: the tests' store.
    #[cfg(any(test, feature = "testing"))]
    pub fn in_memory() -> Result<Self, TreeError> {
        Self::prepare(Connection::open_in_memory()?)
    }

    /// Tests only: brings the store to [`SCHEMA_VERSION`] where it stands,
    /// as an open does, after a test wrote an older version into it.
    #[cfg(any(test, feature = "testing"))]
    pub fn upgrade_in_place(&mut self) -> Result<(), TreeError> {
        let mut version = meta::get(&self.conn, meta::SCHEMA_VERSION)?;
        while let Some(step) = migrations::from(version.as_deref()) {
            at_version(&self.conn, step.to, step.run)?;
            version = meta::get(&self.conn, meta::SCHEMA_VERSION)?;
        }
        Ok(())
    }

    fn open_file(path: &Path) -> Result<Self, TreeError> {
        use std::os::unix::fs::PermissionsExt;
        let conn = Connection::open(path)?;
        // The names of the user's files are nobody else's business.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        // Brief lock contention (another process mid-transaction) must resolve
        // on its own, never be mistaken for corruption and rebuild a good store.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get::<_, String>(0))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(Self { path: Some(path.to_path_buf()), ..Self::prepare(conn)? })
    }

    /// A second connection to the store at `path`, for reading only (issue
    /// #38): in WAL mode it reads the last committed state and never waits
    /// for the writer. Nothing is created or changed; its reads are the
    /// outbox's lists and sums for the bus.
    pub(crate) fn open_read_only(path: &Path) -> Result<Self, TreeError> {
        use rusqlite::OpenFlags;
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.set_prepared_statement_cache_capacity(64);
        Ok(Self { conn, path: None, whole: false, changes: Default::default() })
    }

    /// Brings what `conn` holds to [`SCHEMA_VERSION`]: a store with no
    /// table at all gets the schema, an older one its migrations, each in a
    /// transaction of its own. A store with tables and no `meta` (a crash
    /// of a build that did not create them in one transaction) reads as one
    /// of unknown version, and is rebuilt.
    fn prepare(conn: Connection) -> Result<Self, TreeError> {
        let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table'", [], |row| row.get(0))?;
        if tables == 0 {
            at_version(&conn, SCHEMA_VERSION, |tx| Ok(tx.execute_batch(&schema())?))?;
        }
        let has_meta: i64 =
            conn.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'", [], |row| row.get(0))?;
        if has_meta == 0 {
            return Err(TreeError::Schema(None));
        }
        let stored = |conn: &Connection| meta::get(conn, meta::SCHEMA_VERSION);
        let mut version = stored(&conn)?;
        while let Some(step) = migrations::from(version.as_deref()) {
            at_version(&conn, step.to, step.run)?;
            tracing::info!("the tree store is at version {}: {}", step.to, step.what);
            version = stored(&conn)?;
        }
        if version.as_deref() != Some(SCHEMA_VERSION) {
            return Err(TreeError::Schema(version));
        }
        let whole = conn.query_row("SELECT 1 FROM meta WHERE key = ?1", [meta::STAGING_WHOLE], |_| Ok(())).optional()?.is_some();
        // The outbox's point queries run thousands of times in one examination.
        conn.set_prepared_statement_cache_capacity(64);
        let changes = std::sync::Arc::new(outbox::OutboxChanges::default());
        outbox::watch(&conn, &changes)?;
        Ok(Self { conn, path: None, whole, changes })
    }
}

#[cfg(test)]
mod tests;
