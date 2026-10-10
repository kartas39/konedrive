//! What the tree holds: a row of `onedrive_items` and `staging`, a delta entry, the
//! words the store keeps for a kind and a placement, and the one place a
//! row is read from SQLite and written to it. And an item as `items` holds it: its ids,
//! its versions, and the sync state they give.

use std::ffi::OsString;
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
/// An item's kind in `items`, as stored: a file is [`FILE`].
const DIRECTORY: &str = "directory";
/// A row's `content_presence` in `items`, as stored.
pub(crate) const ON_DISK: &str = "on-disk";
pub(crate) const NOT_ON_DISK: &str = "not-on-disk";

/// Every column, for copies between `onedrive_items` and `staging`: the local ones
/// (`thumb_key`, `local_seq`) travel with the row.
pub(crate) const COLUMNS: &str = "id, parent_id, name, kind, size, mtime, etag, ctag, quickxor, mime, placement, thumb_key, local_seq";
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Table {
    Items,
    Staging,
}

impl Table {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Table::Items => "onedrive_items",
            Table::Staging => "staging",
        }
    }
}

/// What a cycle builds its new tree from
/// ([`TreeStore::begin_staging`](crate::TreeStore::begin_staging)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewTree {
    /// A delta's: only what it changes is staged, laid over `onedrive_items`.
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
/// a row of `onedrive_items`, of `staging`, or of `deferred`, which keeps the same
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

/// The id of a drive in OneDrive: an account's identity, and half of what names an item of
/// OneDrive ([`OneDriveItemReference`]). Never empty: an account that has no drive yet has
/// none (`Option<DriveId>`).
///
/// Graph does not spell one drive's id the same in every answer, so the id is made the same
/// here, where it is made ([`new`](Self::new)): its ASCII letters in lower case, and nothing
/// else changed. Two spellings of one drive are then one `DriveId`, in a comparison, as a key
/// and in the store. Compared with a text, the text is read the same way.
#[derive(Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct DriveId(String);

impl DriveId {
    /// `text` as a drive id, its ASCII letters in lower case; `None` when it is empty.
    pub fn new(text: impl Into<String>) -> Option<Self> {
        let mut text = text.into();
        text.make_ascii_lowercase();
        (!text.is_empty()).then_some(Self(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    /// Whether `text` spells this drive's id.
    fn is_spelled(&self, text: &str) -> bool {
        self.0.eq_ignore_ascii_case(text)
    }
}

/// As the text it is, quoted: what `{:?}` printed while the id was a `String`, which
/// `Accounts.LastError` and the log say.
impl std::fmt::Debug for DriveId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

impl std::fmt::Display for DriveId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::ops::Deref for DriveId {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for DriveId {
    fn eq(&self, other: &str) -> bool {
        self.is_spelled(other)
    }
}

impl PartialEq<&str> for DriveId {
    fn eq(&self, other: &&str) -> bool {
        self.is_spelled(other)
    }
}

impl PartialEq<String> for DriveId {
    fn eq(&self, other: &String) -> bool {
        self.is_spelled(other)
    }
}

/// The permanent number the client gives an item: the key of its row in `items`, and the
/// kernel's node id of what the folder shows. Never reused, never changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalItemId(i64);

impl LocalItemId {
    /// The root of the folder: the number FUSE gives the root.
    pub const ROOT: Self = Self(1);

    pub fn new(number: i64) -> Self {
        Self(number)
    }

    pub fn as_i64(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for LocalItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The id OneDrive has for an item within its drive, as Graph gave it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OneDriveItemId(String);

impl OneDriveItemId {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What names one item of OneDrive, as Graph's `itemReference` does: a drive and the item's
/// id within it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OneDriveItemReference {
    pub drive_id: DriveId,
    pub item_id: OneDriveItemId,
}

/// The kind of an item, in the filesystem's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKind {
    File,
    Directory,
}

impl ItemKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ItemKind::File => FILE,
            ItemKind::Directory => DIRECTORY,
        }
    }

    /// What a stored kind says. Any word but a directory's reads as a file, as
    /// [`Kind::decode`] reads a folder's.
    pub(crate) fn decode(value: &str) -> Self {
        if value == DIRECTORY {
            ItemKind::Directory
        } else {
            ItemKind::File
        }
    }
}

/// The directory an item is in and its name there. The parent is named by the client's own
/// number on every side, so that places of the two sides can be compared. The name is kept as
/// it was given, bytes that are not UTF-8 included, and never changed to compare it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ItemPlace {
    pub parent_local_item_id: LocalItemId,
    pub name: OsString,
}

/// What tells one content of a file from another, with its size and its modification time
/// (`mtime` in whole Unix seconds, `mtime_nsec` the rest of it: 0 for a time OneDrive gave).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ContentVersion {
    /// Told by what OneDrive gave or the client computed from the bytes: the QuickXorHash
    /// (base64) and the cTag.
    Hashed { size: u64, mtime: i64, mtime_nsec: u32, hash: Option<String>, ctag: Option<String> },
    /// Changed by a program here, with no hash known yet: told by a number the client raises
    /// each time a program begins to change the content.
    ChangedHere { size: u64, mtime: i64, mtime_nsec: u32, change_count: u64 },
}

impl ContentVersion {
    pub fn size(&self) -> u64 {
        match self {
            ContentVersion::Hashed { size, .. } | ContentVersion::ChangedHere { size, .. } => *size,
        }
    }

