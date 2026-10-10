//! What the check of the backing directory finds at one path ([`DiskFinding`]), and the
//! decision that finds it ([`decide_finding`]): a function of what the rows say of the path
//! ([`PathRow`]) and of what the backing directory has there ([`PathEntry`]), with no disk
//! and no store in it.
//!
//! The decision is asked once for each entry the backing directory holds, with the row the
//! entry is taken as when that row has the path, and once for each row no entry is taken
//! as. A row no entry is taken as whose place the folder cannot keep is decided by a second
//! function of the row alone ([`decide_finding_without_place`]): nothing of the backing
//! directory is asked there. Which item an entry is, is settled before it is asked
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
    /// An item that is gone from the disk and is removed here: one OneDrive never had,
    /// or a file of OneDrive whose copy on disk the person had before them.
    Removed,
    /// An item of OneDrive with nothing of its own on disk whose place in the folder is
    /// gone: another entry has its name, or the row of its directory goes. Nobody removed
    /// it: the folder does not show it any more, and OneDrive has it.
    PlaceLost,
    /// A clean downloaded copy that is gone: its content is not on disk, and nothing is
    /// removed.
    ContentNoLongerOnDisk,
    /// The whole copy of a file whose row says its content is not on disk: a download that
    /// landed before its row was written.
    CopyLandedBeforeRow,
}

impl DiskFinding {
    /// Every finding, in the order they are declared.
    pub(in crate::fuse) const ALL: [DiskFinding; 11] = [
        DiskFinding::NothingChanged,
        DiskFinding::Made,
        DiskFinding::MadeWithStaleMark,
        DiskFinding::MarkedWithoutRow,
        DiskFinding::Moved,
        DiskFinding::MovedOverAnotherItem,
        DiskFinding::ContentChanged,
        DiskFinding::Removed,
        DiskFinding::PlaceLost,
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

/// Where the local content of a file came from, as its row says it: a copy of what OneDrive
/// has, or bytes that are here alone. A copy of OneDrive's is the content the two sides
/// agreed on, and one the rows took from the cTag mark of a copy, which has nothing agreed
/// on yet. Every other content is here alone: one a program changed here, one that was
/// hashed here and is not agreed on, and that of a file with no binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContentOrigin {
    FromOneDrive,
    ChangedHere,
}

/// What the rows of items say of a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PathRow {
    /// No row has the path, or the one that had it is found at another.
    Nothing,
    /// The row that has the path is another item's than the entry there: the entry is of
    /// the other kind, or it is an item moved here.
    AnotherItemsRow,
    /// The row of a file.
    File { binding: ItemBinding, content_presence: ContentPresence, origin: ContentOrigin },
    /// The row of a directory.
    Directory { binding: ItemBinding },
}

/// Whose the OneDrive item id is that an entry of the backing directory is marked with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MarkOwner {
    /// The entry is marked with no id.
    Unmarked,
    /// The row at the entry's path has the id: the entry is where its row says.
    RowAtPath,
    /// A row at another path has it, where nothing of that item is: the entry is that item,
    /// moved.
    RowElsewhere,
    /// It is the id of another item than the entry is: of a row another entry is taken as,
    /// of a row of the other kind, of a file whose content was not on disk to be moved, or
    /// of the entry that was bound to an id no row has. The entry is a copy made by hand,
    /// which keeps the marks.
    AnotherItem,
    /// No row the folder shows has it.
    NoRow,
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
    AnotherItemsEntry,
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
    /// Nothing of its item is where its row says, and part of the backing directory was not
    /// read: the item may be there. Or it carries the mark of a row whose own place could
    /// not be looked at, and may be a copy of it. The row keeps its local version, the entry
    /// its marks.
    MayBeUnread,
    /// Rows of two drives have the OneDrive item id it is marked with, or its row has: a
    /// mark names an item id and no drive, so which of the rows an entry elsewhere is cannot
    /// be said. The entry keeps its marks and the rows are as they were.
    ItemIdInTwoDrives,
}

