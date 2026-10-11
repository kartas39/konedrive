//! What the check does to the marks of an entry ([`MarkChange`]), and the decision that
//! says it ([`decide_mark_change`]): a function of whose entry it is, of what its id mark
//! names and of how many names its file has, with no disk in it.
//!
//! The check writes marks for two things only. An entry it takes as the own entry of an
//! item OneDrive has carries that item's id afterwards; and a mark on an entry that becomes
//! a new item of the user's own is taken off. A mark is the file's, not the name's: where a
//! file has several names, a mark written or taken off through one of them is written or
//! taken off for the others, which may be other items' entries. Such a file's marks are
//! left as they are. That is stable from start to start, also after a store was rebuilt:
//! while two names in the folder carry one id, the id is one several entries carry, and
//! binds none of them.

use super::disk_finding::EntryMark;

/// Whose entry the check took an entry as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryOwner {
    /// The row at its place, an item OneDrive has.
    BoundRow,
    /// The row at its place, an item OneDrive never had.
    UnboundRow,
    /// A new item of the user's own.
    NewItem,
    /// A new item bound to the id of the entry's mark.
    NewItemBoundByMark,
}

/// How many names the file of an entry has. A directory has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryLinks {
    One,
    Several,
}

/// What is done to the marks of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MarkChange {
    Nothing,
    /// The id of the entry's item is written; it carried none.
    WriteId,
    /// The marks it carries, which are another item's, are taken off, and the id of its
    /// item written.
    ReplaceWithId,
    /// The marks it carries are taken off: it is the user's own.
    TakeOff,
}

/// What the check does to the marks of an entry it took as `owner`'s, marked as `mark`
/// says, whose file has `links` names.
pub(super) fn decide_mark_change(owner: EntryOwner, mark: EntryMark, links: EntryLinks) -> MarkChange {
    match (links, owner, mark) {
        // The marks are the file's, and another name of it may be another item's entry.
        (EntryLinks::Several, _, _) => MarkChange::Nothing,
        (EntryLinks::One, EntryOwner::BoundRow, EntryMark::OfRowAtPlace) => MarkChange::Nothing,
        (EntryLinks::One, EntryOwner::BoundRow, EntryMark::Unmarked) => MarkChange::WriteId,
        (EntryLinks::One, EntryOwner::BoundRow, EntryMark::OfNoRow | EntryMark::OfAnotherRow | EntryMark::OfSeveralEntries) => MarkChange::ReplaceWithId,
        (EntryLinks::One, EntryOwner::NewItem, EntryMark::Unmarked) => MarkChange::Nothing,
        (EntryLinks::One, EntryOwner::NewItem, EntryMark::OfRowAtPlace | EntryMark::OfNoRow | EntryMark::OfAnotherRow | EntryMark::OfSeveralEntries) => MarkChange::TakeOff,
        // What the check reads no mark of, it writes none of; and the mark an item is
        // bound by is its own.
        (EntryLinks::One, EntryOwner::UnboundRow | EntryOwner::NewItemBoundByMark, _) => MarkChange::Nothing,
    }
}

#[cfg(test)]
mod tests;
