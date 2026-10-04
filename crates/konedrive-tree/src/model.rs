//! What the tree holds: a row of `items` and `staging`, a delta entry, the
//! words the store keeps for a kind and a placement, and the one place a
//! row is read from SQLite and written to it.

use std::path::PathBuf;

use rusqlite::params;

/// A row's `placement` while the item is in the folder, as stored.
pub(crate) const PLACED: &str = "placed";
/// What a row's `placement` begins with while the item is not: a
/// [`SkipReason`]'s word follows.
const SKIPPED: &str = "skipped:";

/// SQL for "the row whose placement is `column` is placed", as
/// [`Placement::decode`] reads it: anything that does not begin as a
/// skipped one. A query, an index and the decoder all ask it this way, so
/// a word none of them knows is read the same by each.
pub(crate) fn placed(column: &str) -> String {
    format!("substr({column}, 1, {}) != '{SKIPPED}'", SKIPPED.len())
}

/// SQL for "the row whose placement is `column` is skipped": what
/// [`placed`] is not.
pub(crate) fn skipped(column: &str) -> String {
    format!("substr({column}, 1, {}) = '{SKIPPED}'", SKIPPED.len())
}
/// A row's `kind`, as stored.
pub(crate) const FILE: &str = "file";
const FOLDER: &str = "folder";

/// Every column, for copies between `items` and `staging`: the local ones
/// (`thumb_key`, `local_handle`, `local_seq`) travel with the row.
pub(crate) const COLUMNS: &str = "id, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement, thumb_key, local_handle, local_seq";
/// The columns of a [`Row`], in the order [`row_from`] reads them and
/// [`upsert`] writes them. A query that reads rows selects exactly these
/// first, and whatever else it needs from [`ROW_WIDTH`] on.
pub(crate) const ROW_COLUMNS: &str = "id, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement";
/// How many columns [`ROW_COLUMNS`] names: where a query's own columns begin.
pub(crate) const ROW_WIDTH: usize = width(ROW_COLUMNS);

/// Where `name` is in `list`, a list of column names as a `SELECT` takes
/// it. Used for constants only, so a name the list does not have fails the
/// build, not a query.
pub(crate) const fn column(list: &str, name: &str) -> usize {
    match find(list, Some(name)) {
        (Some(at), _) => at,
        (None, _) => panic!("the list has no such column"),
    }
}

/// How many columns `list` names.
pub(crate) const fn width(list: &str) -> usize {
    find(list, None).1
}

/// The place of `name` among the names of `list`, and how many names were
/// read: all of them when `name` is not there.
const fn find(list: &str, name: Option<&str>) -> (Option<usize>, usize) {
    let list = list.as_bytes();
    let (mut at, mut count) = (0, 0);
    while at < list.len() {
        if list[at] == b',' || list[at].is_ascii_whitespace() {
            at += 1;
            continue;
        }
        let start = at;
        while at < list.len() && list[at] != b',' && !list[at].is_ascii_whitespace() {
            at += 1;
        }
        if let Some(name) = name {
            let name = name.as_bytes();
            let mut same = at - start == name.len();
            let mut i = 0;
            while same && i < name.len() {
                same = list[start + i] == name[i];
                i += 1;
            }
            if same {
                return (Some(count), count + 1);
            }
        }
        count += 1;
    }
    (None, count)
}

/// Where each column of [`ROW_COLUMNS`] is.
pub(crate) mod at {
    use super::{column, ROW_COLUMNS};

    pub(crate) const ID: usize = column(ROW_COLUMNS, "id");
    pub(crate) const PARENT_ID: usize = column(ROW_COLUMNS, "parent_id");
    pub(crate) const NAME: usize = column(ROW_COLUMNS, "name");
    pub(crate) const KIND: usize = column(ROW_COLUMNS, "kind");
    pub(crate) const SIZE: usize = column(ROW_COLUMNS, "size");
    pub(crate) const MTIME: usize = column(ROW_COLUMNS, "mtime");
    pub(crate) const ETAG: usize = column(ROW_COLUMNS, "etag");
    pub(crate) const CTAG: usize = column(ROW_COLUMNS, "ctag");
    pub(crate) const QUICKXOR: usize = column(ROW_COLUMNS, "quickxor");
    pub(crate) const MIME: usize = column(ROW_COLUMNS, "mime");
    pub(crate) const PLACEMENT: usize = column(ROW_COLUMNS, "placement");
}

/// One item's place, as [`chains_sql`](crate::source::chains_sql) finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    pub id: String,
    pub rel: PathBuf,
    /// Every folder above it (below the root) is placed.
    pub above: bool,
    /// It is placed itself.
    pub own: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Items,
    Staging,
}

impl Table {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Table::Items => "items",
            Table::Staging => "staging",
        }
    }
}

/// What a cycle builds its new tree from
/// ([`TreeStore::begin_staging`](crate::TreeStore::begin_staging)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewTree {
    /// A delta's: only what it changes is staged, laid over `items`.
    Delta,
    /// A full listing's: staged whole, from nothing.
    Whole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Folder,
}

impl Kind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Kind::File => FILE,
            Kind::Folder => FOLDER,
        }
    }

    /// What a stored kind says. Any word but a folder's reads as a file
    /// (`docs/limitations/D36.md`).
    fn decode(value: &str) -> Self {
        if value == FOLDER {
            Kind::Folder
        } else {
            Kind::File
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
    pub(crate) fn encode(self) -> String {
        match self {
            Placement::Placed => PLACED.into(),
            Placement::Skipped(reason) => format!("{SKIPPED}{}", reason.as_str()),
        }
    }

    /// What a stored placement says. Anything that does not begin as a
    /// skipped one reads as placed — a word that cannot be read never
    /// takes an object out of the folder — and a skip with a reason nobody
    /// knows as [`SkipReason::Unsupported`]. SQL reads it the same way
    /// ([`placed`]).
    pub(crate) fn decode(value: &str) -> Self {
        match value.strip_prefix(SKIPPED) {
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
    /// either, which [`TreeStore::locate`](crate::TreeStore::locate) works out.
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

/// Whether an item id can be a name in a directory: the materializer keeps a
/// misplaced item in the holding directory under its id, so an id
/// that is empty, `.`, `..`, or holds a `/` or a NUL could name something else.
pub fn usable_id(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains('/') && !id.contains('\0')
}

/// A [`Row`] read from a query whose first columns are [`ROW_COLUMNS`]:
/// a row of `items`, of `staging`, or of `deferred`, which keeps the same
/// columns under the same names.
pub(crate) fn row_from(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    let kind: String = row.get(at::KIND)?;
    let placement: String = row.get(at::PLACEMENT)?;
    Ok(Row {
        id: row.get(at::ID)?,
        parent_id: row.get(at::PARENT_ID)?,
        name: row.get(at::NAME)?,
        kind: Kind::decode(&kind),
        size: row.get::<_, i64>(at::SIZE)? as u64,
        mtime: row.get(at::MTIME)?,
        etag: row.get(at::ETAG)?,
        ctag: row.get(at::CTAG)?,
        quickxor: row.get(at::QUICKXOR)?,
        mime: row.get(at::MIME)?,
        placement: Placement::decode(&placement),
    })
}

/// `row` written into `table`: a new row, or every column of
/// [`ROW_COLUMNS`] of the row that has its id. The local columns of a row
/// that is there stay.
pub(crate) fn upsert(tx: &rusqlite::Transaction<'_>, table: Table, row: &Row) -> rusqlite::Result<usize> {
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

#[cfg(test)]
mod tests;