/// What of a file on disk tells its content from another.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    use ContentOrigin::{ChangedHere, FromOneDrive};
    use ContentComparison::{Different, Same};
    use ContentPresence::{NotOnDisk, OnDisk};
    use CopyWholeness::{Partial, Whole};
    use ItemBinding::{Bound, Unbound};
    use MarkOwner::{AnotherItem, RowElsewhere, RowAtPath, NoRow, Unmarked};

    // An entry of the other kind than the row is never the row's item.
    let row = match (row, entry) {
        (PathRow::File { .. }, PathEntry::Directory { .. }) | (PathRow::Directory { .. }, PathEntry::File { .. }) => PathRow::AnotherItemsRow,
        _ => row,
    };
    Ok(match (row, entry) {
        // No row has the path, or the entry is not the item of the row that has it: what
        // the entry is, its mark says. A row it stands in the place of is decided by itself.
        (PathRow::Nothing | PathRow::AnotherItemsRow, PathEntry::Nothing | PathEntry::AnotherItemsEntry) => DiskFinding::NothingChanged,
        (PathRow::Nothing | PathRow::AnotherItemsRow, PathEntry::File { mark: NoRow, wholeness: Partial, .. }) => return Err(UnresolvedReason::PartialCopy),
        (PathRow::Nothing, PathEntry::File { mark, .. } | PathEntry::Directory { mark }) => match mark {
            // The id of the row at this path cannot be said of a path no row has.
            Unmarked | RowAtPath => DiskFinding::Made,
            RowElsewhere => DiskFinding::Moved,
            AnotherItem => DiskFinding::MadeWithStaleMark,
            NoRow => DiskFinding::MarkedWithoutRow,
        },
        (PathRow::AnotherItemsRow, PathEntry::File { mark, .. } | PathEntry::Directory { mark }) => match mark {
            Unmarked | RowAtPath => DiskFinding::Made,
            RowElsewhere => DiskFinding::MovedOverAnotherItem,
            AnotherItem => DiskFinding::MadeWithStaleMark,
            NoRow => DiskFinding::MarkedWithoutRow,
        },

        // A row no entry is taken as, whose name another item has on disk. What the person
        // could not see is not removed by that.
        //
        // An item OneDrive never had goes: nothing of it is anywhere else.
        (PathRow::File { binding: Unbound, .. } | PathRow::Directory { binding: Unbound }, PathEntry::AnotherItemsEntry) => DiskFinding::Removed,
        // An item of OneDrive with nothing of its own on disk: a file whose content is not
        // on disk, and a directory, which no entry is taken as. It loses its place.
        (PathRow::File { binding: Bound, content_presence: NotOnDisk, .. } | PathRow::Directory { binding: Bound }, PathEntry::AnotherItemsEntry) => DiskFinding::PlaceLost,
        // A file of OneDrive whose copy was on disk, changed here or clean: the person put
        // that entry over a file they had before them. Removed, as a rename over it through
        // the mount removes it.
        (PathRow::File { binding: Bound, content_presence: OnDisk, .. }, PathEntry::AnotherItemsEntry) => DiskFinding::Removed,

        // A row, and an item moved to its path.
        (PathRow::File { .. }, PathEntry::File { mark: RowElsewhere, .. }) | (PathRow::Directory { .. }, PathEntry::Directory { mark: RowElsewhere }) => DiskFinding::MovedOverAnotherItem,

        // A file's row, and nothing of the file.
        (PathRow::File { content_presence: NotOnDisk, .. }, PathEntry::Nothing) => DiskFinding::NothingChanged,
        (PathRow::File { content_presence: OnDisk, origin: FromOneDrive, .. }, PathEntry::Nothing) => DiskFinding::ContentNoLongerOnDisk,
        (PathRow::File { content_presence: OnDisk, origin: ChangedHere, .. }, PathEntry::Nothing) => DiskFinding::Removed,

        // A file's row, and a file that is its item.
        //
        // A row with no binding whose file is marked with an id no row has: a daemon died
        // between marking an uploaded file and its row. The row is bound and keeps the
        // content it has, changed here: it may have gone on past the bytes the mark was
        // written for, and the worst of it is one more send. A marked file no row knows at
        // all is another case on purpose: its row is made with the content its cTag mark
        // tells, since nothing says the file was changed since.
        (PathRow::File { binding: Unbound, .. }, PathEntry::File { mark: NoRow, .. }) => DiskFinding::MarkedWithoutRow,
        // Another item's bytes laid over it.
        (PathRow::File { .. }, PathEntry::File { mark: AnotherItem | NoRow, .. }) => DiskFinding::ContentChanged,
        // Marked with the row's id and not whole: it holds nothing of the file, unless a
        // writer began to change it.
        (PathRow::File { content_presence: NotOnDisk, origin: FromOneDrive, .. }, PathEntry::File { mark: RowAtPath, wholeness: Partial, .. }) => DiskFinding::NothingChanged,
        (PathRow::File { content_presence: OnDisk, origin: FromOneDrive, .. }, PathEntry::File { mark: RowAtPath, wholeness: Partial, .. }) => DiskFinding::ContentNoLongerOnDisk,
        (PathRow::File { content_presence: NotOnDisk, origin: ChangedHere, .. }, PathEntry::File { mark: RowAtPath, wholeness: Partial, .. }) => DiskFinding::ContentChanged,
        // The whole copy of a file whose content is not on disk; any other file there is
        // content the user put there.
        (PathRow::File { content_presence: NotOnDisk, .. }, PathEntry::File { mark: RowAtPath, wholeness: Whole, comparison: Same }) => DiskFinding::CopyLandedBeforeRow,
        (PathRow::File { content_presence: NotOnDisk, .. }, PathEntry::File { mark: Unmarked | RowAtPath, .. }) => DiskFinding::ContentChanged,
        // An unmarked file where the copy of what OneDrive has was: a copy that a download
        // landed is marked, so this is content the user put there, whatever its size and
        // time. (It gets the id of the item as its mark, and no more: a file marked so and
        // not whole would else be read as holding nothing of a content that is OneDrive's.)
        (PathRow::File { content_presence: OnDisk, origin: FromOneDrive, .. }, PathEntry::File { mark: Unmarked, .. }) => DiskFinding::ContentChanged,
        // A copy on disk, told by its size, its time and its cTag mark.
        (PathRow::File { content_presence: OnDisk, .. }, PathEntry::File { mark: Unmarked | RowAtPath, comparison: Same, .. }) => DiskFinding::NothingChanged,
        (PathRow::File { content_presence: OnDisk, .. }, PathEntry::File { mark: Unmarked | RowAtPath, comparison: Different, .. }) => DiskFinding::ContentChanged,

        // A directory's row.
        (PathRow::Directory { binding: Bound }, PathEntry::Nothing) => DiskFinding::NothingChanged,
        (PathRow::Directory { binding: Unbound }, PathEntry::Nothing) => DiskFinding::Removed,
        (PathRow::Directory { binding: Unbound }, PathEntry::Directory { mark: NoRow }) => DiskFinding::MarkedWithoutRow,
        (PathRow::Directory { .. }, PathEntry::Directory { mark: Unmarked | RowAtPath | AnotherItem | NoRow }) => DiskFinding::NothingChanged,

        // The other kind was made another item's above.
        (PathRow::File { .. }, PathEntry::Directory { .. }) | (PathRow::Directory { .. }, PathEntry::File { .. }) => DiskFinding::NothingChanged,
    })
}

