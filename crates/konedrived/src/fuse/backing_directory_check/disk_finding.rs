//! What the check of the backing directory finds at one path ([`DiskFinding`]), and the
//! decision that finds it ([`decide_finding`]): a function of what the rows say of the path
//! ([`PathRow`]) and of what the backing directory has there ([`PathEntry`]), with no disk
//! and no store in it.
//!
//! The decision is asked once for each entry the backing directory holds, with the row the
//! entry is taken as when that row has the path, and once for each row no entry is taken
//! as. Which item an entry is, is settled before it is asked
//! ([`entry_identities`](super::entry_identities)) and comes to it as the entry's
//! [`MarkOwner`]. An entry that turns out to be a row's own by the finding — a file moved
//! here, or bound here — is asked once more as that row's, for its content alone.

use konedrive_tree::{ContentPresence, ContentVersion};

/// What the check found at a path, and so what it writes to the rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(in crate::fuse) enum DiskFinding {
    /// The rows say what the disk has.
    NothingChanged,
    /// A file or directory no row knows: a new item.
    Made,
    /// A new item that carries another item's marks: a copy made by hand keeps them.
    MadeWithStaleMark,
    /// A file or directory marked with a OneDrive item id that no row the folder shows has:
    /// a store that was lost, or a daemon that died between marking an uploaded file and
    /// its row. A new item bound to that id, or the row that is there, bound to it.
    MarkedWithoutRow,
    /// An item whose row is at another path, where nothing of it is: renamed or moved by
    /// hand.
    Moved,
    /// The same, to a path another item's row has: that item is removed here.
    MovedOverAnotherItem,
    /// A file whose content is not the one its row has.
    ContentChanged,
    /// An item that is gone from the disk and is removed here.
    Removed,
    /// A clean downloaded copy that is gone: its content is not on disk, and nothing is
    /// removed.
    ContentNoLongerOnDisk,
    /// The whole copy of a file whose row says its content is not on disk: a download that
    /// landed before its row was written.
    CopyLandedBeforeRow,
}

impl DiskFinding {
    /// Every finding, in the order they are declared.
    pub(in crate::fuse) const ALL: [DiskFinding; 10] = [
        DiskFinding::NothingChanged,
        DiskFinding::Made,
        DiskFinding::MadeWithStaleMark,
        DiskFinding::MarkedWithoutRow,
        DiskFinding::Moved,
        DiskFinding::MovedOverAnotherItem,
        DiskFinding::ContentChanged,
        DiskFinding::Removed,
        DiskFinding::ContentNoLongerOnDisk,
        DiskFinding::CopyLandedBeforeRow,
    ];
}

/// Whether an item has a binding: OneDrive has it, or never had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ItemBinding {
    Bound,
    Unbound,
}

/// Whether a file's local content is its synced content: a clean copy, or one changed here
/// and not sent. A file with no binding has nothing synced and is changed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContentAgreement {
    Synced,
    ChangedHere,
}

/// What the rows of items say of a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PathRow {
    /// No row has the path, or the one that had it is found at another.
    Nothing,
    /// A row has the path, and the entry there is not that row's item: it is of the other
    /// kind, or it is an item moved here.
    AnotherItem,
    /// The row of a file.
    File { binding: ItemBinding, content_presence: ContentPresence, agreement: ContentAgreement },
    /// The row of a directory.
    Directory { binding: ItemBinding },
}

/// Whose the OneDrive item id is that an entry of the backing directory is marked with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MarkOwner {
    /// The entry is marked with no id.
    Unmarked,
    /// The id of the row at the entry's path.
    SameItem,
    /// The id of an item whose row is at another path, where nothing of it is: the entry is
    /// that item.
    MovedItem,
    /// The id of an item that is where its row says, or that another entry is taken as, or
    /// that is of the other kind: the entry is a copy made by hand, which keeps the marks.
    CopiedItem,
    /// An id no row the folder shows has.
    UnknownItem,
}

/// Whether a file that is marked with a OneDrive item id is marked whole: the whole content
/// of that file, or a file that holds nothing of it yet. An unmarked file is whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CopyWholeness {
    Whole,
    Partial,
}

