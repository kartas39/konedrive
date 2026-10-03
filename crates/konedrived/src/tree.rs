//! The tree store: one row per file and folder of the
//! drive, and the delta link. A map, not the truth — the extended attributes
//! on the files are that — so it is rebuilt from a full listing whenever it
//! cannot be used, and losing it costs one listing, never data.
//!
//! `items` is the tree the folder was last made to match; [`Table::Staging`]
//! is the tree a cycle is building. The folder is reconciled against the new
//! tree, and only then does it replace `items`, in one transaction, so a
//! crash in between leaves `items` and the delta link as they were and the
//! next cycle asks for the same changes again.
//!
//! A delta's new tree is `items` with the delta laid over it (issue #39):
//! the table `staging` holds only the rows the delta writes, each a whole
//! row, and `staging_gone` the ids it removes. Reading the new tree reads
//! `staging` first and `items` for the rest; the swap writes those rows and
//! removes those ids, nothing else. A full listing, which may leave out
//! anything, is staged whole instead: `staging` is then the new tree by
//! itself (`meta` [`STAGING_WHOLE`]), and the swap replaces every row.
//!
//! A folder's first listing is the one exception: each page goes
//! into `items` as soon as it is placed, with the link to the next page
//! (`listing_next`) in the same transaction, so that a listing stopped
//! part-way resumes where it stopped. `commit_staging` ends it as it ends
//! every listing.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use crate::drive::item::{DriveItem, NAME_MAX, RESERVED_PREFIX};

pub mod outbox;
pub mod reconcile;

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

/// How many activity events the store keeps: the oldest go.
pub const ACTIVITY_KEPT: usize = 200;

/// Thumbnail candidates looked at by one query, and in one call
/// ([`TreeStore::thumbnail_candidates`]; guesses, issue #39).
pub const THUMB_PAGE: usize = 500;
pub const THUMB_SCAN: usize = 5000;

/// A chain of parents longer than this is a cycle or corruption, not a drive.
const MAX_CHAIN: usize = konedrive_fs::MAX_DEPTH + 2;

/// The `meta` key of a first listing's resume point.
pub const LISTING_NEXT: &str = "listing_next";

/// The `meta` key set while `staging` holds a whole new tree (a full
/// listing) rather than a delta laid over `items`.
pub const STAGING_WHOLE: &str = "staging_whole";

/// Every column, for copies between `items` and `staging`: the local ones
/// (`thumb_key`, `local_handle`, `local_seq`) travel with the row.
const COLUMNS: &str = "id, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement, thumb_key, local_handle, local_seq";
const ROW_COLUMNS: &str = "id, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement";

/// Created on every open (`IF NOT EXISTS`, issue #39): what a delta removes
/// from the tree while it is staged, and the indexes that keep a cycle from
/// reading the whole tree — the outbox's recent commits, what has no local
/// object on record, what is skipped.
const SCALE: &str = "
    CREATE TABLE IF NOT EXISTS staging_gone (id TEXT PRIMARY KEY);
    CREATE INDEX IF NOT EXISTS items_seq ON items(local_seq);
    CREATE INDEX IF NOT EXISTS items_unplaced ON items(id) WHERE local_handle IS NULL AND placement = 'placed';
    CREATE INDEX IF NOT EXISTS items_skipped ON items(id) WHERE placement != 'placed';";

/// An `items` row `p` the delta laid over it leaves as it is.
const UNTOUCHED: &str =
    "NOT EXISTS (SELECT 1 FROM staging s WHERE s.id = p.id) AND NOT EXISTS (SELECT 1 FROM staging_gone g WHERE g.id = p.id)";

/// Where the rows of a tree are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// `items`.
    Items,
    /// `staging` alone: a full listing staged whole.
    Whole,
    /// `staging` over `items`, less `staging_gone`: a delta staged.
    Overlay,
}

impl Source {
    /// The tree as a table expression, for a query SQLite can push its
    /// `WHERE` into (a point or index lookup) — never for a join or a
    /// recursion, which would read it whole; see [`Source::step`].
    fn rows(self) -> String {
        match self {
            Source::Items => "items".into(),
            Source::Whole => "staging".into(),
            Source::Overlay => {
                format!("(SELECT {COLUMNS} FROM staging UNION ALL SELECT {COLUMNS} FROM items p WHERE {UNTOUCHED})")
            }
        }
    }

    /// One recursive step of a CTE over the tree: `select` reads `p`, the
    /// tree's row, and `c`, the CTE's, joined `on`, where `filter` holds.
    fn step(self, select: &str, cte: &str, on: &str, filter: &str) -> String {
        match self {
            Source::Items | Source::Whole => {
                let t = if self == Source::Items { "items" } else { "staging" };
                format!("SELECT {select} FROM {cte} c JOIN {t} p ON {on} WHERE {filter}")
            }
            Source::Overlay => format!(
                "SELECT {select} FROM {cte} c JOIN staging p ON {on} WHERE {filter}
                 UNION ALL
                 SELECT {select} FROM {cte} c JOIN items p ON {on} WHERE {filter} AND {UNTOUCHED}"
            ),
        }
    }
}

/// Everything below `?1` in the tree, as a query of ids.
fn below_sql(source: Source) -> String {
    format!(
        "WITH RECURSIVE below(id, depth) AS (
             SELECT id, 1 FROM {rows} WHERE parent_id = ?1
             UNION ALL
             {step})
         SELECT id FROM below",
        rows = source.rows(),
        step = source.step("p.id, c.depth + 1", "below", "p.parent_id = c.id", &format!("c.depth < {MAX_CHAIN}"))
    )
}

/// Where each item `start` selects is (`id, parent_id, name, placement` of
/// the tree's rows, with `?1` the drive's root): `(id, path, above,
/// own)` — the chain of names from the root, whether every folder above it
/// (below the root) is placed, and whether it is itself. An item whose
/// chain does not reach the root is left out. One query for the lot.
fn chains_sql(source: Source, start: &str) -> String {
    chains_then(source, start, "SELECT start, path, above, own FROM chain WHERE parent_id = ?1")
}

/// [`chains_sql`] with a query of its own over `chain(start, parent_id,
/// path, above, own)`: a row whose `parent_id` is the root (`?1`) has its
/// whole path.
fn chains_then(source: Source, start: &str, then: &str) -> String {
    format!(
        "WITH RECURSIVE chain(start, parent_id, path, above, own, depth) AS (
             SELECT id, parent_id, name, 1, placement = 'placed', 0 FROM ({start}) WHERE id != ?1
             UNION ALL
             {step})
         {then}",
        step = source.step(
            "c.start, p.parent_id, p.name || '/' || c.path, c.above AND p.placement = 'placed', c.own, c.depth + 1",
            "chain",
            "p.id = c.parent_id",
            &format!("c.depth < {MAX_CHAIN} AND c.parent_id != ?1")
        )
    )
}