/// What is found of a row no entry is taken as, whose place the folder cannot keep: the row
/// of the directory it is in leaves the folder, or its name is one the check sets an item
/// aside to, which no entry has. What the person could not see is not removed by that.
pub(super) fn decide_finding_without_place(row: PathRow) -> DiskFinding {
    use ContentOrigin::{ChangedHere, FromOneDrive};
    use ContentPresence::{NotOnDisk, OnDisk};
    use ItemBinding::{Bound, Unbound};

    match row {
        PathRow::Nothing | PathRow::AnotherItemsRow => DiskFinding::NothingChanged,
        // An item OneDrive never had goes: nothing of it is anywhere else.
        PathRow::File { binding: Unbound, .. } | PathRow::Directory { binding: Unbound } => DiskFinding::Removed,
        // An item of OneDrive with nothing of its own on disk loses its place.
        PathRow::File { binding: Bound, content_presence: NotOnDisk, .. } | PathRow::Directory { binding: Bound } => DiskFinding::PlaceLost,
        // A file of OneDrive changed here, whose copy was on disk: the person had it before
        // them, and it is gone.
        PathRow::File { binding: Bound, content_presence: OnDisk, origin: ChangedHere } => DiskFinding::Removed,
        // A clean copy that was on disk is gone as a clean copy that is gone anywhere: not
        // a removal, and it cannot stay.
        PathRow::File { binding: Bound, content_presence: OnDisk, origin: FromOneDrive } => DiskFinding::PlaceLost,
    }
}

#[cfg(test)]
mod tests;