/// Whether a file on disk has the content its row has, as far as its size, its time and its
/// cTag mark tell ([`compare_content`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContentComparison {
    Same,
    Different,
}

/// What the backing directory has at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PathEntry {
    Nothing,
    /// An entry that is taken as another item than the row the path has.
    AnotherItem,
    File { mark: MarkOwner, wholeness: CopyWholeness, comparison: ContentComparison },
    Directory { mark: MarkOwner },
}

/// Why the check left a path as it found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fuse) enum UnresolvedReason {
    /// A file marked with a OneDrive item id no row has, and not marked whole: it holds
    /// nothing of that file, and no row can say so before OneDrive's listing brings the
    /// item.
    PartialCopy,
    /// Neither a regular file nor a directory: the folder shows no such thing.
    NotFileOrDirectory,
    /// Deeper than the rows go.
    TooDeep,
    /// The filesystem refused to read it.
    Unreadable(String),
    /// The store refused to write it.
    Refused(String),
    /// What it waits for could not be written: the directory it is in, or the row that has
    /// its name.
    Blocked,
}

/// What of a file on disk tells its content from another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DiskContent {
    pub(super) size: u64,
    /// Whole Unix seconds, and the nanoseconds beyond them.
    pub(super) mtime: i64,
    pub(super) mtime_nsec: u32,
    /// The cTag the file is marked with, when it is.
    pub(super) ctag: Option<String>,
}

/// Whether the file on disk has the content `row` says: its size and its time in whole
/// seconds are the row's, and its cTag mark is the row's cTag where both have one. A row
/// whose content was changed here has no cTag, and the mark, which is of the copy the change
/// began from, is not asked.
pub(super) fn compare_content(row: &ContentVersion, file: &DiskContent) -> ContentComparison {
    let row_ctag = match row {
        ContentVersion::Hashed { ctag, .. } => ctag.as_deref(),
        ContentVersion::ChangedHere { .. } => None,
    };
    let is_marked_as_the_rows = match (row_ctag, file.ctag.as_deref()) {
        (Some(of_row), Some(mark)) => of_row == mark,
        _ => true,
    };
    if row.size() == file.size && row.mtime().0 == file.mtime && is_marked_as_the_rows {
        ContentComparison::Same
    } else {
        ContentComparison::Different
    }
}