/// Thumbnails to make, with their paths, and the id to go on from
/// ([`TreeStore::thumbnail_candidates`]).
pub type ThumbnailBatch = (Vec<(Row, PathBuf)>, Option<String>);

/// One item's place, as [`chains_sql`] finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    pub id: String,
    pub rel: PathBuf,
    /// Every folder above it (below the root) is placed.
    pub above: bool,
    /// It is placed itself.
    pub own: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    #[error("the tree store: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("the tree store: {0}")]
    Io(#[from] std::io::Error),
    #[error("the tree store has schema version {0:?}; this daemon knows {SCHEMA_VERSION}")]
    Schema(Option<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Items,
    Staging,
}

impl Table {
    fn name(self) -> &'static str {
        match self {
            Table::Items => "items",
            Table::Staging => "staging",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Folder,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Folder => "folder",
        }
    }
}

/// Why an item is not in the folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkipReason {
    NameTooLong,
    PersonalVault,
    Shared,
    OneNote,
    ReservedName,
    Unsupported,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::NameTooLong => "name-too-long",
            SkipReason::PersonalVault => "personal-vault",
            SkipReason::Shared => "shared",
            SkipReason::OneNote => "onenote",
            SkipReason::ReservedName => "reserved-name",
            SkipReason::Unsupported => "unsupported",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::NameTooLong, Self::PersonalVault, Self::Shared, Self::OneNote, Self::ReservedName, Self::Unsupported]
            .into_iter()
            .find(|reason| reason.as_str() == value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Placed,
    Skipped(SkipReason),
}

impl Placement {
    fn encode(self) -> String {
        match self {
            Placement::Placed => "placed".into(),
            Placement::Skipped(reason) => format!("skipped:{}", reason.as_str()),
        }
    }

    fn decode(value: &str) -> Self {
        match value.strip_prefix("skipped:") {
            None => Placement::Placed,
            Some(reason) => Placement::Skipped(SkipReason::parse(reason).unwrap_or(SkipReason::Unsupported)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    /// `None` only for the drive's root.
    pub parent_id: Option<String>,
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    /// `fileSystemInfo.lastModifiedDateTime`, Unix seconds.
    pub mtime: i64,
    pub etag: Option<String>,
    pub ctag: Option<String>,
    /// `file.hashes.quickXorHash`, base64.
    pub quickxor: Option<String>,
    pub mime: Option<String>,
    /// The item's own placement; an item inside a skipped folder is not placed
    /// either, which [`TreeStore::locate`] works out.
    pub placement: Placement,
}

/// One entry of the delta feed, as the store applies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Root(Row),
    Upsert(Row),
    Delete(String),
}

impl Change {
    pub fn id(&self) -> &str {
        match self {
            Change::Root(row) | Change::Upsert(row) => &row.id,
            Change::Delete(id) => id,
        }
    }
}

/// Where an item is, relative to the sync root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub rel: PathBuf,
    /// The item and every folder above it (below the root) are placed.
    pub placed: bool,
    /// 0 for the root.
    pub depth: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Every item but the root.
    pub listed: u64,
    /// Items in the folder: placed, under placed folders.
    pub placed: u64,
    /// Skipped items whose folder is placed — what `Skipped()` lists.
    pub skipped: u64,
}

/// One event of the activity log, as stored: unix seconds, one
/// of the kinds `sync::activity::Kind` names, a full path and a detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRow {
    pub at: i64,
    pub kind: String,
    pub path: String,
    pub detail: String,
}

/// A local version kept because the file changed or was removed in
/// OneDrive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRow {
    pub at: i64,
    /// Where it was, as a full path.
    pub original: String,
    /// Where it is now, as a full path.
    pub rescued: String,
    pub kind: ConflictKind,
}

/// How a local version was kept (`Conflicts.List()`'s fourth field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    /// Moved out of the way, out of the folder (the read phase's rescue).
    Rescued,
    /// Kept as a copy beside the original, in a read-write folder, and
    /// uploaded (`docs/design/writes.md` §7).
    Copy,
}

impl ConflictKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rescued => "rescued",
            Self::Copy => "copy",
        }
    }

    fn parse(value: &str) -> Self {
        if value == "copy" {
            Self::Copy
        } else {
            Self::Rescued
        }
    }
}

/// What a Graph item becomes in the tree.
pub fn classify(item: &DriveItem) -> Change {
    if item.deleted.is_some() {
        return Change::Delete(item.id.clone());
    }
    let mut row = Row {
        id: item.id.clone(),
        parent_id: item.parent_reference.as_ref().and_then(|p| p.id.clone()),
        name: item.name.clone().unwrap_or_default(),
        kind: if item.folder.is_some() || item.package.is_some() { Kind::Folder } else { Kind::File },
        size: 0,
        mtime: item.mtime(),
        etag: item.e_tag.clone(),
        ctag: item.c_tag.clone(),
        quickxor: item.quick_xor_hash().map(str::to_owned),
        mime: item.file.as_ref().and_then(|f| f.mime_type.clone()),
        placement: Placement::Placed,
    };
    if item.root.is_some() {
        row.parent_id = None;
        row.name = String::new();
        row.kind = Kind::Folder;
        return Change::Root(row);
    }
    if row.kind == Kind::File {
        row.size = item.size.unwrap_or(0);
    }
    if let Some(reason) = skip_reason(item, &row.name) {
        row.placement = Placement::Skipped(reason);
    }
    Change::Upsert(row)
}

/// Whether an item id can be a name in a directory: the materializer keeps a
/// misplaced item in the holding directory under its id, so an id
/// that is empty, `.`, `..`, or holds a `/` or a NUL could name something else.
pub fn usable_id(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains('/') && !id.contains('\0')
}

fn skip_reason(item: &DriveItem, name: &str) -> Option<SkipReason> {
    if !usable_id(&item.id) {
        return Some(SkipReason::Unsupported);
    }
    if item.remote_item.is_some() {
        return Some(SkipReason::Shared);
    }
    if item.package.is_some() {
        return Some(SkipReason::OneNote);
    }
    if item.special_folder.as_ref().and_then(|s| s.name.as_deref()) == Some("vault") {
        return Some(SkipReason::PersonalVault);
    }
    if item.file.is_none() && item.folder.is_none() {
        return Some(SkipReason::Unsupported);
    }
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Some(SkipReason::Unsupported);
    }
    if name.len() > NAME_MAX {
        return Some(SkipReason::NameTooLong);
    }
    if name.starts_with(RESERVED_PREFIX) {
        return Some(SkipReason::ReservedName);
    }
    None
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
                 SELECT c.id, 1 FROM {table} c JOIN {table} p ON c.parent_id = p.id WHERE p.placement != 'placed'
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

