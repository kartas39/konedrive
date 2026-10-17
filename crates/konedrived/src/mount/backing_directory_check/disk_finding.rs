//! What the check of the backing directory finds at one place ([`DiskFinding`]), and the
//! decision that finds it ([`decide_finding`]): a function of what the rows say of the place
//! ([`PlaceRow`]) and of what the backing directory has there ([`PlaceEntry`]), with no disk
//! and no store in it. It is the table of `docs/design/mount.md` §7.2, a row of the table an arm.
//!
//! A file's content is compared by its size, its time and its cTag mark, with one
//! exception: a row that still keeps the content of before a write ([`WriterAtStop::Open`])
//! is of a file a program held open when the daemon stopped, and is changed here whatever
//! the comparison says. A file opened for writing and never written is so sent once more.
//!
//! The decision looks at one place. Nothing of another place enters it: not where an entry
//! of the same mark is, not whether a row elsewhere misses its entry. Of a mark it is told
//! one thing ([`EntryMark`]): whether the id is the one of the row at this place, of no row
//! at all, or of some row.
//!
//! An entry of the other kind than the row is never the row's item: the row is decided as
//! one with nothing at its place, and the entry is asked again as one no row has. A row
//! whose directory's row leaves the folder has no place to keep, and is decided by the row
//! alone ([`decide_finding_without_place`]).

use konedrive_tree::{ContentPresence, ContentVersion};

use super::mark_change::EntryLinks;

/// What the check found at a place, and so what it writes to the rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(in crate::mount) enum DiskFinding {
    /// The rows say what the disk has.
    NothingChanged,
    /// A file or directory no row has at its place: a new item.
    Made,
    /// A file or directory no row has at its place, marked with a OneDrive item id that no
    /// row has: a new item bound to that id. A store that was lost.
    MarkedWithoutRow,
    /// A file whose content is not the one its row has.
    ContentChanged,
    /// The whole copy of a file whose row says its content is not on disk: a download that
    /// landed before its row was written.
    CopyLandedBeforeRow,
    /// An item of OneDrive that the folder does not show any more: nothing of it is where
    /// its row says, or an entry of the other kind is. Its local version and what was
    /// agreed on are taken away; its binding and what OneDrive has stay. Nothing is removed
    /// in OneDrive.
    NotShownAnyMore,
    /// An item OneDrive never had, with nothing where its row says: the row goes.
    Removed,
}

impl DiskFinding {
    /// Every finding, in the order they are declared.
    pub(in crate::mount) const ALL: [DiskFinding; 7] = [
        DiskFinding::NothingChanged,
        DiskFinding::Made,
        DiskFinding::MarkedWithoutRow,
        DiskFinding::ContentChanged,
        DiskFinding::CopyLandedBeforeRow,
        DiskFinding::NotShownAnyMore,
        DiskFinding::Removed,
    ];
}

/// Whether an item has a binding: OneDrive has it, or never had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ItemBinding {
    Bound,
    Unbound,
}

/// Whether a writer held a file open when the daemon stopped: the row still keeps the
/// content of before the write, which the last close takes away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriterAtStop {
    /// No program had it open for writing, or its last close was written.
    Closed,
    /// A program had it open for writing and no close was written: bytes may have gone
    /// into the file that neither its size nor its time shows.
    Open,
}

/// What the rows of items say of a place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlaceRow {
    /// The folder shows nothing there.
    Nothing,
    File { binding: ItemBinding, content_presence: ContentPresence, writer: WriterAtStop },
    Directory { binding: ItemBinding },
}

/// Whose the OneDrive item id is that an entry of the backing directory is marked with, as
/// far as the place itself tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryMark {
    /// The entry is marked with no id.
    Unmarked,
    /// The id of the row at the entry's own place.
    OfRowAtPlace,
    /// An id no row has, shown by the folder or not, of whichever drive, that this entry
    /// alone carries.
    OfNoRow,
    /// An id no row has that more than one entry carries: a second name of the file, a
    /// copy made with its marks. Which of them is the item is not known, and none is.
    OfSeveralEntries,
    /// An id some row has that is not the row at this place. Which row, and where, is not
    /// asked.
    OfAnotherRow,
}

/// Whether a file is marked as the whole content of the item its id mark names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CopyWholeness {
    Whole,
    NotWhole,
}

/// Whether a file on disk has the content its row has, as far as its size, its time and its
/// cTag mark tell ([`compare_content`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContentComparison {
    Same,
    Different,
}

/// What the backing directory has at a place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlaceEntry {
    Nothing,
    /// `comparison` is with the content of the file's row at this place; it says nothing
    /// where no such row is.
    File { mark: EntryMark, wholeness: CopyWholeness, comparison: ContentComparison },
    Directory { mark: EntryMark },
}

