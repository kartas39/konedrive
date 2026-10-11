//! What becomes of a place change a death cut short ([`PlaceChangeSettlement`]), and the
//! decision that says it ([`decide_settlement`]): a function of what the record says the
//! daemon was about to do, of what the rows say now of the items it names, and of what the
//! disk has at the places it names ([`RecordFacts`]), with no disk and no store in it.
//!
//! The record is the daemon's own knowledge of its own change (design §8): the entry is at
//! one of the places the record names, and no mark is asked. The rules:
//!
//! - *A row that was written is not undone.* Where the rows say what the change was for —
//!   the item is at the new place, the row is gone, the content is on disk — nothing is
//!   left to do, whatever the disk has.
//! - *A made entry of a new item is finished*: the entry is there and no row has its
//!   place, so its row is made, as the operation would have made it. Nothing is removed
//!   from the disk for a change that was not finished.
//! - *A move and a set-aside are undone on disk*: the entry goes back where the row says
//!   it is. The record does not say whose change it was, a program's or a step's of the
//!   plan, and their rows differ (a set-aside is a removal for one and the taking of
//!   OneDrive's content for another); the row as it stands is right for all of them once
//!   the disk is as it was, and the plan or the person does the change again. No row of an
//!   item is moved or removed by a record.
//! - *An entry is moved back only where the row says the item has an entry on disk*: what
//!   is at the new place of an item with nothing on disk is not its entry.
//! - *A backing file that was to be made for a writer after the row was told* and is not
//!   there: the row is put back as it was before the writer's open.
//! - *A landed file* whose row still has the content that was downloaded is the copy: the
//!   row says so. One whose row has another content by now is the daemon's own stale
//!   download, and goes.
//! - *An undo that cannot be done is not tried again at every start.* The directory an
//!   entry is to be put back into is gone; or what is at the new place is not the entry
//!   that was moved (a file of another size or time: the person saved that name again by
//!   hand); or the name to put an entry back under is taken. The record goes, nothing is
//!   moved over the person's file, and the entries are left as they are for the check to
//!   decide like any other: a file with no row is a new item, shown and sent.
//! - *A place that cannot be looked at* decides nothing: the record stands, and the rows
//!   it names are left as they are. A record stands for no other reason.

/// What the disk has at a place a record names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DiskPlace {
    /// Its directory is there, and nothing under the name.
    Nothing,
    /// Its directory is not there: no entry can be put back under the name.
    NoDirectory,
    /// An entry that may be the one the record is about: of the item's kind, and for a
    /// file whose row says its content is on disk, of the size and the time in whole
    /// seconds that row says.
    OwnEntry,
    /// An entry that is not the one the record is about: of the other kind, or a file of
    /// another size or time — one the person put there since.
    AnotherEntry,
    NotLookedAt,
}

/// What the disk has at the name an entry was set aside to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AsideEntry {
    Nothing,
    Entry,
    NotLookedAt,
}

/// What the rows say of a file whose backing file was to be made after its row was told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriterFileRow {
    /// No file's row of that item is at the place.
    NotAtPlace,
    /// The row was not told: the content is not on disk.
    NotOnDisk,
    /// The row says on disk and keeps the content from before a writer's open.
    OnDiskWithContentBeforeWrite,
    /// The row says on disk and keeps no former content: a truncation.
    OnDiskWithoutContentBeforeWrite,
}

/// What the rows say of an item whose entry was to be moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MovedRow {
    /// At the place it was to be moved from, and the row says it has an entry on disk: a
    /// directory, or a file whose content is on disk.
    AtFromWithEntry,
    /// At the place it was to be moved from, a file whose content is not on disk.
    AtFromWithoutEntry,
    /// At the place it was to be moved to: the row was written.
    AtTo,
    /// Anywhere else, not shown, or gone.
    Elsewhere,
}

/// What the rows say of the item whose entry another was to be moved over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReplacedRow {
    /// Still at the place, with an entry on disk as its row says.
    AtToWithEntry,
    /// Still at the place, a file whose content is not on disk.
    AtToWithoutEntry,
    NotAtTo,
}

/// What the rows say of an item whose entry was to be set aside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AsideRow {
    /// At the place, and the row says it has an entry on disk.
    AtPlaceWithEntry,
    /// Not there any more, or a file whose row says its content is not on disk: the row of
    /// the change was written.
    Otherwise,
}