    /// The modification time: whole Unix seconds, and the nanoseconds beyond them.
    pub fn mtime(&self) -> (i64, u32) {
        match self {
            ContentVersion::Hashed { mtime, mtime_nsec, .. } | ContentVersion::ChangedHere { mtime, mtime_nsec, .. } => (*mtime, *mtime_nsec),
        }
    }

    /// How many times a program began to change the content since a hash was last known.
    pub(crate) fn change_count(&self) -> u64 {
        match self {
            ContentVersion::Hashed { .. } => 0,
            ContentVersion::ChangedHere { change_count, .. } => *change_count,
        }
    }
}

/// What an item is on one side: its place, and for a file its content. The root of the
/// folder has no place; every other item has one. A file has a content and a directory has
/// none. What kind the item is, is the item's ([`Item::kind`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemVersion {
    pub place: Option<ItemPlace>,
    pub content: Option<ContentVersion>,
}

/// Whether the whole content of a file the folder shows is in the backing directory. A stable
/// state: a download or an upload in progress is not written into it. An item that is no file
/// of the folder is `NotOnDisk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentPresence {
    OnDisk,
    NotOnDisk,
}

impl ContentPresence {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ContentPresence::OnDisk => ON_DISK,
            ContentPresence::NotOnDisk => NOT_ON_DISK,
        }
    }

    /// What a stored presence says. Any word but `on-disk` reads as not on disk: a word that
    /// cannot be read never says content is here that may not be.
    pub(crate) fn decode(value: &str) -> Self {
        if value == ON_DISK {
            ContentPresence::OnDisk
        } else {
            ContentPresence::NotOnDisk
        }
    }
}

/// One item: a file or directory the folder shows or OneDrive has. A row of `items`.
///
/// Until the OneDrive version is in the row too, `onedrive_items` holds it, with the eTag
/// and the placement, under the binding's item id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub local_item_id: LocalItemId,
    /// The binding: which item of OneDrive this is. No other item has the same.
    pub onedrive_item: Option<OneDriveItemReference>,
    /// A file or a directory: known when the row is made, and never changed. A file replaced
    /// by a directory is another item.
    pub kind: ItemKind,
    /// What the folder shows of the item; `None` when it does not show it.
    pub local: Option<ItemVersion>,
    /// The place the two sides last agreed on.
    pub synced_place: Option<ItemPlace>,
    /// The content the two sides last agreed on.
    pub synced_content: Option<ContentVersion>,
    pub content_presence: ContentPresence,
    /// The content a file had before its first writer opened it; held until the last writer
    /// closes.
    pub content_before_write: Option<ContentVersion>,
    /// The upload rules leave it, or a directory above it, here.
    pub is_ignored: bool,
}

/// How a file the folder shows stands between OneDrive and this computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileSyncState {
    /// Made here; OneDrive has no item for it.
    LocalOnly,
    /// In OneDrive, and its content is not on disk.
    RemoteOnly,
    /// In OneDrive, and its content here is not the one the two sides last agreed on.
    ModifiedHere,
    /// In OneDrive, on disk, and the same on both sides.
    Synced,
}

impl Item {
    /// The sync state of the file the folder shows as this item: the first that holds of no
    /// binding, content not on disk, local content that is not the synced one, and synced.
    /// `None` for an item the folder does not show as a file.
    pub fn file_sync_state(&self) -> Option<FileSyncState> {
        let local = self.local.as_ref().filter(|_| self.kind == ItemKind::File)?;
        Some(if self.onedrive_item.is_none() {
            FileSyncState::LocalOnly
        } else if self.content_presence == ContentPresence::NotOnDisk {
            FileSyncState::RemoteOnly
        } else if local.content != self.synced_content {
            FileSyncState::ModifiedHere
        } else {
            FileSyncState::Synced
        })
    }
}

/// One change of an item's local version, which a write operation of the mount hands the
/// store ([`ItemStore::apply`](crate::ItemStore::apply)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalVersionChange {
    /// A file or directory was made at `place`: a file with `content`, a directory with none.
    /// A file made at the place of a file removed here whose removal is not sent yet takes
    /// over that item's binding.
    Made { kind: ItemKind, place: ItemPlace, content: Option<ContentVersion>, is_ignored: bool },
    /// `item` was renamed or moved to `place`, where nothing is. `is_ignored` is what the
    /// upload rules say of it there, for it and all below it.
    Moved { item: LocalItemId, place: ItemPlace, is_ignored: bool },
    /// `replacing` was renamed over `replaced` and has its place now: a file saved over
    /// another takes over its binding.
    SavedOver { replacing: LocalItemId, replaced: LocalItemId, is_ignored: bool },
    /// `item` was removed from the folder.
    Removed { item: LocalItemId },
    /// The first writer opened the file `item`: its content begins to change.
    OpenedForWriting { item: LocalItemId },
    /// The last writer closed the file `item`, and its content is another now, of this size
    /// and modification time.
    ClosedChanged { item: LocalItemId, size: u64, mtime: i64, mtime_nsec: u32 },
    /// The last writer closed the file `item` and nothing was changed.
    ClosedUnchanged { item: LocalItemId },
    /// The file `item` was cut or grown to `size` with no writer's close to follow.
    Truncated { item: LocalItemId, size: u64, mtime: i64, mtime_nsec: u32 },
}

#[cfg(test)]
mod tests;
