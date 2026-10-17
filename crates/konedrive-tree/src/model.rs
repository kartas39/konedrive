//! What the tree holds: an item as `items` holds it — its ids, its versions, and the sync
//! state they give —, an entry of OneDrive's delta, and the words the store keeps for a
//! kind, a placement and whether a content is on disk.

use std::ffi::OsString;


/// A row's `placement` while the item is in the folder, as stored.
pub(crate) const PLACED: &str = "placed";
/// What a row's `placement` begins with while the item is not: a
/// [`SkipReason`]'s word follows.
const SKIPPED: &str = "skipped:";

/// A row's `kind`, as stored.
pub(crate) const FILE: &str = "file";
/// An item's kind in `items`, as stored: a file is [`FILE`].
const DIRECTORY: &str = "directory";
/// A row's `content_presence` in `items`, as stored.
pub(crate) const ON_DISK: &str = "on-disk";
pub(crate) const NOT_ON_DISK: &str = "not-on-disk";

/// Where `name` is in `list`, a list of column names as a `SELECT` takes
/// it. Used for constants only, so a name the list does not have fails the
/// build, not a query.
pub(crate) const fn column(list: &str, name: &str) -> usize {
    match find(list, Some(name)) {
        (Some(at), _) => at,
        (None, _) => panic!("the list has no such column"),
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Folder,
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
    /// knows as [`SkipReason::Unsupported`].
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
    /// either.
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

/// Whether OneDrive's item id is one the folder takes: an id that is empty, `.`, `..`,
/// or holds a `/` or a NUL could name something else, and the listing does not show its
/// item (`SkipReason::Unsupported`).
pub fn usable_id(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains('/') && !id.contains('\0')
}

/// The id of a drive in OneDrive: an account's identity, and half of what names an item of
/// OneDrive ([`OneDriveItemReference`]). Never empty: an account that has no drive yet has
/// none (`Option<DriveId>`).
///
/// Graph does not spell one drive's id the same in every answer, so the id is made the same
/// here, where it is made ([`new`](Self::new)): its ASCII letters in lower case, and nothing
/// else changed. Two spellings of one drive are then one `DriveId`, in a comparison, as a key
/// and in the store. Compared with a text, the text is read the same way.
///
/// Its text is taken by [`as_str`](Self::as_str) or [`into_string`](Self::into_string) and
/// in no other way: it is no `str` by itself and is not formatted into a text, so that
/// every place that takes the lowered text, which may not be the spelling Graph knows the
/// drive by, is found by those two names.
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

    /// What a stored kind says. Any word but a directory's reads as a file.
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
    /// (base64) and the cTag. Either may be missing; a copy on disk whose row was lost has
    /// the cTag it is marked with and no hash.
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

    /// How many times a program began to change the content since a hash was last known: 0
    /// for a hashed content.
    pub fn change_count(&self) -> u64 {
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
    /// What OneDrive had of the item when it was last asked; `None` when OneDrive does not
    /// have it, or the client was never told what it has. The root's has no place, and is
    /// there once the root is bound.
    pub onedrive: Option<ItemVersion>,
    /// The eTag OneDrive gave with that version: what a request to OneDrive is guarded with.
    pub onedrive_etag: Option<String>,
    /// The MIME type OneDrive gives for the content of a file it has (`file.mimeType`), as a
    /// listing said it: what tells an image or a video, for a thumbnail. `None` for a
    /// directory, for an item OneDrive does not have, and until a listing said it.
    pub onedrive_mime: Option<String>,
    /// The place the two sides last agreed on.
    pub synced_place: Option<ItemPlace>,
    /// The content the two sides last agreed on.
    pub synced_content: Option<ContentVersion>,
    pub content_presence: ContentPresence,
    /// The content a file had before its first writer opened it; held until the last writer
    /// closes.
    pub content_before_write: Option<ContentVersion>,
    /// Whether the folder can show the item at all, as OneDrive's listing said of the item
    /// itself: an item below a directory that cannot be shown cannot be shown either, which
    /// [`ItemStore::find_folder_visibility`](crate::ItemStore::find_folder_visibility) asks. `Placed` for an
    /// item OneDrive does not have.
    pub placement: Placement,
    /// The upload rules ignore the item's own name. Whether the item is left here is not
    /// stored: [`ItemStore::is_ignored`](crate::ItemStore::is_ignored) derives it.
    pub is_name_ignored: bool,
    /// The count of confirmations at which OneDrive last confirmed something of the item to
    /// a step of the client; 0 when it never did. The stamp of the guard against an outdated
    /// listing: what a listing fetched before that count says of the item may be older than
    /// what was confirmed.
    pub confirmed_at_count: u64,
    /// The owner's executable bit of a file, as a program set it through the mount (chmod,
    /// or the mode a file was made with): the folder shows the file with it. OneDrive
    /// keeps no modes, so it is this computer's alone and no part of any version: it stays
    /// with the item through a rename and a move, and no step sends it. Never set for a
    /// directory. Read with the row; written by
    /// [`ItemStore::set_executable_bit`](crate::ItemStore::set_executable_bit) alone, never
    /// by a write of the whole row.
    pub is_executable: bool,
    /// A stamp from one counter of the store, given when the item's local version first came
    /// to differ from what the two sides last agreed on — in its place or its content, or
    /// by one of the two being there and the other not — and `None` while they agree. The
    /// order in which what is unsent came to be so. The store keeps it with every write of
    /// either ([`ItemStore::list_unsent`](crate::ItemStore::list_unsent)); nothing writes
    /// it by itself. An item that is left here has it too: the stamp says what differs, and
    /// the reader of what is unsent leaves such an item out.
    pub unsent_since: Option<u64>,
}

/// Whether the folder can show an item as OneDrive has it
/// ([`ItemStore::find_folder_visibility`](crate::ItemStore::find_folder_visibility)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FolderVisibility {
    /// Its own placement says so, and that of every directory above it in OneDrive, up to
    /// the root.
    CanBeShown,
    /// Its own placement, or that of a directory above it, says no: so it stays until
    /// OneDrive says otherwise of that item or directory.
    CannotBeShown,
    /// A directory above it was never listed, or the rows above it do not end at the root:
    /// a listing that has not ended, or has not said it yet, will say.
    NotKnownYet,
    /// OneDrive does not have the item, by its row.
    NotInOneDrive,
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
    /// `is_name_ignored` is what the upload rules say of its name.
    /// `replaced` is the write operation's statement that the file is made where that file
    /// was removed here, whose removal is not sent yet: the made file takes over its
    /// binding, and OneDrive sees one item whose content changed. The store checks the
    /// statement and finds no such item by itself.
    Made { kind: ItemKind, place: ItemPlace, content: Option<ContentVersion>, is_name_ignored: bool, replaced: Option<LocalItemId> },
    /// `item` was renamed or moved to `place`, where nothing is. `is_name_ignored` is what
    /// the upload rules say of its name there.
    Moved { item: LocalItemId, place: ItemPlace, is_name_ignored: bool },
    /// `replacing` was renamed over `replaced` and has its place now, and with its name what
    /// the upload rules say of that name: a file saved over another takes over its binding.
    SavedOver { replacing: LocalItemId, replaced: LocalItemId },
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
