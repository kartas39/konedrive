//! The tree store: one row per file and folder of the
//! drive, and the delta link. A map, not the truth — the extended attributes
//! on the files are that — so it is rebuilt from a full listing whenever it
//! cannot be used, and losing it costs one listing, never data.
//!
//! `items` is the tree the folder was last made to match; `staging` is the
//! tree a cycle is building. The folder is reconciled against `staging`, and
//! only then does `staging` replace `items`, so a crash in between
//! leaves `items` and the delta link as they were and the next cycle asks for
//! the same changes again.
//!
//! A folder's first listing is the one exception: each page goes
//! into `items` as soon as it is placed, with the link to the next page
//! (`listing_next`) in the same transaction, so that a listing stopped
//! part-way resumes where it stopped. `commit_staging` ends it as it ends
//! every listing.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use crate::drive::item::{DriveItem, NAME_MAX, RESERVED_PREFIX};

/// Version 2 added `activity` and `conflicts`. A store of any
/// other version is rebuilt from a full listing, so a
/// version 1 store is rebuilt once, and loses nothing but a listing.
pub const SCHEMA_VERSION: &str = "2";

/// How many activity events the store keeps: the oldest go.
pub const ACTIVITY_KEPT: usize = 200;

/// A chain of parents longer than this is a cycle or corruption, not a drive.
const MAX_CHAIN: usize = konedrive_fs::MAX_DEPTH + 2;

/// The `meta` key of a first listing's resume point.
pub const LISTING_NEXT: &str = "listing_next";

const COLUMNS: &str = "id, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement, thumb_key";
const ROW_COLUMNS: &str = "id, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement";

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

