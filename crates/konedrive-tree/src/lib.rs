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

use konedrive_graph::drive::item::{DriveItem, NAME_MAX, RESERVED_PREFIX};

mod activity;
mod conflicts;
pub mod outbox;
pub mod reconcile;
mod shared;
mod source;
mod staging;
mod thumbs;

pub use activity::{ActivityRow, ACTIVITY_KEPT};
pub use conflicts::{ConflictKind, ConflictRow};
pub use shared::{Store, QUEUE};
use source::{below_sql, chains_sql, chains_then, Source, UNTOUCHED};
use staging::apply;
pub use thumbs::{ThumbnailBatch, THUMB_PAGE, THUMB_SCAN};

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

    /// Runs `sql` as it is: the bench seeds a large store fast.
    #[cfg(any(test, feature = "testing"))]
    pub fn bench_sql(&self, sql: &str) -> Result<(), TreeError> {
        self.conn.execute_batch(sql)?;
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
#[cfg(any(test, feature = "testing"))]
pub fn off_runtime<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| scope.spawn(f).join().expect("the plain thread panicked"))
}

#[cfg(test)]
mod tests;