pub struct TreeStore {
    conn: Connection,
    /// Where it is on disk; `None` in memory.
    path: Option<PathBuf>,
    /// `staging` holds a whole new tree ([`STAGING_WHOLE`]), not a delta.
    whole: bool,
    /// What changed in the outbox since it was last asked (issue #38).
    changes: std::sync::Arc<outbox::OutboxChanges>,
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
                                         kind TEXT NOT NULL DEFAULT 'rescued');
                 {}
                 INSERT INTO meta (key, value) VALUES ('schema_version', '{SCHEMA_VERSION}');
                 COMMIT;",
                outbox::SCHEMA
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
        conn.execute_batch(SCALE)?;
        outbox::upgrade(&conn)?;
        let whole = conn.query_row("SELECT 1 FROM meta WHERE key = ?1", [STAGING_WHOLE], |_| Ok(())).optional()?.is_some();
        // The outbox's point queries run thousands of times in one examination.
        conn.set_prepared_statement_cache_capacity(64);
        let changes = std::sync::Arc::new(outbox::OutboxChanges::default());
        outbox::watch(&conn, &changes)?;
        Ok(Self { conn, path: None, whole, changes })
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>, TreeError> {
        let value: Option<Option<String>> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| row.get(0))
            .optional()?;
        Ok(value.flatten())
    }

    pub fn set_meta(&self, key: &str, value: Option<&str>) -> Result<(), TreeError> {
        match value {
            Some(value) => self.conn.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?,
            None => self.conn.execute("DELETE FROM meta WHERE key = ?1", [key])?,
        };
        Ok(())
    }

    pub fn delta_link(&self) -> Result<Option<String>, TreeError> {
        self.meta("delta_link")
    }

    /// Where a first listing placed page by page goes on from:
    /// the link to the page after the last one placed, or `""` — the start —
    /// before its first page is committed. `None` when no such listing is
    /// under way.
    pub fn listing_next(&self) -> Result<Option<String>, TreeError> {
        self.meta(LISTING_NEXT)
    }

    /// A first listing placed page by page begins: under way, at
    /// the start, before anything of it is placed.
    pub fn begin_placing(&self) -> Result<(), TreeError> {
        self.set_meta(LISTING_NEXT, Some(""))
    }

    /// Where `table`'s rows are.
    fn source(&self, table: Table) -> Source {
        match (table, self.whole) {
            (Table::Items, _) => Source::Items,
            (Table::Staging, true) => Source::Whole,
            (Table::Staging, false) => Source::Overlay,
        }
    }

    /// Whether `table` holds no row at all, the root's included.
    pub fn is_empty(&self, table: Table) -> Result<bool, TreeError> {
        let sql = format!("SELECT 1 FROM {} LIMIT 1", self.source(table).rows());
        let any: Option<i64> = self.conn.query_row(&sql, [], |row| row.get(0)).optional()?;
        Ok(any.is_none())
    }

    pub fn root_item_id(&self) -> Result<Option<String>, TreeError> {
        self.meta("root_item_id")
    }

    pub fn get(&self, table: Table, id: &str) -> Result<Option<Row>, TreeError> {
        get_row(&self.conn, self.source(table), id)
    }

    pub fn children(&self, table: Table, id: &str) -> Result<Vec<Row>, TreeError> {
        let sql = format!("SELECT {ROW_COLUMNS} FROM {} WHERE parent_id = ?1 ORDER BY name", self.source(table).rows());
        let mut statement = self.conn.prepare_cached(&sql)?;
        let rows = statement.query_map([id], row_from)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn descendants(&self, table: Table, id: &str) -> Result<Vec<String>, TreeError> {
        descendants_in(&self.conn, self.source(table), id)
    }

    /// Where `id` is: the chain of names from the root. `None` for an item that
    /// is not in the table, or whose chain does not reach the drive's root —
    /// its parent never arrived, or the chain is a cycle.
    pub fn locate(&self, table: Table, id: &str) -> Result<Option<Located>, TreeError> {
        let root = self.root_item_id()?;
        let source = self.source(table);
        let sql = format!(
            "WITH RECURSIVE chain(id, parent_id, name, placement, depth) AS (
                 SELECT id, parent_id, name, placement, 0 FROM {rows} WHERE id = ?1
                 UNION ALL
                 {step})
             SELECT id, parent_id, name, placement FROM chain ORDER BY depth DESC",
            rows = source.rows(),
            step = source.step("p.id, p.parent_id, p.name, p.placement, c.depth + 1", "chain", "p.id = c.parent_id", &format!("c.depth < {MAX_CHAIN}"))
        );
        let mut statement = self.conn.prepare_cached(&sql)?;
        let chain: Vec<(String, Option<String>, String, String)> = statement
            .query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
            .collect::<Result<_, _>>()?;
        let Some((top, top_parent, _, _)) = chain.first() else {
            return Ok(None);
        };
        if top_parent.is_some() || Some(top.as_str()) != root.as_deref() {
            return Ok(None);
        }
        let below = &chain[1..];
        Ok(Some(Located {
            rel: below.iter().map(|(_, _, name, _)| name.as_str()).collect(),
            placed: below.iter().all(|(_, _, _, placement)| placement == "placed"),
            depth: below.len(),
        }))
    }

    /// Where each item `start` selects is — a query of `id, parent_id, name,
    /// placement` over `table`'s rows, with `params` from `?2` on (`?1` is
    /// the drive's root) — in one query ([`chains_sql`]). Items whose chain
    /// does not reach the root are left out.
    pub(crate) fn chains(&self, table: Table, start: &str, params: &[&dyn rusqlite::ToSql]) -> Result<Vec<Chain>, TreeError> {
        let Some(root) = self.root_item_id()? else { return Ok(Vec::new()) };
        let sql = chains_sql(self.source(table), start);
        let mut all: Vec<&dyn rusqlite::ToSql> = vec![&root];
        all.extend_from_slice(params);
        let mut statement = self.conn.prepare_cached(&sql)?;
        let chains = statement
            .query_map(all.as_slice(), |r| {
                Ok(Chain { id: r.get(0)?, rel: PathBuf::from(r.get::<_, String>(1)?), above: r.get(2)?, own: r.get(3)? })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(chains)
    }

    /// Starts building a new tree: a full listing's from nothing
    /// (`copy_items` false), or a delta's over `items`, which it leaves as it
    /// is until the swap.
    pub fn begin_staging(&mut self, copy_items: bool) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM staging", [])?;
        tx.execute("DELETE FROM staging_gone", [])?;
        if copy_items {
            tx.execute("DELETE FROM meta WHERE key = ?1", [STAGING_WHOLE])?;
        } else {
            tx.execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, '1')", [STAGING_WHOLE])?;
        }
        tx.commit()?;
        self.whole = !copy_items;
        Ok(())
    }

    /// Applies delta entries to the new tree, in order. Deleting a folder
    /// takes whatever is still inside it in the new tree — an item the feed
    /// moved out before, or moves out after, survives (the order of a batch
    /// is not the order of events).
    pub fn stage(&mut self, changes: &[Change]) -> Result<(), TreeError> {
        let source = self.source(Table::Staging);
        let tx = self.conn.transaction()?;
        apply(&tx, source, changes)?;
        tx.commit()?;
        Ok(())
    }

    /// The new tree becomes `items`, with the link to ask from next time, in
    /// one transaction. A delta's swap writes only the rows it staged and
    /// removes only what it removed (issue #39); a full listing's replaces
    /// every row. The version a cached thumbnail was made for, the local
    /// inode and the last outbox commit travel along: a row staged without
    /// them keeps what `items` has — but for the inode of a row `items` does
    /// not place, which is no object of a row placed again (issue #104).
    pub fn commit_staging(&mut self, delta_link: &str) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        if self.whole {
            tx.execute(
                "UPDATE staging SET thumb_key = (SELECT i.thumb_key FROM items i WHERE i.id = staging.id)
                  WHERE thumb_key IS NULL",
                [],
            )?;
            // A row that turns placed again takes no object from `items`:
            // whatever was there when it stopped being placed is gone
            // (issue #104).
            tx.execute(
                "UPDATE staging SET local_handle = (SELECT i.local_handle FROM items i WHERE i.id = staging.id AND (i.placement = 'placed' OR staging.placement != 'placed'))
                  WHERE local_handle IS NULL",
                [],
            )?;
            tx.execute(
                "UPDATE staging SET local_seq = MAX(local_seq, COALESCE((SELECT i.local_seq FROM items i WHERE i.id = staging.id), 0))",
                [],
            )?;
            tx.execute("DELETE FROM items", [])?;
            tx.execute(&format!("INSERT INTO items ({COLUMNS}) SELECT {COLUMNS} FROM staging"), [])?;
        } else {
            tx.execute(
                &format!(
                    "INSERT INTO items ({COLUMNS})
                     SELECT s.id, s.parent_id, s.name, s.kind, s.size, s.mtime, s.etag, s.ctag, s.quickxor, s.mime, s.placement,
                            COALESCE(s.thumb_key, i.thumb_key), COALESCE(s.local_handle, CASE WHEN i.placement = 'placed' OR s.placement != 'placed' THEN i.local_handle END),
                            MAX(s.local_seq, COALESCE(i.local_seq, 0))
                       FROM staging s LEFT JOIN items i ON i.id = s.id WHERE true
                     ON CONFLICT(id) DO UPDATE SET
                       parent_id = excluded.parent_id, name = excluded.name, kind = excluded.kind,
                       size = excluded.size, mtime = excluded.mtime, etag = excluded.etag, ctag = excluded.ctag,
                       quickxor = excluded.quickxor, mime = excluded.mime, placement = excluded.placement,
                       thumb_key = excluded.thumb_key, local_handle = excluded.local_handle, local_seq = excluded.local_seq"
                ),
                [],
            )?;
            tx.execute("DELETE FROM items WHERE id IN (SELECT id FROM staging_gone)", [])?;
        }
        tx.execute("DELETE FROM staging", [])?;
        tx.execute("DELETE FROM staging_gone", [])?;
        tx.execute("DELETE FROM meta WHERE key = ?1", [STAGING_WHOLE])?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('delta_link', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [delta_link],
        )?;
        // A first listing placed page by page ends here too.
        tx.execute("DELETE FROM meta WHERE key = ?1", [LISTING_NEXT])?;
        tx.commit()?;
        self.whole = false;
        Ok(())
    }

    /// One page of a first listing, placed: its entries applied to
    /// `items` as [`stage`](Self::stage) applies them to the new tree, and
    /// `next`, the link to the page after it, kept as where the listing goes
    /// on from — in one transaction, so that a listing stopped anywhere
    /// resumes with every page placed so far and asks for none of them
    /// again. Entries whose folder has not come yet go in too; they are
    /// placed when it comes. A delta staged over `items` is done with: the
    /// new tree is `items` again.
    pub fn commit_page(&mut self, changes: &[Change], next: &str) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        apply(&tx, Source::Items, changes)?;
        // The handles the placement recorded in `staging` for items that
        // were not in `items` yet.
        tx.execute(
            "UPDATE items SET local_handle = s.local_handle FROM staging s
              WHERE s.id = items.id AND items.local_handle IS NULL AND s.local_handle IS NOT NULL",
            [],
        )?;
        if !self.whole {
            tx.execute("DELETE FROM staging", [])?;
            tx.execute("DELETE FROM staging_gone", [])?;
        }
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![LISTING_NEXT, next],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Ids that differ between `items` and the new tree — added, removed or
    /// changed in any column but the local ones. For a delta, read from what
    /// it staged alone.
    pub fn changed_ids(&self) -> Result<Vec<String>, TreeError> {
        let sql = if self.whole {
            format!(
                "SELECT id FROM (SELECT {ROW_COLUMNS} FROM staging EXCEPT SELECT {ROW_COLUMNS} FROM items)
                 UNION
                 SELECT id FROM (SELECT {ROW_COLUMNS} FROM items EXCEPT SELECT {ROW_COLUMNS} FROM staging)"
            )
        } else {
            "SELECT s.id FROM staging s LEFT JOIN items i ON i.id = s.id
              WHERE i.id IS NULL OR s.parent_id IS NOT i.parent_id OR s.name IS NOT i.name OR s.kind IS NOT i.kind
                 OR s.size IS NOT i.size OR s.mtime IS NOT i.mtime OR s.etag IS NOT i.etag OR s.ctag IS NOT i.ctag
                 OR s.quickxor IS NOT i.quickxor OR s.mime IS NOT i.mime OR s.placement IS NOT i.placement
             UNION
             SELECT id FROM staging_gone"
                .to_owned()
        };
        let mut statement = self.conn.prepare(&sql)?;
        let ids = statement.query_map([], |row| row.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Every item but the root: what is listed, counted without a walk.
    pub fn listed_count(&self) -> Result<u64, TreeError> {
        let root = self.root_item_id()?.unwrap_or_default();
        let listed: i64 = self.conn.query_row("SELECT count(*) FROM items WHERE id != ?1", [&root], |row| row.get(0))?;
        Ok(listed as u64)
    }

    /// What is listed, placed and skipped in `items`: a walk of the whole
    /// tree, asked for once per cycle that changed it (issue #39).
    pub fn counts(&self) -> Result<Counts, TreeError> {
        let Some(root) = self.root_item_id()? else {
            return Ok(Counts::default());
        };
        let listed = self.listed_count()?;
        let (placed, skipped): (i64, i64) = self.conn.query_row(
            &format!(
                "WITH RECURSIVE placed(id, depth) AS (
                     SELECT ?1, 0
                     UNION ALL
                     SELECT c.id, p.depth + 1 FROM items c JOIN placed p ON c.parent_id = p.id
                      WHERE c.placement = 'placed' AND p.depth < {MAX_CHAIN})
                 SELECT (SELECT count(*) - 1 FROM placed),
                        (SELECT count(*) FROM items s JOIN placed p ON s.parent_id = p.id WHERE s.placement != 'placed')"
            ),
            [&root],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(Counts { listed, placed: placed as u64, skipped: skipped as u64 })
    }

    /// The skipped items `Skipped()` lists: those whose own folder is in the
    /// folder. What is inside a skipped folder is covered by that folder's
    /// line. One query, from the index of skipped items up to the root
    /// (issue #39).
    pub fn skipped(&self) -> Result<Vec<(PathBuf, SkipReason)>, TreeError> {
        let Some(root) = self.root_item_id()? else {
            return Ok(Vec::new());
        };
        let sql = chains_then(
            Source::Items,
            "SELECT id, parent_id, name, placement FROM items WHERE placement != 'placed'",
            "SELECT c.path, i.placement FROM chain c JOIN items i ON i.id = c.start WHERE c.parent_id = ?1 AND c.above",
        );
        let mut statement = self.conn.prepare_cached(&sql)?;
        let mut out = Vec::new();
        for row in statement.query_map([&root], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (path, placement) = row?;
            if let Placement::Skipped(reason) = Placement::decode(&placement) {
                out.push((PathBuf::from(path), reason));
            }
        }
        out.sort();
        Ok(out)
    }

    /// Appends `events` to the activity log and drops all but
    /// the newest [`ACTIVITY_KEPT`], in one transaction.
    pub fn add_activity(&mut self, events: &[ActivityRow]) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        for event in events {
            tx.execute(
                "INSERT INTO activity (at, kind, path, detail) VALUES (?1, ?2, ?3, ?4)",
                params![event.at, event.kind, event.path, event.detail],
            )?;
        }
        tx.execute(
            "DELETE FROM activity WHERE id NOT IN (SELECT id FROM activity ORDER BY id DESC LIMIT ?1)",
            [ACTIVITY_KEPT as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The newest `limit` events, newest first.
    pub fn recent_activity(&self, limit: usize) -> Result<Vec<ActivityRow>, TreeError> {
        let mut statement =
            self.conn.prepare("SELECT at, kind, path, detail FROM activity ORDER BY id DESC LIMIT ?1")?;
        let rows = statement
            .query_map([limit.min(ACTIVITY_KEPT) as i64], |row| {
                Ok(ActivityRow { at: row.get(0)?, kind: row.get(1)?, path: row.get(2)?, detail: row.get(3)? })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Records local versions a reconcile kept: `(at, original, rescued,
    /// kind)`, full paths. A path kept at again replaces its row.
    pub fn add_conflicts(&mut self, conflicts: &[ConflictRow]) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        for c in conflicts {
            tx.execute(
                "INSERT INTO conflicts (rescued, at, original, kind) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(rescued) DO UPDATE SET at = excluded.at, original = excluded.original, kind = excluded.kind",
                params![c.rescued, c.at, c.original, c.kind.as_str()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every recorded conflict, newest first.
    pub fn conflicts(&self) -> Result<Vec<ConflictRow>, TreeError> {
        let mut statement =
            self.conn.prepare("SELECT at, original, rescued, kind FROM conflicts ORDER BY at DESC, rescued")?;
        let rows = statement
            .query_map([], |row| {
                Ok(ConflictRow {
                    at: row.get(0)?,
                    original: row.get(1)?,
                    rescued: row.get(2)?,
                    kind: ConflictKind::parse(&row.get::<_, String>(3)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Up to `limit` conflicts whose rescued path sorts after `after`, in
    /// that order.
    pub fn conflicts_after(&self, after: &str, limit: usize) -> Result<Vec<ConflictRow>, TreeError> {
        let mut statement =
            self.conn.prepare_cached("SELECT at, original, rescued, kind FROM conflicts WHERE rescued > ?1 ORDER BY rescued LIMIT ?2")?;
        let rows = statement
            .query_map(params![after, limit as i64], |row| {
                Ok(ConflictRow { at: row.get(0)?, original: row.get(1)?, rescued: row.get(2)?, kind: ConflictKind::parse(&row.get::<_, String>(3)?) })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// How many conflicts are on record.
    pub fn conflict_count(&self) -> Result<u64, TreeError> {
        Ok(self.conn.query_row("SELECT count(*) FROM conflicts", [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Deletes the conflicts whose rescued files are `rescued`, in one
    /// transaction; how many there were.
    pub fn remove_conflicts(&mut self, rescued: &[String]) -> Result<usize, TreeError> {
        let tx = self.conn.transaction()?;
        let mut removed = 0;
        {
            let mut delete = tx.prepare_cached("DELETE FROM conflicts WHERE rescued = ?1")?;
            for path in rescued {
                removed += delete.execute([path])?;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Deletes the conflict whose rescued file is `rescued`; whether there
    /// was one.
    pub fn remove_conflict(&self, rescued: &str) -> Result<bool, TreeError> {
        Ok(self.conn.execute("DELETE FROM conflicts WHERE rescued = ?1", [rescued])? > 0)
    }

    /// Placed images and videos whose cached thumbnail was not made for what
    /// they are now (`thumb_key`, which `sync::thumbs::thumb_key` writes: the
    /// cTag, the path and the time), with their paths: up to `limit` of them,
    /// looking at the candidates after id `after` in id order (issue #39),
    /// [`THUMB_PAGE`] at a time and at most [`THUMB_SCAN`] in one call. Each
    /// page is filtered and its paths found in one query. Also the id to go
    /// on from, `None` once the last candidate has been looked at.
    pub fn thumbnail_candidates(&self, after: &str, limit: usize) -> Result<ThumbnailBatch, TreeError> {
        if limit == 0 {
            return Ok((Vec::new(), Some(after.to_owned())));
        }
        const PAGE: &str = "SELECT id, parent_id, name, placement FROM items
              WHERE kind = 'file' AND placement = 'placed' AND ctag IS NOT NULL
                AND (mime LIKE 'image/%' OR mime LIKE 'video/%') AND id > ?2
              ORDER BY id LIMIT ?3";
        let Some(root) = self.root_item_id()? else { return Ok((Vec::new(), None)) };
        let wanted = format!(
            "SELECT {}, c.path FROM chain c JOIN items i ON i.id = c.start
              WHERE c.parent_id = ?1 AND c.above AND c.own
                AND (i.thumb_key IS NULL OR i.thumb_key != i.ctag || '|' || c.path || '|' || i.mtime)
              ORDER BY i.id",
            ROW_COLUMNS.split(", ").map(|c| format!("i.{c}")).collect::<Vec<_>>().join(", ")
        );
        let sql = chains_then(Source::Items, PAGE, &wanted);
        let mut out = Vec::new();
        let mut from = after.to_owned();
        let mut scanned = 0;
        loop {
            let (last, n): (Option<String>, usize) = self.conn.prepare_cached(&format!("SELECT max(id), count(*) FROM ({PAGE})"))?.query_row(
                params![root, from, THUMB_PAGE as i64],
                |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as usize)),
            )?;
            let Some(last) = last else { return Ok((out, None)) };
            let mut statement = self.conn.prepare_cached(&sql)?;
            let found = statement.query_map(params![root, from, THUMB_PAGE as i64], |r| Ok((row_from(r)?, PathBuf::from(r.get::<_, String>(11)?))))?;
            for candidate in found {
                out.push(candidate?);
                if out.len() == limit {
                    // The next call looks at the rest of the page again.
                    let taken = out[limit - 1].0.id.clone();
                    return Ok((out, Some(taken)));
                }
            }
            scanned += n;
            if n < THUMB_PAGE {
                return Ok((out, None));
            }
            from = last;
            if scanned >= THUMB_SCAN {
                return Ok((out, Some(from)));
            }
        }
    }

    /// Runs `sql` as it is: the bench seeds a large store fast.
    #[cfg(test)]
    pub fn bench_sql(&self, sql: &str) -> Result<(), TreeError> {
        self.conn.execute_batch(sql)?;
        Ok(())
    }

    /// Records what a cached thumbnail of `id` was made for (`key`), so the
    /// next cycle does not make it again.
    pub fn set_thumb_key(&self, id: &str, key: &str) -> Result<(), TreeError> {
        self.conn.execute("UPDATE items SET thumb_key = ?2 WHERE id = ?1", params![id, key])?;
        Ok(())
    }
}

fn row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    let kind: String = row.get(3)?;
    let placement: String = row.get(10)?;
    Ok(Row {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        name: row.get(2)?,
        kind: if kind == "folder" { Kind::Folder } else { Kind::File },
        size: row.get::<_, i64>(4)? as u64,
        mtime: row.get(5)?,
        etag: row.get(6)?,
        ctag: row.get(7)?,
        quickxor: row.get(8)?,
        mime: row.get(9)?,
        placement: Placement::decode(&placement),
    })
}

/// A row of the tree `source`.
fn get_row(conn: &Connection, source: Source, id: &str) -> Result<Option<Row>, TreeError> {
    let sql = format!("SELECT {ROW_COLUMNS} FROM {} WHERE id = ?1", source.rows());
    Ok(conn.prepare_cached(&sql)?.query_row([id], row_from).optional()?)
}

/// Everything below `id` in the tree `source`.
fn descendants_in(conn: &Connection, source: Source, id: &str) -> Result<Vec<String>, TreeError> {
    let mut statement = conn.prepare_cached(&below_sql(source))?;
    let ids = statement.query_map([id], |row| row.get(0))?.collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// Delta entries applied to the tree `source`, in order (see
/// [`TreeStore::stage`]). Over `items` (a delta staged), a row written is
/// first copied from `items`, local columns and all, and then changed; a row
/// removed is noted in `staging_gone` when `items` has it.
fn apply(tx: &rusqlite::Transaction<'_>, source: Source, changes: &[Change]) -> Result<(), TreeError> {
    for change in changes {
        match change {
            Change::Root(row) => {
                write(tx, source, row)?;
                // Written with the row, as soon as it is staged, rather
                // than deferred to `commit_staging` — the materializer
                // reconciles `staging` against the folder before the
                // commit and needs the drive's root id then, and a
                // drive's root id never changes. Do not move this.
                tx.execute(
                    "INSERT INTO meta (key, value) VALUES ('root_item_id', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    [&row.id],
                )?;
            }
            Change::Upsert(row) => write(tx, source, row)?,
            Change::Delete(id) => match source {
                Source::Items | Source::Whole => {
                    let t = if source == Source::Items { "items" } else { "staging" };
                    tx.execute(
                        &format!(
                            "WITH RECURSIVE below(id, depth) AS (
                                 SELECT id, 1 FROM {t} WHERE parent_id = ?1
                                 UNION ALL
                                 SELECT c.id, b.depth + 1 FROM {t} c JOIN below b ON c.parent_id = b.id
                                  WHERE b.depth < {MAX_CHAIN})
                             DELETE FROM {t} WHERE id IN (SELECT id FROM below)"
                        ),
                        [id],
                    )?;
                    tx.execute(&format!("DELETE FROM {t} WHERE id = ?1"), [id])?;
                }
                Source::Overlay => {
                    let mut gone = descendants_in(tx, source, id)?;
                    gone.push(id.clone());
                    let mut unstage = tx.prepare_cached("DELETE FROM staging WHERE id = ?1")?;
                    let mut note = tx.prepare_cached("INSERT OR IGNORE INTO staging_gone (id) SELECT id FROM items WHERE id = ?1")?;
                    for id in &gone {
                        unstage.execute([id])?;
                        note.execute([id])?;
                    }
                }
            },
        }
    }
    Ok(())
}

/// `row` written into the tree `source` (see [`apply`]).
fn write(tx: &rusqlite::Transaction<'_>, source: Source, row: &Row) -> Result<(), TreeError> {
    match source {
        Source::Items => {
            let was = placement_in_items(tx, &row.id)?;
            upsert(tx, Table::Items, row)?;
            if turns_placed(was, row) {
                forget_subtrees(tx, std::slice::from_ref(&row.id), true, &[])?;
            }
        }
        Source::Whole => {
            upsert(tx, Table::Staging, row)?;
            // What is below it, by `items`: the swap would give it back.
            if turns_placed(placement_in_items(tx, &row.id)?, row) {
                forget_subtrees(tx, std::slice::from_ref(&row.id), false, &[])?;
            }
        }
        Source::Overlay => {
            tx.prepare_cached(&format!("INSERT OR IGNORE INTO staging ({COLUMNS}) SELECT {COLUMNS} FROM items WHERE id = ?1"))?
                .execute([&row.id])?;
            tx.prepare_cached("DELETE FROM staging_gone WHERE id = ?1")?.execute([&row.id])?;
            upsert(tx, Table::Staging, row)?;
            if turns_placed(placement_in_items(tx, &row.id)?, row) {
                // The row itself, and everything below it in both tables:
                // the rows below that the delta does not stage show through
                // from `items` at the swap.
                forget_subtrees(tx, std::slice::from_ref(&row.id), true, &[])?;
            }
        }
    }
    Ok(())
}

/// The subtrees at `roots` — `roots` themselves when `with_roots`, and
/// everything `items` or `staging` has below them — forget their local
/// objects in both tables, and so does every row recording one of
/// `handles` (issue #104): seeded from a temporary table, one statement per
/// table, whatever the number of roots.
pub(crate) fn forget_subtrees(tx: &rusqlite::Transaction<'_>, roots: &[String], with_roots: bool, handles: &[konedrive_fs::handle::FileHandle]) -> Result<(), TreeError> {
    if !handles.is_empty() {
        tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS forget_handles (handle BLOB PRIMARY KEY); DELETE FROM forget_handles;")?;
        {
            let mut handle = tx.prepare_cached("INSERT OR IGNORE INTO forget_handles (handle) VALUES (?1)")?;
            for h in handles {
                handle.execute([h.encode()])?;
            }
        }
        for table in ["items", "staging"] {
            tx.execute(&format!("UPDATE {table} SET local_handle = NULL WHERE local_handle IN (SELECT handle FROM forget_handles)"), [])?;
        }
        tx.execute_batch("DELETE FROM forget_handles;")?;
    }
    if roots.is_empty() {
        return Ok(());
    }
    // Nothing below a single root (a file) and nothing of its own to forget:
    // no recursive query at all, through the parent indexes.
    if !with_roots && roots.len() == 1 {
        let below: bool = tx
            .prepare_cached("SELECT EXISTS (SELECT 1 FROM items WHERE parent_id = ?1) OR EXISTS (SELECT 1 FROM staging WHERE parent_id = ?1)")?
            .query_row([&roots[0]], |r| r.get(0))?;
        if !below {
            return Ok(());
        }
    }
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS forget_roots (id TEXT PRIMARY KEY);
         CREATE TEMP TABLE IF NOT EXISTS forget_below (id TEXT PRIMARY KEY);
         DELETE FROM forget_roots; DELETE FROM forget_below;",
    )?;
    {
        let mut root = tx.prepare_cached("INSERT OR IGNORE INTO forget_roots (id) VALUES (?1)")?;
        for id in roots {
            root.execute([id])?;
        }
    }
    // The set is built once, each step through a table's parent index, and
    // then forgotten in both tables by primary key.
    let from = if with_roots { 0 } else { 1 };
    tx.execute(
        &format!(
            "WITH RECURSIVE below(id, depth) AS (
                 SELECT id, 0 FROM forget_roots
                 UNION SELECT c.id, b.depth + 1 FROM below b JOIN items c ON c.parent_id = b.id WHERE b.depth < {MAX_CHAIN}
                 UNION SELECT c.id, b.depth + 1 FROM below b JOIN staging c ON c.parent_id = b.id WHERE b.depth < {MAX_CHAIN})
             INSERT OR IGNORE INTO forget_below (id) SELECT id FROM below WHERE depth >= {from}"
        ),
        [],
    )?;
    for table in ["items", "staging"] {
        tx.execute(&format!("UPDATE {table} SET local_handle = NULL WHERE local_handle IS NOT NULL AND id IN (SELECT id FROM forget_below)"), [])?;
    }
    tx.execute_batch("DELETE FROM forget_roots; DELETE FROM forget_below;")?;
    Ok(())
}

/// Item `id`'s own placement in `items`, if it has a row there.
fn placement_in_items(tx: &rusqlite::Transaction<'_>, id: &str) -> Result<Option<Placement>, TreeError> {
    let placement: Option<String> = tx
        .prepare_cached("SELECT placement FROM items WHERE id = ?1")?
        .query_row([id], |r| r.get(0))
        .optional()?;
    Ok(placement.as_deref().map(Placement::decode))
}

/// Whether `row` turns placed again over a row of `items` that was not
/// (`was`). Such a row, and every row below it, carries no local object
/// (issue #104): what was on disk when it stopped being placed was taken
/// off, and its placement records the objects it is placed as. Forgotten
/// as it is staged, so that the placement that follows records them anew.
fn turns_placed(was: Option<Placement>, row: &Row) -> bool {
    row.placement == Placement::Placed && was.is_some_and(|was| was != Placement::Placed)
}

fn upsert(tx: &rusqlite::Transaction<'_>, table: Table, row: &Row) -> rusqlite::Result<usize> {
    tx.prepare_cached(&format!(
            "INSERT INTO {} (id, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
               parent_id = excluded.parent_id, name = excluded.name, kind = excluded.kind,
               size = excluded.size, mtime = excluded.mtime, etag = excluded.etag, ctag = excluded.ctag,
               quickxor = excluded.quickxor, mime = excluded.mime, placement = excluded.placement",
        table.name()
    ))?
    .execute(params![
            row.id,
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
        ])
}

/// Runs `f` on a plain thread of its own and waits for it: for tests that
/// call blocking code (the activity log, the examiner) from async code.
#[cfg(test)]
pub fn off_runtime<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| scope.spawn(f).join().expect("the plain thread panicked"))
}

/// A job for a store's owner thread: a closure over the store, which sends
/// its own answer.
type Job = Box<dyn FnOnce(&mut TreeStore) + Send>;

/// Jobs a store's channel holds before a sender waits (issue #38).
pub const QUEUE: usize = 1024;

/// Hands out the ids of the owner threads.
static NEXT_OWNER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

thread_local! {
    /// The id of the store this thread owns; 0 on any other thread.
    static OWNING: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// One thread that owns a connection and runs the jobs sent to it, one at a
/// time, in the order they arrive (issue #38).
struct Owner {
    jobs: tokio::sync::mpsc::Sender<Job>,
    id: u64,
}

impl Owner {
    /// Starts the thread. It ends, dropping the connection, once every
    /// sender is gone and the jobs already queued have run. After each job
    /// that changed the outbox, `changes` tells those waiting for a change.
    fn spawn(mut store: TreeStore, name: &str, changes: Option<std::sync::Arc<outbox::OutboxChanges>>) -> Self {
        let id = NEXT_OWNER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (jobs, mut queue) = tokio::sync::mpsc::channel::<Job>(QUEUE);
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                OWNING.with(|owning| owning.set(id));
                while let Some(job) = queue.blocking_recv() {
                    let before = changes.as_ref().map(|c| c.generation());
                    // A job that panics answers nobody (its caller gets an error);
                    // its transaction, if any, is rolled back as it is dropped.
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&mut store))).is_err() {
                        tracing::error!("a job of the tree store panicked; the store goes on");
                    }
                    if let (Some(changes), Some(before)) = (&changes, before) {
                        if changes.generation() != before {
                            changes.committed();
                        }
                    }
                }
            })
            .expect("the tree store's thread starts");
        Owner { jobs, id }
    }

    /// A call from inside one of this owner's jobs would wait for itself:
    /// a bug, which panics in debug and test builds and is an error otherwise.
    fn refuse_reentry(&self) -> Result<(), TreeError> {
        if OWNING.with(|owning| owning.get()) != self.id {
            return Ok(());
        }
        debug_assert!(false, "a job of the tree store called the store: it would wait for itself");
        Err(TreeError::Io(std::io::Error::other("a job of the tree store called the store")))
    }

    fn job<T: Send + 'static>(
        f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static,
    ) -> (Job, tokio::sync::oneshot::Receiver<Result<T, TreeError>>) {
        let (answer, answered) = tokio::sync::oneshot::channel();
        let job: Job = Box::new(move |store| {
            let _ = answer.send(f(store));
        });
        (job, answered)
    }

    async fn call<T: Send + 'static>(&self, f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static) -> Result<T, TreeError> {
        self.refuse_reentry()?;
        let (job, answered) = Self::job(f);
        self.jobs.send(job).await.map_err(|_| stopped())?;
        answered.await.map_err(|_| failed())?
    }

    fn call_blocking<T: Send + 'static>(&self, f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static) -> Result<T, TreeError> {
        self.refuse_reentry()?;
        let (job, answered) = Self::job(f);
        self.jobs.blocking_send(job).map_err(|_| stopped())?;
        answered.blocking_recv().map_err(|_| failed())?
    }
}

fn stopped() -> TreeError {
    TreeError::Io(std::io::Error::other("the tree store's thread has stopped"))
}

fn failed() -> TreeError {
    TreeError::Io(std::io::Error::other("a job of the tree store failed"))
}

/// The store, shared by the tasks of one folder: the listing, the
/// materializer, the outbox worker and the D-Bus queries. One thread owns
/// its connection, and everyone else sends it jobs (issue #38): `call` from
/// async code, `call_blocking` from plain threads. Only that thread holds a
/// read-write connection to the store; the bus's reads go to a second,
/// read-only connection with a thread of its own.
#[derive(Clone)]
pub struct Store {
    owner: std::sync::Arc<Owner>,
    changes: std::sync::Arc<outbox::OutboxChanges>,
    /// The read-only connection's owner ([`Store::read`]), started when
    /// first used; `None` inside when it cannot be opened.
    reader: std::sync::Arc<std::sync::OnceLock<Option<Owner>>>,
    path: Option<PathBuf>,
    /// The pause as `meta` last had it (`outbox::PAUSED_UNTIL`): [`NOT_PAUSED`],
    /// or paused until then, 0 for until resumed. Kept here so that it is
    /// read without a job, from anywhere.
    pause: std::sync::Arc<std::sync::atomic::AtomicI64>,
}

/// [`Store::pause`]'s memory while not paused.
const NOT_PAUSED: i64 = -1;

impl Store {
    pub fn new(store: TreeStore) -> Self {
        let changes = std::sync::Arc::clone(&store.changes);
        let path = store.path.clone();
        let pause = store.meta(outbox::PAUSED_UNTIL).ok().flatten().map_or(NOT_PAUSED, |v| v.parse::<i64>().unwrap_or(0).max(0));
        let owner = Owner::spawn(store, "konedrive-store", Some(std::sync::Arc::clone(&changes)));
        Self {
            owner: std::sync::Arc::new(owner),
            changes,
            reader: Default::default(),
            path,
            pause: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(pause)),
        }
    }

    /// The pause as last written: paused until then (unix seconds, 0 for
    /// until resumed), or `None`. From memory: no job.
    pub fn pause(&self) -> Option<i64> {
        let until = self.pause.load(std::sync::atomic::Ordering::SeqCst);
        (until != NOT_PAUSED).then_some(until)
    }

    /// Writes the pause (`None`: resumed), and remembers it.
    pub async fn set_pause(&self, until: Option<i64>) -> Result<(), TreeError> {
        self.call(move |s| s.set_meta(outbox::PAUSED_UNTIL, until.map(|u| u.to_string()).as_deref())).await?;
        self.pause.store(until.map_or(NOT_PAUSED, |u| u.max(0)), std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// [`set_pause`](Self::set_pause) for plain threads.
    pub fn set_pause_blocking(&self, until: Option<i64>) -> Result<(), TreeError> {
        self.call_blocking(move |s| s.set_meta(outbox::PAUSED_UNTIL, until.map(|u| u.to_string()).as_deref()))?;
        self.pause.store(until.map_or(NOT_PAUSED, |u| u.max(0)), std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// A timed pause until `until` has run out: forgotten here, and taken
    /// off `meta` by a job nobody waits for (unless a pause was written since).
    pub fn pause_ended(&self, until: i64) {
        use std::sync::atomic::Ordering;
        if self.pause.compare_exchange(until, NOT_PAUSED, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return;
        }
        let (job, _) = Owner::job(move |s| {
            if s.meta(outbox::PAUSED_UNTIL)?.and_then(|v| v.parse::<i64>().ok()) == Some(until) {
                s.set_meta(outbox::PAUSED_UNTIL, None)?;
            }
            Ok(())
        });
        let _ = self.owner.jobs.try_send(job);
    }

    /// Runs `f` on the store's thread and waits for its answer: for async
    /// code. Jobs run one at a time, in the order they arrive; `f` may hold a
    /// transaction, and must never await, block on anything but SQLite, or
    /// call the store.
    pub async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static,
    ) -> Result<T, TreeError> {
        self.owner.call(f).await
    }

    /// [`call`](Self::call) for plain threads (the examiner, the
    /// materializer, the body of a `spawn_blocking`): waits for the answer.
    /// On an async runtime's thread it panics (tokio refuses to block there).
    pub fn call_blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static,
    ) -> Result<T, TreeError> {
        self.owner.call_blocking(f)
    }

    /// The read-only connection's owner, started when first asked for; `None`
    /// for a store in memory, or one that cannot be opened for reading alone.
    fn reader(&self) -> Option<&Owner> {
        let path = self.path.as_ref()?;
        self.reader
            .get_or_init(|| match TreeStore::open_read_only(path) {
                Ok(opened) => Some(Owner::spawn(opened, "konedrive-store-read", None)),
                Err(e) => {
                    tracing::warn!("the tree store cannot be opened for reading alone ({e}); it is read through its own thread");
                    None
                }
            })
            .as_ref()
    }

    /// Runs `f` on the store's read-only connection, which never waits for a
    /// writer and sees what was last committed (issue #38): the bus's lists and
    /// sums. A store in memory, or one whose second connection cannot be
    /// opened, is read through its own thread.
    pub async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static,
    ) -> Result<T, TreeError> {
        match self.reader() {
            Some(reader) => reader.call(f).await,
            None => self.call(f).await,
        }
    }

    /// [`read`](Self::read) for plain threads.
    pub fn read_blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static,
    ) -> Result<T, TreeError> {
        match self.reader() {
            Some(reader) => reader.call_blocking(f),
            None => self.call_blocking(f),
        }
    }

    /// What changed in the outbox, shared with the store.
    pub fn changes(&self) -> &std::sync::Arc<outbox::OutboxChanges> {
        &self.changes
    }
}

#[cfg(test)]
mod tests;