/// What the rows say of a file whose download was to land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LandedRow {
    NotAtPlace,
    /// Not on disk, its local content the one the record says was downloaded.
    NotOnDiskWithRecordedContent,
    /// Not on disk, with another content by now.
    NotOnDiskWithOtherContent,
    OnDisk,
}

/// What the disk has where a downloaded file was to be linked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LinkedEntry {
    Nothing,
    /// A file of the size and time of the content the record says was downloaded.
    FileOfRecordedContent,
    AnotherEntry,
    NotLookedAt,
}

/// A record, with what the rows and the disk say now of what it names: a case for each
/// case of a record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecordFacts {
    /// An entry was to be made for an item with no row yet.
    MadeForNewItem { is_place_shown: bool, entry: DiskPlace },
    /// The backing file of a file that has a row was to be made.
    MadeForItem { row: WriterFileRow, entry: DiskPlace },
    Moved { row: MovedRow, from: DiskPlace, to: DiskPlace },
    MovedOver { row: MovedRow, replaced: ReplacedRow, from: DiskPlace, to: DiskPlace, aside: AsideEntry },
    SetAside { row: AsideRow, place: DiskPlace, aside: AsideEntry },
    Linked { row: LandedRow, entry: LinkedEntry },
}

/// The row a change is finished with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowFinish {
    /// The row of the made entry is made.
    MadeItem,
    /// The landed file is the item's content on disk.
    ContentOnDisk,
}

/// How a change is undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChangeUndo {
    /// On disk: the moved entry back to where it was.
    MoveBack,
    /// On disk: the entry that was set aside back under its name.
    Restore,
    /// On disk: the moved entry back, then the entry it was moved over back under its name.
    MoveBackAndRestore,
    /// On disk: the linked file removed.
    RemoveLinkedFile,
    /// In the row: the content from before the writer's open, not on disk.
    ContentBeforeWrite,
}

/// What becomes of a place change that was left on record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlaceChangeSettlement {
    /// Nothing of it is left to do, or it was never begun, or its row was written: the
    /// record goes, and the places are the check's to decide like any other.
    NothingLeft,
    /// The row is written as the operation would have, and the record goes with it.
    Finish(RowFinish),
    /// It is taken back, and the record goes.
    Undo(ChangeUndo),
    /// A place it names could not be looked at: the record stands.
    LeftStanding,
}