/// Why the check left a path as it found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::mount) enum UnresolvedReason {
    /// Neither a regular file nor a directory: the folder shows no such thing.
    NotFileOrDirectory,
    /// Deeper than the rows go.
    TooDeep,
    /// The filesystem refused to read it, or to change it.
    Unreadable(String),
    /// The store refused to write it.
    Refused(String),
    /// What it waits for could not be written: the row that has its name, or a row below
    /// it.
    Blocked,
    /// A change of the daemon's own that a death cut short names it, and could be neither
    /// finished nor undone: its record stands.
    PlaceChangeUnsettled,
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
///
/// The cTag mark of a file that has several names is not asked either: the mark is the
/// file's, written through whichever name was last sent or downloaded, and says nothing of
/// the item of another name.
pub(super) fn compare_content(row: &ContentVersion, file: &DiskContent, links: EntryLinks) -> ContentComparison {

    let row_ctag = match row {
        ContentVersion::Hashed { ctag, .. } => ctag.as_deref(),
        ContentVersion::ChangedHere { .. } => None,
    };
    let is_marked_as_the_rows = match (links, row_ctag, file.ctag.as_deref()) {
        (EntryLinks::One, Some(of_row), Some(mark)) => of_row == mark,
        _ => true,
    };
    if row.size() == file.size && row.mtime().0 == file.mtime && is_marked_as_the_rows {
        ContentComparison::Same
    } else {
        ContentComparison::Different
    }
}

/// The one decision of the check: what is found at a place of which the rows say `row` and
/// where the backing directory has `entry`.
pub(super) fn decide_finding(row: PlaceRow, entry: PlaceEntry) -> DiskFinding {
    use ContentComparison::{Different, Same};
    use ContentPresence::{NotOnDisk, OnDisk};
    use ItemBinding::{Bound, Unbound};

    match (row, entry) {
        // Row 1: an entry of the row's kind is that item. A file's content is compared.
        (PlaceRow::Directory { .. }, PlaceEntry::Directory { .. }) => DiskFinding::NothingChanged,
        // A file a writer held open when the daemon stopped is changed here whatever its
        // size and time say: bytes may have been written that neither shows.
        (PlaceRow::File { writer: WriterAtStop::Open, .. }, PlaceEntry::File { .. }) => DiskFinding::ContentChanged,
        (PlaceRow::File { content_presence: OnDisk, .. }, PlaceEntry::File { comparison: Same, .. }) => DiskFinding::NothingChanged,
        (PlaceRow::File { content_presence: OnDisk, .. }, PlaceEntry::File { comparison: Different, .. }) => DiskFinding::ContentChanged,
        // Row 2: the whole copy of that item, where the row says its content is not on disk.
        (PlaceRow::File { content_presence: NotOnDisk, .. }, PlaceEntry::File { mark: EntryMark::OfRowAtPlace, wholeness: CopyWholeness::Whole, comparison: Same }) => DiskFinding::CopyLandedBeforeRow,
        // Row 3: any other file there is content put there.
        (PlaceRow::File { content_presence: NotOnDisk, .. }, PlaceEntry::File { .. }) => DiskFinding::ContentChanged,

        // Nothing where the row says nothing of the item is on disk: a file that is not
        // downloaded, a directory of OneDrive, which needs none.
        (PlaceRow::File { binding: Bound, content_presence: NotOnDisk, .. } | PlaceRow::Directory { binding: Bound }, PlaceEntry::Nothing) => DiskFinding::NothingChanged,
        // Row 4: an item OneDrive has, and nothing where its copy was, or an entry of the
        // other kind.
        (PlaceRow::File { binding: Bound, content_presence: OnDisk, .. }, PlaceEntry::Nothing)
        | (PlaceRow::File { binding: Bound, .. }, PlaceEntry::Directory { .. })
        | (PlaceRow::Directory { binding: Bound }, PlaceEntry::File { .. }) => DiskFinding::NotShownAnyMore,
        // Row 5: an item OneDrive never had, and nothing or an entry of the other kind.
        (PlaceRow::File { binding: Unbound, .. }, PlaceEntry::Nothing | PlaceEntry::Directory { .. }) | (PlaceRow::Directory { binding: Unbound }, PlaceEntry::Nothing | PlaceEntry::File { .. }) => DiskFinding::Removed,

        // Row 6: an entry no row has, marked with an id no row has.
        (PlaceRow::Nothing, PlaceEntry::File { mark: EntryMark::OfNoRow, .. } | PlaceEntry::Directory { mark: EntryMark::OfNoRow }) => DiskFinding::MarkedWithoutRow,
        // Row 7: any other entry no row has; one whose id several entries carry among them.
        (PlaceRow::Nothing, PlaceEntry::File { .. } | PlaceEntry::Directory { .. }) => DiskFinding::Made,
        (PlaceRow::Nothing, PlaceEntry::Nothing) => DiskFinding::NothingChanged,
    }
}

/// What is found of a row whose directory's row leaves the folder: it has no place to keep,
/// whatever it has on disk.
pub(super) fn decide_finding_without_place(row: PlaceRow) -> DiskFinding {
    match row {
        PlaceRow::Nothing => DiskFinding::NothingChanged,
        PlaceRow::File { binding: ItemBinding::Bound, .. } | PlaceRow::Directory { binding: ItemBinding::Bound } => DiskFinding::NotShownAnyMore,
        PlaceRow::File { binding: ItemBinding::Unbound, .. } | PlaceRow::Directory { binding: ItemBinding::Unbound } => DiskFinding::Removed,
    }
}

#[cfg(test)]
mod tests;