/// A local version a reconcile moved out of the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRow {
    pub at: i64,
    /// Where it was, as a full path.
    pub original: String,
    /// Where it is now, as a full path.
    pub rescued: String,
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
        Self::prepare(conn)
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
                schema.push_str(&format!(
                    "CREATE TABLE {table} (
                        id TEXT PRIMARY KEY, parent_id TEXT, name TEXT NOT NULL, kind TEXT NOT NULL,
                        size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL DEFAULT 0,
                        etag TEXT, ctag TEXT, quickxor TEXT, mime TEXT,
                        placement TEXT NOT NULL, thumb_key TEXT);
                     CREATE INDEX {table}_parent ON {table}(parent_id);"
                ));
            }
            schema.push_str(&format!(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE activity (id INTEGER PRIMARY KEY, at INTEGER NOT NULL, kind TEXT NOT NULL,
                                        path TEXT NOT NULL, detail TEXT NOT NULL);
                 CREATE TABLE conflicts (rescued TEXT PRIMARY KEY, at INTEGER NOT NULL, original TEXT NOT NULL);
                 INSERT INTO meta (key, value) VALUES ('schema_version', '{SCHEMA_VERSION}');
                 COMMIT;"
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
        let version: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |row| row.get(0))
            .optional()?;
        if version.as_deref() != Some(SCHEMA_VERSION) {
            return Err(TreeError::Schema(version));
        }
        Ok(Self { conn })
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

    /// Whether `table` holds no row at all, the root's included.
    pub fn is_empty(&self, table: Table) -> Result<bool, TreeError> {
        let any: Option<i64> = self.conn.query_row(&format!("SELECT 1 FROM {} LIMIT 1", table.name()), [], |row| row.get(0)).optional()?;
        Ok(any.is_none())
    }

    pub fn root_item_id(&self) -> Result<Option<String>, TreeError> {
        self.meta("root_item_id")
    }

    pub fn get(&self, table: Table, id: &str) -> Result<Option<Row>, TreeError> {
        let sql = format!("SELECT {ROW_COLUMNS} FROM {} WHERE id = ?1", table.name());
        Ok(self.conn.query_row(&sql, [id], row_from).optional()?)
    }

    pub fn children(&self, table: Table, id: &str) -> Result<Vec<Row>, TreeError> {
        let sql = format!("SELECT {ROW_COLUMNS} FROM {} WHERE parent_id = ?1 ORDER BY name", table.name());
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map([id], row_from)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn descendants(&self, table: Table, id: &str) -> Result<Vec<String>, TreeError> {
        let sql = format!(
            "WITH RECURSIVE below(id, depth) AS (
                 SELECT id, 1 FROM {t} WHERE parent_id = ?1
                 UNION ALL
                 SELECT c.id, b.depth + 1 FROM {t} c JOIN below b ON c.parent_id = b.id WHERE b.depth < {max})
             SELECT id FROM below",
            t = table.name(),
            max = MAX_CHAIN
        );
        let mut statement = self.conn.prepare(&sql)?;
        let ids = statement.query_map([id], |row| row.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Where `id` is: the chain of names from the root. `None` for an item that
    /// is not in the table, or whose chain does not reach the drive's root —
    /// its parent never arrived, or the chain is a cycle.
    pub fn locate(&self, table: Table, id: &str) -> Result<Option<Located>, TreeError> {
        let root = self.root_item_id()?;
        let sql = format!(
            "WITH RECURSIVE chain(id, parent_id, name, placement, depth) AS (
                 SELECT id, parent_id, name, placement, 0 FROM {t} WHERE id = ?1
                 UNION ALL
                 SELECT p.id, p.parent_id, p.name, p.placement, c.depth + 1
                   FROM {t} p JOIN chain c ON p.id = c.parent_id WHERE c.depth < {max})
             SELECT id, parent_id, name, placement FROM chain ORDER BY depth DESC",
            t = table.name(),
            max = MAX_CHAIN
        );
        let mut statement = self.conn.prepare(&sql)?;
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

    /// Starts building a tree in `staging`: empty for a full listing, a copy of
    /// `items` for a delta to be applied on top.
    pub fn begin_staging(&mut self, copy_items: bool) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM staging", [])?;
        if copy_items {
            tx.execute(&format!("INSERT INTO staging ({COLUMNS}) SELECT {COLUMNS} FROM items"), [])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Applies delta entries to `staging`, in order. Deleting a folder takes
    /// whatever is still inside it in `staging` — an item the feed moved out
    /// before, or moves out after, survives (the order of a batch is
    /// not the order of events).
    pub fn stage(&mut self, changes: &[Change]) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        apply(&tx, Table::Staging, changes)?;
        tx.commit()?;
        Ok(())
    }

    /// `staging` becomes `items`, with the link to ask from next time. The
    /// version a cached thumbnail was made for travels along.
    pub fn commit_staging(&mut self, delta_link: &str) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE staging SET thumb_key = (SELECT i.thumb_key FROM items i WHERE i.id = staging.id)
              WHERE thumb_key IS NULL",
            [],
        )?;
        tx.execute("DELETE FROM items", [])?;
        tx.execute(&format!("INSERT INTO items ({COLUMNS}) SELECT {COLUMNS} FROM staging"), [])?;
        tx.execute("DELETE FROM staging", [])?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('delta_link', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [delta_link],
        )?;
        // A first listing placed page by page ends here too.
        tx.execute("DELETE FROM meta WHERE key = ?1", [LISTING_NEXT])?;
        tx.commit()?;
        Ok(())
    }

    /// One page of a first listing, placed: its entries applied to
    /// `items` as [`stage`](Self::stage) applies them to `staging`, and
    /// `next`, the link to the page after it, kept as where the listing goes
    /// on from — in one transaction, so that a listing stopped anywhere
    /// resumes with every page placed so far and asks for none of them
    /// again. Entries whose folder has not come yet go in too; they are
    /// placed when it comes.
    pub fn commit_page(&mut self, changes: &[Change], next: &str) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        apply(&tx, Table::Items, changes)?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![LISTING_NEXT, next],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Ids that differ between `items` and `staging` — added, removed or
    /// changed in any column but `thumb_key`.
    pub fn changed_ids(&self) -> Result<Vec<String>, TreeError> {
        let sql = format!(
            "SELECT id FROM (SELECT {ROW_COLUMNS} FROM staging EXCEPT SELECT {ROW_COLUMNS} FROM items)
             UNION
             SELECT id FROM (SELECT {ROW_COLUMNS} FROM items EXCEPT SELECT {ROW_COLUMNS} FROM staging)"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let ids = statement.query_map([], |row| row.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    pub fn counts(&self, table: Table) -> Result<Counts, TreeError> {
        let Some(root) = self.root_item_id()? else {
            return Ok(Counts::default());
        };
        let t = table.name();
        let listed: i64 = self.conn.query_row(&format!("SELECT count(*) FROM {t} WHERE id != ?1"), [&root], |row| row.get(0))?;
        let (placed, skipped): (i64, i64) = self.conn.query_row(
            &format!(
                "WITH RECURSIVE placed(id, depth) AS (
                     SELECT ?1, 0
                     UNION ALL
                     SELECT c.id, p.depth + 1 FROM {t} c JOIN placed p ON c.parent_id = p.id
                      WHERE c.placement = 'placed' AND p.depth < {MAX_CHAIN})
                 SELECT (SELECT count(*) - 1 FROM placed),
                        (SELECT count(*) FROM {t} s JOIN placed p ON s.parent_id = p.id WHERE s.placement != 'placed')"
            ),
            [&root],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(Counts { listed: listed as u64, placed: placed as u64, skipped: skipped as u64 })
    }

    /// The skipped items `Skipped()` lists: those whose own folder is in the
    /// folder. What is inside a skipped folder is covered by that folder's line.
    pub fn skipped(&self, table: Table) -> Result<Vec<(PathBuf, SkipReason)>, TreeError> {
        let Some(root) = self.root_item_id()? else {
            return Ok(Vec::new());
        };
        let t = table.name();
        let ids: Vec<(String, String)> = {
            let mut statement = self.conn.prepare(&format!(
                "WITH RECURSIVE placed(id, depth) AS (
                     SELECT ?1, 0
                     UNION ALL
                     SELECT c.id, p.depth + 1 FROM {t} c JOIN placed p ON c.parent_id = p.id
                      WHERE c.placement = 'placed' AND p.depth < {MAX_CHAIN})
                 SELECT s.id, s.placement FROM {t} s JOIN placed p ON s.parent_id = p.id
                  WHERE s.placement != 'placed'"
            ))?;
            let rows = statement.query_map([&root], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<Result<_, _>>()?;
            rows
        };
        let mut out = Vec::new();
        for (id, placement) in ids {
            let Placement::Skipped(reason) = Placement::decode(&placement) else { continue };
            if let Some(located) = self.locate(table, &id)? {
                out.push((located.rel, reason));
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

    /// Records local versions a reconcile moved out of the way:
    /// `(at, original, rescued)`, full paths. A path rescued to again replaces
    /// its row.
    pub fn add_conflicts(&mut self, conflicts: &[ConflictRow]) -> Result<(), TreeError> {
        let tx = self.conn.transaction()?;
        for c in conflicts {
            tx.execute(
                "INSERT INTO conflicts (rescued, at, original) VALUES (?1, ?2, ?3)
                 ON CONFLICT(rescued) DO UPDATE SET at = excluded.at, original = excluded.original",
                params![c.rescued, c.at, c.original],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every recorded conflict, newest first.
    pub fn conflicts(&self) -> Result<Vec<ConflictRow>, TreeError> {
        let mut statement =
            self.conn.prepare("SELECT at, original, rescued FROM conflicts ORDER BY at DESC, rescued")?;
        let rows = statement
            .query_map([], |row| Ok(ConflictRow { at: row.get(0)?, original: row.get(1)?, rescued: row.get(2)? }))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Deletes the conflict whose rescued file is `rescued`; whether there
    /// was one.
    pub fn remove_conflict(&self, rescued: &str) -> Result<bool, TreeError> {
        Ok(self.conn.execute("DELETE FROM conflicts WHERE rescued = ?1", [rescued])? > 0)
    }

    /// Placed images and videos whose cached thumbnail was not made for what
    /// they are now (`key`, see `sync::thumbs::thumb_key`), with their paths.
    pub fn thumbnail_candidates(&self, limit: usize, key: impl Fn(&Row, &Path) -> String) -> Result<Vec<(Row, PathBuf)>, TreeError> {
        let sql = format!(
            "SELECT {ROW_COLUMNS}, thumb_key FROM items
              WHERE kind = 'file' AND placement = 'placed' AND ctag IS NOT NULL
                AND (mime LIKE 'image/%' OR mime LIKE 'video/%')"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows: Vec<(Row, Option<String>)> = statement
            .query_map([], |r| Ok((row_from(r)?, r.get(11)?)))?
            .collect::<Result<_, _>>()?;
        let mut out = Vec::new();
        for (row, made_for) in rows {
            let Some(located) = self.locate(Table::Items, &row.id)?.filter(|l| l.placed) else { continue };
            if made_for.as_deref() != Some(key(&row, &located.rel).as_str()) {
                out.push((row, located.rel));
                if out.len() == limit {
                    break;
                }
            }
        }
        Ok(out)
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

/// Delta entries applied to `table`, in order (see [`TreeStore::stage`]).
fn apply(tx: &rusqlite::Transaction<'_>, table: Table, changes: &[Change]) -> Result<(), TreeError> {
    let t = table.name();
    for change in changes {
        match change {
            Change::Root(row) => {
                upsert(tx, table, row)?;
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
            Change::Upsert(row) => {
                upsert(tx, table, row)?;
            }
            Change::Delete(id) => {
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
        }
    }
    Ok(())
}

fn upsert(tx: &rusqlite::Transaction<'_>, table: Table, row: &Row) -> rusqlite::Result<usize> {
    tx.execute(
        &format!(
            "INSERT INTO {} (id, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
               parent_id = excluded.parent_id, name = excluded.name, kind = excluded.kind,
               size = excluded.size, mtime = excluded.mtime, etag = excluded.etag, ctag = excluded.ctag,
               quickxor = excluded.quickxor, mime = excluded.mime, placement = excluded.placement",
            table.name()
        ),
        params![
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
        ],
    )
}

/// The store, shared by the tasks of one folder: the listing, the
/// materializer and the D-Bus queries. A query holds the lock only for itself.
#[derive(Clone)]
pub struct Store(std::sync::Arc<std::sync::Mutex<TreeStore>>);

impl Store {
    pub fn new(store: TreeStore) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(store)))
    }

    /// For synchronous callers already off the async runtime (the materializer).
    pub fn with<T>(&self, f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError>) -> Result<T, TreeError> {
        let mut store = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut store)
    }

    /// For async callers: SQLite is synchronous, so the call runs on a
    /// blocking thread.
    pub async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut TreeStore) -> Result<T, TreeError> + Send + 'static,
    ) -> Result<T, TreeError> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.with(f))
            .await
            .map_err(|e| TreeError::Io(std::io::Error::other(format!("the store task failed: {e}"))))?
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn item(value: serde_json::Value) -> DriveItem {
        serde_json::from_value(value).unwrap()
    }

    fn folder(id: &str, parent: &str, name: &str) -> Change {
        Change::Upsert(Row { id: id.into(), parent_id: Some(parent.into()), name: name.into(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed })
    }

    fn file(id: &str, parent: &str, name: &str) -> Change {
        Change::Upsert(Row { id: id.into(), parent_id: Some(parent.into()), name: name.into(), kind: Kind::File, size: 1, mtime: 0, etag: None, ctag: Some(format!("c-{id}")), quickxor: None, mime: None, placement: Placement::Placed })
    }

    fn root() -> Change {
        Change::Root(Row { id: "R".into(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed })
    }

    fn committed(changes: &[Change]) -> TreeStore {
        let mut store = TreeStore::in_memory().unwrap();
        store.begin_staging(false).unwrap();
        store.stage(changes).unwrap();
        store.commit_staging("link-1").unwrap();
        store
    }

    #[test]
    fn a_file_item_becomes_a_placed_file_row() {
        let change = classify(&item(json!({
            "id": "F", "name": "a.jpg", "size": 7, "eTag": "e", "cTag": "c",
            "parentReference": {"id": "R"},
            "file": {"mimeType": "image/jpeg", "hashes": {"quickXorHash": "q"}},
            "fileSystemInfo": {"lastModifiedDateTime": "2024-05-01T10:00:00Z"}
        })));
        let Change::Upsert(row) = change else { panic!("{change:?}") };
        assert_eq!((row.kind, row.size, row.mtime, row.placement), (Kind::File, 7, 1_714_557_600, Placement::Placed));
        assert_eq!((row.ctag.as_deref(), row.quickxor.as_deref(), row.mime.as_deref()), (Some("c"), Some("q"), Some("image/jpeg")));
        assert_eq!(row.parent_id.as_deref(), Some("R"));
    }

    #[test]
    fn the_root_deletions_and_folders_are_told_apart() {
        assert!(matches!(classify(&item(json!({"id": "R", "root": {}, "folder": {}}))), Change::Root(_)));
        assert_eq!(classify(&item(json!({"id": "X", "deleted": {"state": "deleted"}}))), Change::Delete("X".into()));
        let Change::Upsert(row) = classify(&item(json!({"id": "D", "name": "d", "size": 999, "folder": {}, "parentReference": {"id": "R"}}))) else { panic!() };
        assert_eq!((row.kind, row.size), (Kind::Folder, 0), "a folder's size is its content's, not a file size");
    }

    #[test]
    fn every_skip_reason_is_recognised() {
        let cases = [
            (json!({"id": "1", "name": "я".repeat(128), "file": {}, "parentReference": {"id": "R"}}), SkipReason::NameTooLong),
            (json!({"id": "2", "name": "Personal Vault", "folder": {}, "specialFolder": {"name": "vault"}, "parentReference": {"id": "R"}}), SkipReason::PersonalVault),
            (json!({"id": "3", "name": "shared", "folder": {}, "remoteItem": {"id": "x"}, "parentReference": {"id": "R"}}), SkipReason::Shared),
            (json!({"id": "4", "name": "Notes", "package": {"type": "oneNote"}, "parentReference": {"id": "R"}}), SkipReason::OneNote),
            (json!({"id": "5", "name": ".konedrive-holding", "folder": {}, "parentReference": {"id": "R"}}), SkipReason::ReservedName),
            (json!({"id": "6", "name": "odd", "parentReference": {"id": "R"}}), SkipReason::Unsupported),
            (json!({"id": "7", "name": "..", "file": {}, "parentReference": {"id": "R"}}), SkipReason::Unsupported),
        ];
        for (value, reason) in cases {
            let Change::Upsert(row) = classify(&item(value.clone())) else { panic!("{value}") };
            assert_eq!(row.placement, Placement::Skipped(reason), "{value}");
        }
        let Change::Upsert(fits) = classify(&item(json!({"id": "8", "name": "я".repeat(127), "file": {}, "parentReference": {"id": "R"}}))) else { panic!() };
        assert_eq!(fits.placement, Placement::Placed, "127 Cyrillic letters are 254 bytes and fit");
    }

    #[test]
    fn the_255_byte_name_is_the_exact_boundary() {
        let Change::Upsert(exact) = classify(&item(json!({"id": "9", "name": "a".repeat(255), "file": {}, "parentReference": {"id": "R"}}))) else { panic!() };
        assert_eq!(exact.placement, Placement::Placed, "255 bytes is Linux's limit, inclusive");
        let Change::Upsert(over) = classify(&item(json!({"id": "10", "name": "a".repeat(256), "file": {}, "parentReference": {"id": "R"}}))) else { panic!() };
        assert_eq!(over.placement, Placement::Skipped(SkipReason::NameTooLong), "256 bytes is one over");
    }

    /// An item id becomes a name in the holding directory: an id that cannot
    /// be one keeps the item out of the folder.
    #[test]
    fn an_id_that_cannot_be_a_file_name_is_not_placed() {
        for id in ["", ".", "..", "a/b", "a\0b"] {
            let Change::Upsert(row) = classify(&item(json!({"id": id, "name": "ok.txt", "file": {}, "parentReference": {"id": "R"}}))) else { panic!("{id:?}") };
            assert_eq!(row.placement, Placement::Skipped(SkipReason::Unsupported), "{id:?}");
        }
        let Change::Upsert(fine) = classify(&item(json!({"id": "8F6C!101", "name": "ok.txt", "file": {}, "parentReference": {"id": "R"}}))) else { panic!() };
        assert_eq!(fine.placement, Placement::Placed);
    }

    #[test]
    fn staged_rows_are_invisible_until_committed() {
        let mut store = TreeStore::in_memory().unwrap();
        store.begin_staging(false).unwrap();
        store.stage(&[root(), file("A", "R", "a")]).unwrap();
        assert!(store.get(Table::Items, "A").unwrap().is_none());
        assert!(store.get(Table::Staging, "A").unwrap().is_some());
        store.commit_staging("link-1").unwrap();
        assert!(store.get(Table::Items, "A").unwrap().is_some());
        assert!(store.get(Table::Staging, "A").unwrap().is_none());
        assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-1"));
        assert_eq!(store.root_item_id().unwrap().as_deref(), Some("R"));
    }

    /// A page of a first listing goes into `items` as it was placed,
    /// with the link to the page after it, in one transaction. `staging` is
    /// not written, and a deletion takes what is inside the folder, as it
    /// does in `staging`.
    #[test]
    fn a_placed_page_is_committed_with_where_the_listing_goes_on() {
        let mut store = TreeStore::in_memory().unwrap();
        store.commit_page(&[root(), folder("D", "R", "docs"), file("F", "D", "f")], "next-2").unwrap();
        assert!(store.get(Table::Items, "F").unwrap().is_some());
        assert_eq!(store.listing_next().unwrap().as_deref(), Some("next-2"));
        assert_eq!(store.root_item_id().unwrap().as_deref(), Some("R"));
        assert_eq!(store.delta_link().unwrap(), None, "the listing has not ended");

        store.commit_page(&[Change::Delete("D".into()), file("G", "R", "g")], "next-3").unwrap();
        assert!(store.get(Table::Items, "D").unwrap().is_none());
        assert!(store.get(Table::Items, "F").unwrap().is_none(), "what was inside the deleted folder goes with it");
        assert!(store.get(Table::Items, "G").unwrap().is_some());
        assert_eq!(store.listing_next().unwrap().as_deref(), Some("next-3"));
        assert!(store.get(Table::Staging, "G").unwrap().is_none(), "staging is not written");
    }

    /// An entry whose folder has not come yet is committed all the same, not
    /// reaching the root: it is the only record of it once its page is
    /// committed, and it is placed when its folder comes.
    #[test]
    fn an_entry_whose_folder_has_not_come_survives_a_page_commit() {
        let mut store = TreeStore::in_memory().unwrap();
        store.commit_page(&[root(), file("C", "P", "c")], "next-2").unwrap();
        assert!(store.get(Table::Items, "C").unwrap().is_some(), "kept, though nowhere yet");
        assert_eq!(store.locate(Table::Items, "C").unwrap(), None);
        store.commit_page(&[folder("P", "R", "p")], "next-3").unwrap();
        assert_eq!(store.locate(Table::Items, "C").unwrap(), Some(Located { rel: "p/c".into(), placed: true, depth: 2 }));
    }

    /// A first listing placed page by page is under way from the moment it
    /// begins, before anything of it is placed: at the start until its first
    /// page is committed.
    #[test]
    fn a_listing_begun_is_at_the_start_until_its_first_page() {
        let mut store = TreeStore::in_memory().unwrap();
        store.begin_placing().unwrap();
        assert_eq!(store.listing_next().unwrap().as_deref(), Some(""));
        store.commit_page(&[root()], "next-2").unwrap();
        assert_eq!(store.listing_next().unwrap().as_deref(), Some("next-2"));
    }

    /// The swap that ends every listing ends one placed page by page too: the
    /// delta link comes and the place in the listing goes, together.
    #[test]
    fn the_swap_ends_a_listing_placed_page_by_page() {
        let mut store = TreeStore::in_memory().unwrap();
        store.commit_page(&[root(), file("A", "R", "a")], "next-2").unwrap();
        store.begin_staging(true).unwrap();
        store.stage(&[file("B", "R", "b")]).unwrap();
        store.commit_staging("link-1").unwrap();
        assert_eq!(store.listing_next().unwrap(), None);
        assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-1"));
        assert!(store.get(Table::Items, "A").unwrap().is_some());
        assert!(store.get(Table::Items, "B").unwrap().is_some());
    }

    #[test]
    fn a_table_is_empty_until_a_row_is_in_it() {
        let mut store = TreeStore::in_memory().unwrap();
        assert!(store.is_empty(Table::Items).unwrap());
        store.begin_staging(false).unwrap();
        store.stage(&[root()]).unwrap();
        assert!(store.is_empty(Table::Items).unwrap(), "a staged row is not in items");
        assert!(!store.is_empty(Table::Staging).unwrap());
        store.commit_staging("link-1").unwrap();
        assert!(!store.is_empty(Table::Items).unwrap());
    }

    #[test]
    fn a_path_is_the_chain_of_names_and_placed_only_if_every_link_is() {
        let store = committed(&[
            root(),
            folder("D", "R", "docs"),
            file("F", "D", "f.txt"),
            Change::Upsert(Row { placement: Placement::Skipped(SkipReason::NameTooLong), ..match folder("L", "R", "long") { Change::Upsert(r) => r, _ => unreachable!() } }),
            file("G", "L", "g.txt"),
            file("O", "missing-parent", "o.txt"),
        ]);
        assert_eq!(store.locate(Table::Items, "F").unwrap(), Some(Located { rel: "docs/f.txt".into(), placed: true, depth: 2 }));
        assert_eq!(store.locate(Table::Items, "R").unwrap(), Some(Located { rel: "".into(), placed: true, depth: 0 }));
        assert_eq!(store.locate(Table::Items, "G").unwrap().unwrap().placed, false, "inside a skipped folder");
        assert_eq!(store.locate(Table::Items, "O").unwrap(), None, "an orphan is nowhere");
    }

    #[test]
    fn deleting_a_folder_takes_what_is_still_inside_it() {
        let mut store = committed(&[root(), folder("D", "R", "d"), folder("E", "D", "e"), file("F", "E", "f"), file("K", "D", "keep")]);
        store.begin_staging(true).unwrap();
        // K moves out, then D goes — in the other order too, in the next test.
        store.stage(&[file("K", "R", "keep"), Change::Delete("D".into())]).unwrap();
        for gone in ["D", "E", "F"] {
            assert!(store.get(Table::Staging, gone).unwrap().is_none(), "{gone}");
        }
        assert!(store.get(Table::Staging, "K").unwrap().is_some());
    }

    #[test]
    fn an_item_moved_out_after_its_old_folder_was_deleted_survives() {
        let mut store = committed(&[root(), folder("D", "R", "d"), file("K", "D", "keep")]);
        store.begin_staging(true).unwrap();
        store.stage(&[Change::Delete("D".into())]).unwrap();
        store.stage(&[file("K", "R", "keep")]).unwrap();
        assert_eq!(store.locate(Table::Staging, "K").unwrap().unwrap().rel, PathBuf::from("keep"));
    }

    #[test]
    fn counts_and_the_skipped_list_see_only_what_is_reachable() {
        let store = committed(&[
            root(),
            folder("D", "R", "docs"),
            file("F", "D", "f.txt"),
            Change::Upsert(Row { placement: Placement::Skipped(SkipReason::PersonalVault), ..match folder("V", "R", "Personal Vault") { Change::Upsert(r) => r, _ => unreachable!() } }),
            file("VF", "V", "secret.txt"),
            Change::Upsert(Row { placement: Placement::Skipped(SkipReason::NameTooLong), ..match file("N", "D", "n") { Change::Upsert(r) => r, _ => unreachable!() } }),
        ]);
        assert_eq!(store.counts(Table::Items).unwrap(), Counts { listed: 5, placed: 2, skipped: 2 });
        assert_eq!(
            store.skipped(Table::Items).unwrap(),
            vec![(PathBuf::from("Personal Vault"), SkipReason::PersonalVault), (PathBuf::from("docs/n"), SkipReason::NameTooLong)],
            "what is inside a skipped folder is not listed item by item"
        );
    }

    /// What a delta changed, read back from the two tables: an upsert, a
    /// rename and a delete are the three ids; a row the delta left alone is
    /// not one of them.
    #[test]
    fn the_changed_ids_are_what_staging_differs_from_items_by() {
        let mut store = committed(&[root(), folder("D", "R", "docs"), file("F", "D", "f"), file("G", "D", "g"), file("K", "D", "keep")]);
        store.begin_staging(true).unwrap();
        store.stage(&[file("N", "D", "new"), file("F", "D", "renamed"), Change::Delete("G".into())]).unwrap();
        let mut ids = store.changed_ids().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["F".to_owned(), "G".to_owned(), "N".to_owned()]);
    }

    #[test]
    fn descendants_are_every_level_below() {
        let store = committed(&[root(), folder("D", "R", "d"), folder("E", "D", "e"), file("F", "E", "f")]);
        let mut below = store.descendants(Table::Items, "D").unwrap();
        below.sort();
        assert_eq!(below, vec!["E".to_owned(), "F".to_owned()]);
    }

    #[test]
    fn a_store_survives_reopening_and_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/tree.sqlite");
        {
            let mut store = TreeStore::open(&path).unwrap();
            store.begin_staging(false).unwrap();
            store.stage(&[root(), file("A", "R", "a")]).unwrap();
            store.commit_staging("link-1").unwrap();
        }
        let store = TreeStore::open(&path).unwrap();
        assert!(store.get(Table::Items, "A").unwrap().is_some());
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    /// A permission failure is not corruption: `open` must fail rather than
    /// silently discard a good store (rebuild trigger is missing,
    /// unreadable-as-a-database, or an unknown schema version — never a
    /// transient or permission failure). Skipped under root, which chmod 000
    /// never refuses.
    #[test]
    fn a_permission_error_does_not_rebuild_a_good_store() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: running as root, which chmod 000 cannot refuse");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tree.sqlite");
        {
            let mut store = TreeStore::open(&path).unwrap();
            store.begin_staging(false).unwrap();
            store.stage(&[root(), file("A", "R", "a")]).unwrap();
            store.commit_staging("link-1").unwrap();
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(TreeStore::open(&path).is_err(), "a permission failure must be returned, not treated as corruption");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let store = TreeStore::open(&path).unwrap();
        assert!(store.get(Table::Items, "A").unwrap().is_some(), "the good store must survive a transient open failure");
    }

    #[test]
    fn an_unknown_schema_version_is_rebuilt_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tree.sqlite");
        {
            let mut store = TreeStore::open(&path).unwrap();
            store.begin_staging(false).unwrap();
            store.stage(&[root(), file("A", "R", "a")]).unwrap();
            store.commit_staging("link-1").unwrap();
            store.set_meta("schema_version", Some("99")).unwrap();
        }
        let store = TreeStore::open(&path).unwrap();
        assert!(store.get(Table::Items, "A").unwrap().is_none());
        assert_eq!(store.delta_link().unwrap(), None, "a rebuilt store starts with a full listing");
        assert_eq!(store.meta("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
    }

    /// a schema whose creation a crash cut short —
    /// some tables, no `meta` — is rebuilt, not an error at every open.
    #[test]
    fn a_store_with_tables_and_no_meta_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tree.sqlite");
        rusqlite::Connection::open(&path).unwrap().execute_batch("CREATE TABLE items (id TEXT PRIMARY KEY);").unwrap();
        let store = TreeStore::open(&path).unwrap();
        assert_eq!(store.meta("schema_version").unwrap().as_deref(), Some(SCHEMA_VERSION));
    }

    /// Version 2 added `activity` and `conflicts`, so a store
    /// written by the daemon before them is rebuilt once — from a full
    /// listing, since it comes back with no delta link — and
    /// then kept. With the version left at 1 the old store opens as it is
    /// and has nowhere to put an event.
    #[test]
    fn a_store_from_before_the_activity_log_is_rebuilt_once_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tree.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE items (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT NOT NULL, kind TEXT NOT NULL,
                     size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL DEFAULT 0, etag TEXT, ctag TEXT,
                     quickxor TEXT, mime TEXT, placement TEXT NOT NULL, thumb_key TEXT);
                 CREATE TABLE staging AS SELECT * FROM items;
                 CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 INSERT INTO meta VALUES ('schema_version', '1'), ('delta_link', 'link-1');",
            )
            .unwrap();
        }
        let mut store = TreeStore::open(&path).unwrap();
        assert_eq!(store.delta_link().unwrap(), None, "the old store is rebuilt, so the next cycle lists in full");
        let event = ActivityRow { at: 1, kind: "listed".into(), path: "/f".into(), detail: "1 item".into() };
        store.add_activity(std::slice::from_ref(&event)).unwrap();
        store.set_meta("delta_link", Some("link-2")).unwrap();
        drop(store);
        let store = TreeStore::open(&path).unwrap();
        assert_eq!(store.delta_link().unwrap().as_deref(), Some("link-2"), "rebuilt once, then kept");
        assert_eq!(store.recent_activity(10).unwrap(), vec![event]);
    }

    /// The last 200 events are kept; the oldest go.
    #[test]
    fn the_activity_log_keeps_the_newest_two_hundred() {
        let mut store = TreeStore::in_memory().unwrap();
        let event = |n: i64| ActivityRow { at: n, kind: "downloaded".into(), path: format!("/f{n}"), detail: String::new() };
        store.add_activity(&(1..=150).map(event).collect::<Vec<_>>()).unwrap();
        store.add_activity(&(151..=205).map(event).collect::<Vec<_>>()).unwrap();
        let rows: i64 = store.conn.query_row("SELECT count(*) FROM activity", [], |row| row.get(0)).unwrap();
        assert_eq!(rows, ACTIVITY_KEPT as i64, "the table itself holds no more, not just what is read back");
        let kept = store.recent_activity(1000).unwrap();
        assert_eq!(kept.len(), ACTIVITY_KEPT);
        assert_eq!((kept[0].at, kept[ACTIVITY_KEPT - 1].at), (205, 6), "newest first, the five oldest gone");
        assert_eq!(store.recent_activity(3).unwrap().iter().map(|e| e.at).collect::<Vec<_>>(), vec![205, 204, 203]);
    }

    #[test]
    fn a_conflict_is_listed_until_it_is_removed() {
        let mut store = TreeStore::in_memory().unwrap();
        let row = ConflictRow { at: 7, original: "/root/a.txt".into(), rescued: "/rescued/now/a.txt".into() };
        store.add_conflicts(std::slice::from_ref(&row)).unwrap();
        assert_eq!(store.conflicts().unwrap(), vec![row]);
        assert!(!store.remove_conflict("/elsewhere").unwrap());
        assert!(store.remove_conflict("/rescued/now/a.txt").unwrap());
        assert!(store.conflicts().unwrap().is_empty());
    }

    #[test]
    fn a_file_that_is_not_a_database_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tree.sqlite");
        std::fs::write(&path, b"this is not sqlite at all, not even a little").unwrap();
        let store = TreeStore::open(&path).unwrap();
        assert_eq!(store.delta_link().unwrap(), None);
    }
}