/// What becomes of the place change of a record, of which the rows and the disk say
/// `facts`.
pub(super) fn decide_settlement(facts: RecordFacts) -> PlaceChangeSettlement {
    use ChangeUndo::{ContentBeforeWrite, MoveBack, MoveBackAndRestore, RemoveLinkedFile, Restore};
    use DiskPlace::{AnotherEntry, NoDirectory, NotLookedAt, Nothing, OwnEntry};
    use PlaceChangeSettlement::{Finish, LeftStanding, NothingLeft, Undo};


    match facts {
        // Make, of a new item. A row has the place: it was written, or another item is there.
        RecordFacts::MadeForNewItem { is_place_shown: true, .. } => NothingLeft,
        RecordFacts::MadeForNewItem { is_place_shown: false, entry } => match entry {
            OwnEntry => Finish(RowFinish::MadeItem),
            Nothing | NoDirectory | AnotherEntry => NothingLeft,
            NotLookedAt => LeftStanding,
        },

        // The backing file of a writer. The row was not told, or is not there.
        RecordFacts::MadeForItem { row: WriterFileRow::NotAtPlace | WriterFileRow::NotOnDisk, .. } => NothingLeft,
        RecordFacts::MadeForItem { row: WriterFileRow::OnDiskWithContentBeforeWrite | WriterFileRow::OnDiskWithoutContentBeforeWrite, entry: NotLookedAt } => LeftStanding,
        RecordFacts::MadeForItem { row: WriterFileRow::OnDiskWithContentBeforeWrite, entry: Nothing | NoDirectory } => Undo(ContentBeforeWrite),
        // A truncation keeps nothing to put back: the check finds the row with no file.
        RecordFacts::MadeForItem { row: WriterFileRow::OnDiskWithoutContentBeforeWrite, entry: Nothing | NoDirectory } => NothingLeft,
        RecordFacts::MadeForItem { entry: OwnEntry | AnotherEntry, .. } => NothingLeft,

        // A move. The row was written, or the item is not where it was moved from.
        RecordFacts::Moved { row: MovedRow::AtTo | MovedRow::Elsewhere, .. } => NothingLeft,
        RecordFacts::Moved { from: NotLookedAt, .. } | RecordFacts::Moved { to: NotLookedAt, .. } => LeftStanding,
        // Nothing of the item is on disk: what is at the new place is not its entry.
        RecordFacts::Moved { row: MovedRow::AtFromWithoutEntry, .. } => NothingLeft,
        // Back under its name, when the directory of that name is there and what is at
        // the new place is still the entry that was moved.
        RecordFacts::Moved { row: MovedRow::AtFromWithEntry, from: Nothing, to: OwnEntry } => Undo(MoveBack),
        RecordFacts::Moved { row: MovedRow::AtFromWithEntry, .. } => NothingLeft,

        // A move over another item's entry.
        RecordFacts::MovedOver { row: MovedRow::AtTo | MovedRow::Elsewhere, .. } | RecordFacts::MovedOver { replaced: ReplacedRow::NotAtTo, .. } => NothingLeft,
        RecordFacts::MovedOver { from: NotLookedAt, .. } | RecordFacts::MovedOver { to: NotLookedAt, .. } | RecordFacts::MovedOver { aside: AsideEntry::NotLookedAt, .. } => LeftStanding,
        // The entry that was set aside goes back under its name when that name is free.
        RecordFacts::MovedOver { row: MovedRow::AtFromWithoutEntry, to, aside, .. } => match (to, aside) {
            (Nothing, AsideEntry::Entry) => Undo(Restore),
            _ => NothingLeft,
        },
        RecordFacts::MovedOver { row: MovedRow::AtFromWithEntry, replaced, from, to, aside } => match (from, to, aside, replaced) {
            // After the move: the moved entry is at the new place, the other set aside.
            (Nothing, OwnEntry, AsideEntry::Entry, _) => Undo(MoveBackAndRestore),
            // After the set-aside, or half way through the undo; or the moved entry is gone.
            // The directory of the name is there, and the name is free.
            (_, Nothing, AsideEntry::Entry, _) => Undo(Restore),
            // Nothing was set aside because the replaced file had nothing on disk: what is
            // at its place is the moved entry.
            (Nothing, OwnEntry, AsideEntry::Nothing, ReplacedRow::AtToWithoutEntry) => Undo(MoveBack),
            // Nothing was set aside and the replaced item has an entry: the one at its
            // place is its own. Or the undo cannot be done: the directory to move back
            // into is gone, or what is at the new place is not the moved entry but a file
            // the person put there since, or the name of the set-aside entry is taken.
            // Nothing is undone over the person's file, and the record does not stand.
            _ => NothingLeft,
        },

        // A set-aside. The row of the change was written.
        RecordFacts::SetAside { row: AsideRow::Otherwise, .. } => NothingLeft,
        RecordFacts::SetAside { place: NotLookedAt, .. } | RecordFacts::SetAside { aside: AsideEntry::NotLookedAt, .. } => LeftStanding,
        RecordFacts::SetAside { row: AsideRow::AtPlaceWithEntry, place: Nothing, aside: AsideEntry::Entry } => Undo(Restore),
        RecordFacts::SetAside { row: AsideRow::AtPlaceWithEntry, .. } => NothingLeft,

        // A landed file.
        RecordFacts::Linked { row: LandedRow::NotAtPlace | LandedRow::OnDisk, .. } => NothingLeft,
        RecordFacts::Linked { entry: LinkedEntry::NotLookedAt, .. } => LeftStanding,
        RecordFacts::Linked { entry: LinkedEntry::Nothing | LinkedEntry::AnotherEntry, .. } => NothingLeft,
        RecordFacts::Linked { row: LandedRow::NotOnDiskWithRecordedContent, entry: LinkedEntry::FileOfRecordedContent } => Finish(RowFinish::ContentOnDisk),
        RecordFacts::Linked { row: LandedRow::NotOnDiskWithOtherContent, entry: LinkedEntry::FileOfRecordedContent } => Undo(RemoveLinkedFile),
    }
}

#[cfg(test)]
mod tests;
