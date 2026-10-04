//! The store's schema: its version, what an open creates and brings up to
//! date, and when a store is rebuilt.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

use crate::conflicts::ConflictKind;
use crate::model::PLACED;
use crate::{outbox, reconcile, TreeError, TreeStore, MAX_CHAIN};

/// Version 2 added `activity` and `conflicts`; version 3 the write phase's
/// `outbox`, `local_skipped`, `items.local_handle`, `items.local_seq` and
/// `conflicts.kind` (`docs/design/writes.md` §5). A store of any
/// other version is rebuilt from a full listing, so a
/// version 1 or 2 store is rebuilt once, and loses nothing but a listing: a
/// version 2 folder was read-only and has nothing waiting to upload.
/// Version 4 is version 3 once the local objects of every row below a row
/// that is not placed are forgotten (issue #104): a version 3 store is
/// brought to it in place, never rebuilt ([`migrate_3_to_4`]).
pub const SCHEMA_VERSION: &str = "4";

/// The `meta` key of a first listing's resume point.
pub const LISTING_NEXT: &str = "listing_next";

/// The `meta` key set while `staging` holds a whole new tree (a full
/// listing) rather than a delta laid over `items`.
pub const STAGING_WHOLE: &str = "staging_whole";

/// Created on every open (`IF NOT EXISTS`, issue #39): what a delta removes
/// from the tree while it is staged, and the indexes that keep a cycle from
/// reading the whole tree — the outbox's recent commits, what has no local
/// object on record, what is skipped.
fn scale() -> String {
    format!(
        "
    CREATE TABLE IF NOT EXISTS staging_gone (id TEXT PRIMARY KEY);
    CREATE INDEX IF NOT EXISTS items_seq ON items(local_seq);
    CREATE INDEX IF NOT EXISTS items_unplaced ON items(id) WHERE local_handle IS NULL AND placement = '{PLACED}';
    CREATE INDEX IF NOT EXISTS items_skipped ON items(id) WHERE placement != '{PLACED}';"
    )
}

/// Version 3 to 4 (issue #104), in one transaction: every row below a row
/// that is not placed forgets its local object, in `items` and `staging`,
/// each by its own tree. A build before #104 kept them when a folder
/// stopped being placed, and once the folder was placed again they read as
/// objects gone — deletes in OneDrive.
fn migrate_3_to_4(conn: &Connection) -> Result<(), TreeError> {
    let mut batch = String::from("BEGIN IMMEDIATE;");
    for table in ["items", "staging"] {
        batch.push_str(&format!(
            "WITH RECURSIVE below(id, depth) AS (
                 SELECT c.id, 1 FROM {table} c JOIN {table} p ON c.parent_id = p.id WHERE p.placement != '{PLACED}'
                 UNION
                 SELECT c.id, b.depth + 1 FROM {table} c JOIN below b ON c.parent_id = b.id WHERE b.depth < {MAX_CHAIN})
             UPDATE {table} SET local_handle = NULL WHERE local_handle IS NOT NULL AND id IN (SELECT id FROM below);"
        ));
    }
    batch.push_str(&format!("UPDATE meta SET value = '{SCHEMA_VERSION}' WHERE key = 'schema_version'; COMMIT;"));
    if let Err(e) = conn.execute_batch(&batch) {
        let _ = conn.execute_batch("ROLLBACK");
        return Err(e.into());
    }
    tracing::info!("the tree store is at version {SCHEMA_VERSION}: what was below a folder not placed forgot its local objects");
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

    pub fn in_memory() -> Result<Self, TreeError> {
        Self::prepare(Connection::open_in_memory()?)
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
    pub fn open_read_only(path: &Path) -> Result<Self, TreeError> {
        use rusqlite::OpenFlags;
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.set_prepared_statement_cache_capacity(64);
        Ok(Self { conn, path: None, whole: false, changes: Default::default() })
    }

    /// Creates the schema in a store with no table at all, in one
    /// transaction: a crash part-way used to leave
    /// some tables and no `meta`, which failed every open with an error that
    /// was not taken for a store to rebuild. A store left that way by an
    /// older build — tables, and no `meta` — reads as one of unknown
    /// version, and is rebuilt.
    fn prepare(conn: Connection) -> Result<Self, TreeError> {
        let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table'", [], |row| row.get(0))?;
        if tables == 0 {
            let mut schema = String::from("BEGIN IMMEDIATE;");
            for table in ["items", "staging"] {
                // `local_handle`: the file handle of the inode the item was
                // placed or adopted as; `local_seq`: the outbox commit that
                // last wrote the row (`docs/design/writes.md` §5).
                schema.push_str(&format!(
                    "CREATE TABLE {table} (
                        id TEXT PRIMARY KEY, parent_id TEXT, name TEXT NOT NULL, kind TEXT NOT NULL,
                        size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL DEFAULT 0,
                        etag TEXT, ctag TEXT, quickxor TEXT, mime TEXT,
                        placement TEXT NOT NULL, thumb_key TEXT,
                        local_handle BLOB, local_seq INTEGER NOT NULL DEFAULT 0);
                     CREATE INDEX {table}_parent ON {table}(parent_id);
                     CREATE INDEX {table}_handle ON {table}(local_handle);"
                ));
            }
            schema.push_str(&format!(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE activity (id INTEGER PRIMARY KEY, at INTEGER NOT NULL, kind TEXT NOT NULL,
                                        path TEXT NOT NULL, detail TEXT NOT NULL);
                 CREATE TABLE conflicts (rescued TEXT PRIMARY KEY, at INTEGER NOT NULL, original TEXT NOT NULL,
                                         kind TEXT NOT NULL DEFAULT '{rescued}');
                 {}
                 INSERT INTO meta (key, value) VALUES ('schema_version', '{SCHEMA_VERSION}');
                 COMMIT;",
                outbox::SCHEMA,
                rescued = ConflictKind::Rescued.as_str()
            ));
            if let Err(e) = conn.execute_batch(&schema) {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e.into());
            }
        }
        let has_meta: i64 =
            conn.query_row("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'", [], |row| row.get(0))?;
        if has_meta == 0 {
            return Err(TreeError::Schema(None));
        }
        let mut version: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |row| row.get(0))
            .optional()?;
        if version.as_deref() == Some("3") {
            migrate_3_to_4(&conn)?;
            version = Some(SCHEMA_VERSION.to_owned());
        }
        if version.as_deref() != Some(SCHEMA_VERSION) {
            return Err(TreeError::Schema(version));
        }
        // The read-write cycle's own tables, added to schema 3 without a
        // rebuild: a store made before them gains them here.
        conn.execute_batch(reconcile::TABLES)?;
        reconcile::upgrade(&conn)?;
        conn.execute_batch(&scale())?;
        outbox::upgrade(&conn)?;
        let whole = conn.query_row("SELECT 1 FROM meta WHERE key = ?1", [STAGING_WHOLE], |_| Ok(())).optional()?.is_some();
        // The outbox's point queries run thousands of times in one examination.
        conn.set_prepared_statement_cache_capacity(64);
        let changes = std::sync::Arc::new(outbox::OutboxChanges::default());
        outbox::watch(&conn, &changes)?;
        Ok(Self { conn, path: None, whole, changes })
    }
}