/// The one decision of the check: what is found at a path of which the rows say `row` and
/// where the backing directory has `entry`.
pub(super) fn decide_finding(row: PathRow, entry: PathEntry) -> Result<DiskFinding, UnresolvedReason> {
    use ContentAgreement::{ChangedHere, Synced};
    use ContentComparison::{Different, Same};
    use ContentPresence::{NotOnDisk, OnDisk};
    use CopyWholeness::{Partial, Whole};
    use ItemBinding::{Bound, Unbound};
    use MarkOwner::{CopiedItem, MovedItem, SameItem, UnknownItem, Unmarked};

    // An entry of the other kind than the row is never the row's item.
    let row = match (row, entry) {
        (PathRow::File { .. }, PathEntry::Directory { .. }) | (PathRow::Directory { .. }, PathEntry::File { .. }) => PathRow::AnotherItem,
        _ => row,
    };
    Ok(match (row, entry) {
        // No row has the path, or the entry is not the item of the row that has it: what
        // the entry is, its mark says. A row it stands in the place of is decided by itself.
        (PathRow::Nothing | PathRow::AnotherItem, PathEntry::Nothing | PathEntry::AnotherItem) => DiskFinding::NothingChanged,
        (PathRow::Nothing | PathRow::AnotherItem, PathEntry::File { mark: UnknownItem, wholeness: Partial, .. }) => return Err(UnresolvedReason::PartialCopy),
        (PathRow::Nothing, PathEntry::File { mark, .. } | PathEntry::Directory { mark }) => match mark {
            // The id of the row at this path cannot be said of a path no row has.
            Unmarked | SameItem => DiskFinding::Made,
            MovedItem => DiskFinding::Moved,
            CopiedItem => DiskFinding::MadeWithStaleMark,
            UnknownItem => DiskFinding::MarkedWithoutRow,
        },
        (PathRow::AnotherItem, PathEntry::File { mark, .. } | PathEntry::Directory { mark }) => match mark {
            Unmarked | SameItem => DiskFinding::Made,
            MovedItem => DiskFinding::MovedOverAnotherItem,
            CopiedItem => DiskFinding::MadeWithStaleMark,
            UnknownItem => DiskFinding::MarkedWithoutRow,
        },

        // A row no entry is taken as, whose name another item has on disk.
        (PathRow::File { .. } | PathRow::Directory { .. }, PathEntry::AnotherItem) => DiskFinding::Removed,

        // A row, and an item moved to its path.
        (PathRow::File { .. }, PathEntry::File { mark: MovedItem, .. }) | (PathRow::Directory { .. }, PathEntry::Directory { mark: MovedItem }) => DiskFinding::MovedOverAnotherItem,

        // A file's row, and nothing of the file.
        (PathRow::File { content_presence: NotOnDisk, .. }, PathEntry::Nothing) => DiskFinding::NothingChanged,
        (PathRow::File { content_presence: OnDisk, agreement: Synced, .. }, PathEntry::Nothing) => DiskFinding::ContentNoLongerOnDisk,
        (PathRow::File { content_presence: OnDisk, agreement: ChangedHere, .. }, PathEntry::Nothing) => DiskFinding::Removed,

        // A file's row, and a file that is its item.
        (PathRow::File { binding: Unbound, .. }, PathEntry::File { mark: UnknownItem, .. }) => DiskFinding::MarkedWithoutRow,
        // Another item's bytes laid over it.
        (PathRow::File { .. }, PathEntry::File { mark: CopiedItem | UnknownItem, .. }) => DiskFinding::ContentChanged,
        // Marked with the row's id and not whole: it holds nothing of the file, unless a
        // writer began to change it.
        (PathRow::File { content_presence: NotOnDisk, agreement: Synced, .. }, PathEntry::File { mark: SameItem, wholeness: Partial, .. }) => DiskFinding::NothingChanged,
        (PathRow::File { content_presence: OnDisk, agreement: Synced, .. }, PathEntry::File { mark: SameItem, wholeness: Partial, .. }) => DiskFinding::ContentNoLongerOnDisk,
        (PathRow::File { content_presence: NotOnDisk, agreement: ChangedHere, .. }, PathEntry::File { mark: SameItem, wholeness: Partial, .. }) => DiskFinding::ContentChanged,
        // The whole copy of a file whose content is not on disk; any other file there is
        // content the user put there.
        (PathRow::File { content_presence: NotOnDisk, .. }, PathEntry::File { mark: SameItem, wholeness: Whole, comparison: Same }) => DiskFinding::CopyLandedBeforeRow,
        (PathRow::File { content_presence: NotOnDisk, .. }, PathEntry::File { mark: Unmarked | SameItem, .. }) => DiskFinding::ContentChanged,
        // A copy on disk, told by its size, its time and its cTag mark.
        (PathRow::File { content_presence: OnDisk, .. }, PathEntry::File { mark: Unmarked | SameItem, comparison: Same, .. }) => DiskFinding::NothingChanged,
        (PathRow::File { content_presence: OnDisk, .. }, PathEntry::File { mark: Unmarked | SameItem, comparison: Different, .. }) => DiskFinding::ContentChanged,

        // A directory's row.
        (PathRow::Directory { binding: Bound }, PathEntry::Nothing) => DiskFinding::NothingChanged,
        (PathRow::Directory { binding: Unbound }, PathEntry::Nothing) => DiskFinding::Removed,
        (PathRow::Directory { binding: Unbound }, PathEntry::Directory { mark: UnknownItem }) => DiskFinding::MarkedWithoutRow,
        (PathRow::Directory { .. }, PathEntry::Directory { mark: Unmarked | SameItem | CopiedItem | UnknownItem }) => DiskFinding::NothingChanged,

        // The other kind was made another item's above.
        (PathRow::File { .. }, PathEntry::Directory { .. }) | (PathRow::Directory { .. }, PathEntry::File { .. }) => DiskFinding::NothingChanged,
    })
}

#[cfg(test)]
mod tests;
