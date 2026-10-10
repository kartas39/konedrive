//! The store's schema: its version, what a new store is created with, how
//! a store of an earlier version would be brought to it ([`migrations`]),
//! and when a store is rebuilt.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

use crate::conflicts::ConflictKind;
use crate::item_store::{step_memory_schema, unsent, unsent_columns, BINDING_ALONE, CHANGED_IN_ONEDRIVE, LEFT_HERE, SHOWN_AGAINST_PLACEMENT};
use crate::model::{skipped, ItemKind, LocalItemId, NOT_ON_DISK, ON_DISK, PLACED};
use crate::{meta, outbox, TreeError, TreeStore};

mod migrations;

/// The schema this daemon reads and writes, kept in `meta` as
/// `schema_version`. A store of a version a step starts from is brought to
/// it in place, one numbered step at a time ([`migrations`]); one of any
/// other version — a newer daemon's, or an earlier one no step starts from, as
/// every version before this one — is rebuilt from a full listing.
///
/// A change of what a table holds, a column, an index, a trigger or a
/// stored word is a new version and a new step: nothing is added to a store
/// outside one.
pub const SCHEMA_VERSION: &str = "17";

/// Every table and index of a store at [`SCHEMA_VERSION`]: what a new store
/// is created with, and what every migrated store ends as.
///
/// - `items`: one row per item, keyed by its local item id ([`crate::item_store`]).
///   `AUTOINCREMENT`: an id is never handed out twice, so the kernel's node id of an item
///   that is gone never names another. The root of the folder is row 1, made here. The
///   binding is `onedrive_drive_id` and `onedrive_item_id`, both or neither, neither ever
///   the empty text, and no two rows have the same (`items_onedrive_item`). `kind` is the
///   item's, a file or a directory, on every row, also one the folder does not show. The local version is the `local_`
///   columns: the folder shows the item when its local place is set, and the root always.
///   The OneDrive version is the `onedrive_` columns: OneDrive has the item when its place
///   there is set, and the root when the root is bound; `onedrive_etag` is what a request to
///   OneDrive is guarded with, and `placement` whether the folder can show the item at all,
///   as the listing said of the item itself; `onedrive_mime` the MIME type OneDrive gives
///   for a file's content, of a file OneDrive has and of nothing else (a check). A parent on that side is named by its local
///   item id like every other. Several rows may have one place there
///   (`items_onedrive_place` is no unique index): a listing that has not ended says so of an
///   exchange of names.
///   Beside a local and an OneDrive name the row has the name folded as OneDrive compares
///   names (`local_folded_name`, `onedrive_folded_name`: [`crate::FoldedName`]), written
///   in the statement that writes the name and there exactly when the name is (a check);
///   `items_local_folded_place` and `items_onedrive_folded_place` find the rows of one
///   folded name in a directory on each side, and neither is unique.
///   The synced place and content are the `synced_` ones, the content before the first
///   writer the `before_write_` ones; a content is six columns, `change_count` null for a
///   hashed one, and always for one OneDrive gave. No two rows have one local place (`items_local_place`). The checks hold
///   what one row can say by itself: a place is a parent and a name; the root is a
///   directory with no place; no row is its own parent; a file the folder shows has a local
///   content and nothing else has one; a directory has no content on any side, none on disk
///   and none before a write; neither has an item the folder does not show; a content is
///   on disk or not on disk; nothing is agreed on, a place or a content, for an item with
///   no binding, so that a row is bound before, or in the statement in which, a synced
///   place or content is written to it; and no OneDrive version and no eTag either. The same
///   checks of a place, of a parent and of a file's content hold of the OneDrive side.
///   `confirmed_at_count` is the count of confirmations at which OneDrive last confirmed
///   something of the row (0: never), the stamp of the guard against an outdated listing;
///   `seen_in_full_listing` the id of the full listing that last listed the item
///   ([`crate::item_store`]'s `onedrive_versions`). `items_changed_in_onedrive` holds the
///   rows whose OneDrive place or content is not the synced one, `items_binding_alone` those
///   that are a binding and nothing else, by their stamp, and `items_seen_in_full_listing`
///   what OneDrive has, by the full listing that saw it: none of the three is found by
///   reading every row. `items_shown_against_placement` holds the rows the folder shows
///   whose own placement says it cannot: a listing changed the placement of an item both
///   sides agree on, which is in neither index of what differs.
///   `unsent_since` is a stamp of one counter of the store (`meta` `last_unsent_since`, whose
///   row is made here), set while the row's local place or content is not the synced one
///   and null while they are the same: three triggers keep it (`items_unsent_since_insert`,
///   `items_unsent_since_given`, `items_unsent_since_cleared`), run after a row is made and
///   after any statement that writes a local or a synced column, so that no writer of
///   either can leave it out; they give the next stamp when the two first differ, leave a
///   stamp that is there, and take it when they agree. `items_unsent` holds the unsent rows
///   by their stamp, but those of items left here, which are never sent: a bounded read
///   of what is unsent reads none of them.
///   `is_name_ignored` is what the upload rules say of the item's own name, and of nothing
///   below it. `is_below_ignored_directory` says that a directory above the item in the
///   folder has an ignored name and no binding: it is derived from the rows above, and kept
///   by the writers of a local place, of `is_name_ignored` and of a binding
///   ([`crate::item_store`]'s `item_row`), so that the index of what is unsent can leave out
///   what is left here without a walk up from each row. A row is made with, and moved to, only a local, an OneDrive and a synced parent that is
///   the row of a directory (the triggers `items_parent_directory_insert` and
///   `items_parent_directory_update`, asked per row, at one read by the key for each side:
///   a directory's row is written before what is in it). A row is not deleted while another
///   names it as its local, its OneDrive or its synced parent, and row 1 never (the trigger
///   `items_parent_kept`, asked per row: what is in a directory is deleted before the
///   directory). An outbox row may name an item whose row is gone: the
///   removal of a file that was never sent leaves the rows that undo what was sent of it.
/// - `step_memory`, `upload_session_openings`, `upload_session_cancellations`,
///   `mismatched_item_removals`: what the steps remember beyond the versions, and the
///   trigger `step_memory_item_gone` that takes it with an item's row
///   ([`crate::item_store`]'s `step_memory`, whose modules have their schema).
/// - `pins`: the items that carry a pin of their own, by OneDrive item id ([`crate::pins`]).
/// - `onedrive_items`, `staging`: the tree and the tree a cycle builds. `local_seq`
///   is the outbox commit that last wrote the row
///   (`docs/design/writes.md` §5). `staging_gone`: what a delta removes
///   while it is staged.
/// - `deferred`, `outbox_gone`: what a read-write cycle keeps between
///   cycles ([`crate::reconcile`]). `deferred.waits`: what keeps an item
///   that is to leave the folder, as the last cycle found it.
/// - `outbox`, `local_skipped`: the write phase ([`crate::outbox`]).
///   `AUTOINCREMENT`: a `seq` is never handed out twice, so a row removed
///   at commit can never be mistaken for a new one by a worker still
///   holding it. `handle` is the file handle of the local object a row is
///   about, which is what the object is. `local_item_id` is the row of `items` it is
///   about. The content being sent is `snapshot_size`,
///   `snapshot_mtime` (whole seconds) and `snapshot_mtime_nsec`, and
///   `snapshot_change_count`, the change count the local content of the row's item had
///   then (null where the snapshot was taken with no item's row at hand).
/// - `upload_sessions`: the upload sessions opened and not yet completed,
///   cancelled or found gone, with the place a new file's
///   session holds in OneDrive with its empty placeholder until then. A row
///   points at its session (`session_url`); one no row points at any more
///   was given up, and is cancelled
///   ([`TreeStore::upload_sessions_given_up`]).
/// - `upload_openings`: the place a new file's session is about to take,
///   recorded before the request that opens it, so that a stop
///   before its URL is persisted still knows the placeholder it may have
///   left. One per row (`at` its first time, `last` the latest attempt
///   whose outcome is not known); the URL replaces it. `upload_openings_left`: a record
///   whose row left, or moved to another place, kept until a
///   `409` there resolves it, or for [`outbox::OPENING_LEFT_KEEP`].
/// - `onedrive_items_skipped` asks whether a row is placed as every query and the
///   decoder do ([`skipped`]), so that a query uses it.
/// - The indexes keep a cycle and the outbox's lookups from reading a whole
///   table.
fn schema() -> String {
    let tree = |table: &str| {
        format!(
            "CREATE TABLE {table} (
                 id TEXT PRIMARY KEY, parent_id TEXT, name TEXT NOT NULL, kind TEXT NOT NULL,
                 size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL DEFAULT 0,
                 etag TEXT, ctag TEXT, quickxor TEXT, mime TEXT,
                 placement TEXT NOT NULL, thumb_key TEXT,
                 local_seq INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX {table}_parent ON {table}(parent_id);"
        )
    };
    // Whether a row being written names, on that side, a parent that is no directory's row.
    let no_parent_directory = |side: &str| {
        format!(
            "(NEW.{side}_parent_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM items WHERE local_item_id = NEW.{side}_parent_id AND kind = '{}'))",
            ItemKind::Directory.as_str()
        )
    };
    // The next stamp of the store's one counter, given to the row a trigger is run for.
    let give_unsent_since = format!(
        "UPDATE meta SET value = CAST(value AS INTEGER) + 1 WHERE key = '{key}';
         UPDATE items SET unsent_since = (SELECT CAST(value AS INTEGER) FROM meta WHERE key = '{key}') WHERE local_item_id = NEW.local_item_id;",
        key = meta::LAST_UNSENT_SINCE
    );
    let content = |of: &str| format!("{of}_size INTEGER, {of}_mtime INTEGER, {of}_mtime_nsec INTEGER, {of}_hash TEXT, {of}_ctag TEXT, {of}_change_count INTEGER");
    format!(
        "CREATE TABLE items (
             local_item_id INTEGER PRIMARY KEY AUTOINCREMENT,
             onedrive_drive_id TEXT, onedrive_item_id TEXT,
             kind TEXT NOT NULL,
             local_parent_id INTEGER, local_name, local_folded_name, {local_content},
             onedrive_parent_id INTEGER, onedrive_name, onedrive_folded_name, {onedrive_content},
             onedrive_etag TEXT, onedrive_mime TEXT,
             synced_parent_id INTEGER, synced_name, {synced_content},
             content_presence TEXT NOT NULL DEFAULT '{not_on_disk}',
             {content_before_write},
             placement TEXT NOT NULL DEFAULT '{placed}',
             is_name_ignored INTEGER NOT NULL DEFAULT 0,
             is_below_ignored_directory INTEGER NOT NULL DEFAULT 0,
             confirmed_at_count INTEGER NOT NULL DEFAULT 0,
             seen_in_full_listing INTEGER NOT NULL DEFAULT 0,
             unsent_since INTEGER,
             CHECK (kind IN ('{file}', '{directory}')),
             CHECK ((onedrive_drive_id IS NULL) = (onedrive_item_id IS NULL)),
             CHECK (onedrive_drive_id != '' AND onedrive_item_id != ''),
             CHECK (onedrive_item_id IS NOT NULL OR (synced_parent_id IS NULL AND synced_size IS NULL)),
             CHECK (onedrive_item_id IS NOT NULL OR (onedrive_parent_id IS NULL AND onedrive_size IS NULL AND onedrive_etag IS NULL AND onedrive_mime IS NULL)),
             CHECK (onedrive_mime IS NULL OR onedrive_size IS NOT NULL),
             CHECK (content_presence IN ('{on_disk}', '{not_on_disk}')),
             CHECK ((local_parent_id IS NULL) = (local_name IS NULL)),
             CHECK ((onedrive_parent_id IS NULL) = (onedrive_name IS NULL)),
             CHECK ((synced_parent_id IS NULL) = (synced_name IS NULL)),
             CHECK ((local_name IS NULL) = (local_folded_name IS NULL) AND (onedrive_name IS NULL) = (onedrive_folded_name IS NULL)),
             CHECK (local_item_id != {root} OR (kind = '{directory}' AND local_parent_id IS NULL AND onedrive_parent_id IS NULL AND synced_parent_id IS NULL)),
             CHECK (local_parent_id IS NOT local_item_id AND onedrive_parent_id IS NOT local_item_id AND synced_parent_id IS NOT local_item_id),
             CHECK ((kind = '{file}' AND local_parent_id IS NOT NULL) = (local_size IS NOT NULL)),
             CHECK ((kind = '{file}' AND onedrive_parent_id IS NOT NULL) = (onedrive_size IS NOT NULL)),
             CHECK (onedrive_change_count IS NULL),
             CHECK (kind = '{file}' OR (synced_size IS NULL AND content_presence = '{not_on_disk}' AND before_write_size IS NULL)),
             CHECK (local_parent_id IS NOT NULL OR (content_presence = '{not_on_disk}' AND before_write_size IS NULL)));
         CREATE UNIQUE INDEX items_onedrive_item ON items(onedrive_drive_id, onedrive_item_id) WHERE onedrive_item_id IS NOT NULL;
         CREATE UNIQUE INDEX items_local_place ON items(local_parent_id, local_name) WHERE local_parent_id IS NOT NULL;
         CREATE INDEX items_onedrive_place ON items(onedrive_parent_id, onedrive_name) WHERE onedrive_parent_id IS NOT NULL;
         CREATE INDEX items_synced_place ON items(synced_parent_id, synced_name) WHERE synced_parent_id IS NOT NULL;
         CREATE INDEX items_local_folded_place ON items(local_parent_id, local_folded_name) WHERE local_parent_id IS NOT NULL;
         CREATE INDEX items_onedrive_folded_place ON items(onedrive_parent_id, onedrive_folded_name) WHERE onedrive_parent_id IS NOT NULL;
         CREATE INDEX items_changed_in_onedrive ON items(local_item_id) WHERE {changed_in_onedrive};
         CREATE INDEX items_binding_alone ON items(confirmed_at_count) WHERE {binding_alone};
         CREATE INDEX items_shown_against_placement ON items(local_item_id) WHERE {shown_against_placement};
         CREATE INDEX items_seen_in_full_listing ON items(seen_in_full_listing) WHERE onedrive_parent_id IS NOT NULL;
         CREATE INDEX items_unsent ON items(unsent_since) WHERE unsent_since IS NOT NULL AND NOT {left_here};
         CREATE TRIGGER items_parent_kept BEFORE DELETE ON items
             WHEN OLD.local_item_id = {root}
               OR EXISTS (SELECT 1 FROM items WHERE local_parent_id = OLD.local_item_id)
               OR EXISTS (SELECT 1 FROM items WHERE onedrive_parent_id = OLD.local_item_id)
               OR EXISTS (SELECT 1 FROM items WHERE synced_parent_id = OLD.local_item_id)
             BEGIN SELECT RAISE(ABORT, 'items: the root''s row and a row that is another row''s parent are not deleted'); END;
         CREATE TRIGGER items_parent_directory_insert BEFORE INSERT ON items
             WHEN {no_local_parent_directory} OR {no_onedrive_parent_directory} OR {no_synced_parent_directory}
             BEGIN SELECT RAISE(ABORT, 'items: a row''s parent is the row of a directory'); END;
         CREATE TRIGGER items_parent_directory_update BEFORE UPDATE OF local_parent_id, onedrive_parent_id, synced_parent_id ON items
             WHEN {no_local_parent_directory} OR {no_onedrive_parent_directory} OR {no_synced_parent_directory}
             BEGIN SELECT RAISE(ABORT, 'items: a row''s parent is the row of a directory'); END;
         INSERT INTO items (local_item_id, kind) VALUES ({root}, '{directory}');
         {step_memory}
         CREATE TABLE pins (id TEXT PRIMARY KEY);
         {onedrive_items}
         {staging}
         CREATE TABLE staging_gone (id TEXT PRIMARY KEY);
         CREATE INDEX onedrive_items_seq ON onedrive_items(local_seq);
         CREATE INDEX onedrive_items_skipped ON onedrive_items(id) WHERE {skipped};
         CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
         INSERT INTO meta (key, value) VALUES ('{last_unsent_since}', '0');
         CREATE TRIGGER items_unsent_since_insert AFTER INSERT ON items
             WHEN {new_unsent}
             BEGIN {give_unsent_since} END;
         CREATE TRIGGER items_unsent_since_given AFTER UPDATE OF {unsent_columns} ON items
             WHEN NEW.unsent_since IS NULL AND {new_unsent}
             BEGIN {give_unsent_since} END;
         CREATE TRIGGER items_unsent_since_cleared AFTER UPDATE OF {unsent_columns} ON items
             WHEN NEW.unsent_since IS NOT NULL AND NOT {new_unsent}
             BEGIN UPDATE items SET unsent_since = NULL WHERE local_item_id = NEW.local_item_id; END;
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
             rel TEXT NOT NULL,
             base_etag TEXT, base_ctag TEXT, base_parent TEXT, base_name TEXT,
             target_parent TEXT, target_name TEXT,
             state TEXT NOT NULL,
             reason TEXT, attempts INTEGER NOT NULL DEFAULT 0, next_try INTEGER,
             snapshot_size INTEGER, snapshot_mtime INTEGER, snapshot_mtime_nsec INTEGER, snapshot_change_count INTEGER,
             session_url TEXT, session_expires INTEGER, session_next INTEGER,
             handle BLOB,
             confirmed INTEGER NOT NULL DEFAULT 0,
             size INTEGER,
             bad_item TEXT, bad_item_ctag TEXT, bad_item_etag TEXT,
             local_item_id INTEGER);
         CREATE INDEX outbox_item ON outbox(item_id);
         CREATE INDEX outbox_local_item ON outbox(local_item_id) WHERE local_item_id IS NOT NULL;
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
        step_memory = step_memory_schema(),
        no_local_parent_directory = no_parent_directory("local"),
        no_onedrive_parent_directory = no_parent_directory("onedrive"),
        no_synced_parent_directory = no_parent_directory("synced"),
        local_content = content("local"),
        onedrive_content = content("onedrive"),
        placed = PLACED,
        changed_in_onedrive = CHANGED_IN_ONEDRIVE,
        binding_alone = BINDING_ALONE,
        shown_against_placement = SHOWN_AGAINST_PLACEMENT,
        left_here = LEFT_HERE,
        last_unsent_since = meta::LAST_UNSENT_SINCE,
        new_unsent = unsent("NEW."),
        unsent_columns = unsent_columns(),
        give_unsent_since = give_unsent_since,
        synced_content = content("synced"),
        content_before_write = content("before_write"),
        root = LocalItemId::ROOT,
        file = ItemKind::File.as_str(),
        directory = ItemKind::Directory.as_str(),
        on_disk = ON_DISK,
        not_on_disk = NOT_ON_DISK,
        onedrive_items = tree("onedrive_items"),
        staging = tree("staging"),
        rescued = ConflictKind::Rescued.as_str(),
        frees = outbox::FREES,
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
    /// is missing, unreadable as a database, or of an unknown schema version.
    /// Every other error — permission denied, disk I/O, brief lock
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

    /// A second connection to the store at `path`, for reading only:
    /// in WAL mode it reads the last committed state and never waits
    /// for the writer. Nothing is created or changed; its reads are the
    /// outbox's lists and sums for the bus.
    pub fn open_read_only(path: &Path) -> Result<Self, TreeError> {
        use rusqlite::OpenFlags;
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.set_prepared_statement_cache_capacity(64);
        Ok(Self { conn, path: None, whole: false, changes: Default::default() })
    }

    /// A number that changes whenever another connection commits to the store: what a
    /// reader keeps of the store is good for as long as it is the same.
    pub fn data_version(&self) -> Result<i64, TreeError> {
        Ok(self.conn.query_row("PRAGMA data_version", [], |row| row.get(0))?)
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
        // The outbox's point queries run thousands of times.
        conn.set_prepared_statement_cache_capacity(64);
        let changes = std::sync::Arc::new(outbox::OutboxChanges::default());
        outbox::watch(&conn, &changes)?;
        Ok(Self { conn, path: None, whole, changes })
    }
}

#[cfg(test)]
mod tests;
