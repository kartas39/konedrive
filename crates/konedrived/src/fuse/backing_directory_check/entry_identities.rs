//! Which item each entry of the backing directory is ([`EntryIdentities`]): an item that has
//! a row, a new one, or one that cannot be said and is left ([`EntryIdentity`]); and so
//! whose the mark on the entry is ([`MarkOwner`]). Settled for the whole backing directory
//! before anything is decided, from the rows and the contents as they were read: no disk and
//! no store here. It is settled here once: nothing later takes an entry as another item, or
//! an item as another entry.
//!
//! An entry is looked up among the rows of the item its directory is, not at its path: the
//! rows below a directory that was moved by hand name it by its id, and are found where
//! they are.
//!
//! - A mark names an item id and no drive, and no drive is guessed here. An entry is the
//!   item of the row at its own place when that row has the mark's id. Otherwise the mark
//!   names the one row that has that item id; when rows of two drives have it, the entry is
//!   left ([`IdentityDoubt::ItemIdInTwoDrives`]).
//! - An entry marked with the id of the one row that has it, a row the folder shows, of its
//!   kind, is that item moved here, when no entry is taken as the item already. It waits
//!   for its turn: the rows nearest the root first, so that a directory moved by hand is
//!   settled, and what is where its row says in it, before a copy of what is in it is
//!   looked at; of two entries that wait for one row, the first the walk came to.
//! - What was not on disk was not moved by hand: an entry marked with the id of a file whose
//!   row says its content is not on disk is not that file moved. It is a copy.
//! - The place a row says counts as looked at only through the entries already taken as the
//!   rows of the directories above it. An entry marked with the id of a row whose own place
//!   could not be looked at is left ([`IdentityDoubt::PlaceNotLookedAt`]): the item may be
//!   where its row says, and the entry a copy. An entry that is left is taken as no item,
//!   and what is in it is left with it.
//! - An entry that is not taken as an item by its mark is the item of the row its path has,
//!   when that row is of its kind. Where other entries carry the id of that row, it waits
//!   until they are settled: when one of them is taken as the item, moved, it is a new
//!   item; when one of them is left, it is left for the same doubt.
//! - A mark no row has: the first entry that carries it is the item of it, a second a copy.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

use konedrive_tree::{ContentPresence, Item, ItemKind, LocalItemId};

use super::backing_directory_contents::{BackingDirectoryContents, EntryIndex};
use super::disk_finding::{MarkOwner, UnresolvedReason};
use super::item_rows::{BoundRows, ItemRows};

/// Why it cannot be said which item an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IdentityDoubt {
    /// It carries the id of a row whose own place could not be looked at, or stands at the
    /// place of a row whose id such an entry carries.
    PlaceNotLookedAt,
    /// It carries an id that rows of two drives have, elsewhere than where one of them
    /// says, or stands at the place of one of them while such an entry is left.
    ItemIdInTwoDrives,
}

impl IdentityDoubt {
    /// The reason the entry is left with.
    pub(super) fn to_reason(self) -> UnresolvedReason {
        match self {
            IdentityDoubt::PlaceNotLookedAt => UnresolvedReason::MayBeUnread,
            IdentityDoubt::ItemIdInTwoDrives => UnresolvedReason::ItemIdInTwoDrives,
        }
    }
}

/// Which item an entry of the backing directory is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryIdentity {
    /// The item of this row.
    Item(LocalItemId),
    /// An item no row knows.
    NewItem,
    /// It cannot be said: the entry is left as it is, with what is in it.
    Left(IdentityDoubt),
}

/// When an entry that waits is settled: the entries that wait for a row by the depth of
/// that row, the nearest the root first, and then those that wait for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Turn {
    /// It carries the id of a row this many directories below the root.
    OfRowAtDepth(usize),
    /// It stands at the place of a row whose id other entries carry.
    AfterMarkedEntries,
}

/// What is said of an entry when it is looked at.
enum Sight {
    Settled(EntryIdentity, MarkOwner),
    /// Not yet: at this turn, and with this said of its mark already.
    Waits(Turn, MarkOwner),
}

pub(super) struct EntryIdentities {
    /// By entry; `None` only while they are being settled.
    settled: Vec<Option<(EntryIdentity, MarkOwner)>>,
    /// The entry each item is.
    entries: HashMap<LocalItemId, EntryIndex>,
    /// The ids no row has that an entry was taken as the item of.
    unknown_ids: HashSet<String>,
}

/// Whether something of the item was on disk to be moved by hand: a file whose content is on
/// disk, or a directory. (The rows do not say whether a directory of OneDrive has one on
/// disk: it needs none.)
pub(super) fn may_have_moved(row: &Item) -> bool {
    row.kind == ItemKind::Directory || row.content_presence == ContentPresence::OnDisk
}

impl EntryIdentities {
    pub(super) fn settle(rows: &ItemRows, contents: &BackingDirectoryContents) -> Self {
        let mut identities = EntryIdentities { settled: vec![None; contents.len()], entries: HashMap::new(), unknown_ids: HashSet::new() };
        // The directories whose entries are next, and the entries that wait for their turn.
        let mut directories: VecDeque<Option<EntryIndex>> = VecDeque::from([None]);
        let mut waiting: BinaryHeap<Reverse<(Turn, EntryIndex)>> = BinaryHeap::new();
        let mut said_of_mark: HashMap<EntryIndex, MarkOwner> = HashMap::new();
        loop {
            while let Some(directory) = directories.pop_front() {
                let directory_identity = identities.identity_of_directory(directory);
                for index in contents.list_inside(directory) {
                    let sight = identities.look_at(rows, contents, index, directory_identity);
                    identities.note(contents, index, sight, &mut directories, &mut waiting, &mut said_of_mark);
                }
            }
            let Some(Reverse((turn, index))) = waiting.pop() else { break };
            let directory_identity = identities.identity_of_directory(contents.parent(index));
            let sight = match turn {
                Turn::OfRowAtDepth(_) => identities.take_by_mark(rows, contents, index, directory_identity),
                Turn::AfterMarkedEntries => {
                    let mark_owner = said_of_mark.remove(&index).unwrap_or(MarkOwner::Unmarked);
                    Sight::Settled(identities.take_by_place_after_marked_entries(rows, contents, index, directory_identity), mark_owner)
                }
            };
            identities.note(contents, index, sight, &mut directories, &mut waiting, &mut said_of_mark);
        }
        identities
    }

    /// Which item `directory` is: the root of the folder for the backing directory itself.
    fn identity_of_directory(&self, directory: Option<EntryIndex>) -> EntryIdentity {
        match directory {
            Some(directory) => self.identity(directory),
            None => EntryIdentity::Item(LocalItemId::ROOT),
        }
    }

    fn note(
        &mut self,
        contents: &BackingDirectoryContents,
        index: EntryIndex,
        sight: Sight,
        directories: &mut VecDeque<Option<EntryIndex>>,
        waiting: &mut BinaryHeap<Reverse<(Turn, EntryIndex)>>,
        said_of_mark: &mut HashMap<EntryIndex, MarkOwner>,
    ) {
        match sight {
            Sight::Waits(turn, mark_owner) => {
                said_of_mark.insert(index, mark_owner);
                waiting.push(Reverse((turn, index)));
            }
            Sight::Settled(identity, mark_owner) => {
                self.settled[index] = Some((identity, mark_owner));
                match identity {
                    EntryIdentity::Item(item) => {
                        self.entries.insert(item, index);
                    }
                    EntryIdentity::NewItem => {}
                    // What is in an entry that is left is left with it, and is looked at no
                    // further.
                    EntryIdentity::Left(_) => return self.leave_inside(contents, index, identity),
                }
                if contents.entry(index).kind == ItemKind::Directory {
                    directories.push_back(Some(index));
                }
            }
        }
    }

    fn leave_inside(&mut self, contents: &BackingDirectoryContents, directory: EntryIndex, left: EntryIdentity) {
        for inside in contents.list_inside(Some(directory)) {
            self.settled[inside] = Some((left, MarkOwner::Unmarked));
            self.leave_inside(contents, inside, left);
        }
    }

    /// The row the path of the entry has: the one `name` has in the item its directory is.
    fn find_row_at<'a>(rows: &'a ItemRows, contents: &BackingDirectoryContents, index: EntryIndex, directory: EntryIdentity) -> Option<&'a Item> {
        match directory {
            EntryIdentity::Item(directory) => rows.find_at(directory, &contents.entry(index).name),
            EntryIdentity::NewItem | EntryIdentity::Left(_) => None,
        }
    }

    /// How many directories below the root the row is.
    fn find_depth(rows: &ItemRows, row: &Item) -> usize {
        let mut depth = 0;
        let mut at = row;
        while depth <= konedrive_fs::MAX_DEPTH {
            let above = at.local.as_ref().and_then(|local| local.place.as_ref()).and_then(|place| rows.find(place.parent_local_item_id));
            let Some(above) = above.filter(|above| above.local_item_id != LocalItemId::ROOT) else { break };
            at = above;
            depth += 1;
        }
        depth
    }

    /// Whether the place the row `row` says could not be looked at: the directory it is in
    /// was not read, or its name there is one that was not looked into, or the same holds
    /// of a directory above it that no entry is taken as. The item may be there. The place
    /// is walked to through the entries taken as the rows above it, and through no other.
    fn is_place_hidden(&self, rows: &ItemRows, contents: &BackingDirectoryContents, row: &Item) -> bool {
        let mut at = row;
        // One row for each directory above the item.
        for _ in 0..=konedrive_fs::MAX_DEPTH {
            let Some(place) = at.local.as_ref().and_then(|local| local.place.as_ref()) else { return false };
            let directory = match place.parent_local_item_id {
                LocalItemId::ROOT => None,
                directory => match (self.find_entry(directory), rows.find(directory)) {
                    (Some(entry), _) => Some(entry),
                    (None, Some(above)) => {
                        at = above;
                        continue;
                    }
                    (None, None) => return false,
                },
            };
            return contents.is_unread(directory) || contents.has_unknown_name(directory, &place.name);
        }
        false
    }

    /// What is said of the entry when the walk comes to it.
    fn look_at(&mut self, rows: &ItemRows, contents: &BackingDirectoryContents, index: EntryIndex, directory: EntryIdentity) -> Sight {
        let entry = contents.entry(index);
        let Some(onedrive_id) = entry.onedrive_id.as_deref() else {
            return self.take_by_place(rows, contents, index, directory, MarkOwner::Unmarked);
        };
        // The row at its own place first, whichever drive that row is of.
        let row_at = Self::find_row_at(rows, contents, index, directory);
        let in_place = row_at.filter(|row| row.kind == entry.kind && row.onedrive_item.as_ref().is_some_and(|reference| reference.item_id.as_str() == onedrive_id));
        if let Some(row) = in_place.filter(|row| !self.entries.contains_key(&row.local_item_id)) {
            return Sight::Settled(EntryIdentity::Item(row.local_item_id), MarkOwner::RowAtPath);
        }
        let mark_owner = match rows.find_bound(onedrive_id) {
            // Which of them is meant the mark does not say.
            BoundRows::OfTwoDrives => return Sight::Settled(EntryIdentity::Left(IdentityDoubt::ItemIdInTwoDrives), MarkOwner::AnotherItem),
            // An item never changes its kind, and the folder itself is no entry of itself.
            BoundRows::One(row) if row.kind != entry.kind || row.local_item_id == LocalItemId::ROOT => MarkOwner::AnotherItem,
            // A row the folder shows.
            BoundRows::One(row) if row.local.is_some() => match may_have_moved(row) {
                true => return Sight::Waits(Turn::OfRowAtDepth(Self::find_depth(rows, row)), MarkOwner::RowElsewhere),
                false => MarkOwner::AnotherItem,
            },
            // No row, or one the folder does not show: one entry is the item of the id, and
            // a second that carries it is a copy.
            BoundRows::One(_) | BoundRows::None => match self.unknown_ids.insert(onedrive_id.to_owned()) {
                true => MarkOwner::NoRow,
                false => MarkOwner::AnotherItem,
            },
        };
        self.take_by_place(rows, contents, index, directory, mark_owner)
    }

    /// What an entry that waited for the row of its mark is: that item, moved here, when no
    /// entry is taken as it and its own place was looked at.
    fn take_by_mark(&mut self, rows: &ItemRows, contents: &BackingDirectoryContents, index: EntryIndex, directory: EntryIdentity) -> Sight {
        let row = match contents.entry(index).onedrive_id.as_deref().map(|onedrive_id| rows.find_bound(onedrive_id)) {
            Some(BoundRows::One(row)) => row,
            _ => return self.take_by_place(rows, contents, index, directory, MarkOwner::AnotherItem),
        };
        if self.entries.contains_key(&row.local_item_id) {
            // Another entry is the item: this one is a copy made by hand.
            return self.take_by_place(rows, contents, index, directory, MarkOwner::AnotherItem);
        }
        if self.is_place_hidden(rows, contents, row) {
            return Sight::Settled(EntryIdentity::Left(IdentityDoubt::PlaceNotLookedAt), MarkOwner::RowElsewhere);
        }
        Sight::Settled(EntryIdentity::Item(row.local_item_id), MarkOwner::RowElsewhere)
    }

    /// The entries of the kind of `row`, other than `index`, that carry its id.
    fn list_marked_as<'a>(contents: &'a BackingDirectoryContents, row: &'a Item, index: EntryIndex) -> impl Iterator<Item = EntryIndex> + 'a {
        let onedrive_id = row.onedrive_item.as_ref().map(|reference| reference.item_id.as_str()).unwrap_or_default();
        contents.list_marked(onedrive_id, row.kind).filter(move |marked| *marked != index)
    }

    /// The row the entry stands at the place of and may be the item of: one of its kind
    /// that no entry is taken as.
    fn find_free_row_at<'a>(&self, rows: &'a ItemRows, contents: &BackingDirectoryContents, index: EntryIndex, directory: EntryIdentity) -> Option<&'a Item> {
        Self::find_row_at(rows, contents, index, directory).filter(|row| row.kind == contents.entry(index).kind && !self.entries.contains_key(&row.local_item_id))
    }

    /// What the entry is when its mark does not say: the item of the row its path has, when
    /// that row is of its kind; else a new item. It waits where other entries carry the id
    /// of that row, and something of the item was on disk to be moved.
    fn take_by_place(&self, rows: &ItemRows, contents: &BackingDirectoryContents, index: EntryIndex, directory: EntryIdentity, mark_owner: MarkOwner) -> Sight {
        let Some(row) = self.find_free_row_at(rows, contents, index, directory) else { return Sight::Settled(EntryIdentity::NewItem, mark_owner) };
        let is_marked_elsewhere = row.onedrive_item.is_some() && may_have_moved(row) && Self::list_marked_as(contents, row, index).next().is_some();
        match is_marked_elsewhere {
            true => Sight::Waits(Turn::AfterMarkedEntries, mark_owner),
            false => Sight::Settled(EntryIdentity::Item(row.local_item_id), mark_owner),
        }
    }

    /// What an entry is that waited for the entries that carry the id of the row at its
    /// place: a new item when one of them is the item, moved; left when one of them is left,
    /// since it may be the item; else the item of that row.
    fn take_by_place_after_marked_entries(&self, rows: &ItemRows, contents: &BackingDirectoryContents, index: EntryIndex, directory: EntryIdentity) -> EntryIdentity {
        let Some(row) = self.find_free_row_at(rows, contents, index, directory) else { return EntryIdentity::NewItem };
        let doubt = Self::list_marked_as(contents, row, index).find_map(|marked| match self.settled[marked] {
            Some((EntryIdentity::Left(doubt), _)) => Some(doubt),
            // Not looked at yet: it waits for this one.
            None => Some(IdentityDoubt::PlaceNotLookedAt),
            Some((EntryIdentity::Item(_) | EntryIdentity::NewItem, _)) => None,
        });
        match doubt {
            Some(doubt) => EntryIdentity::Left(doubt),
            None => EntryIdentity::Item(row.local_item_id),
        }
    }

    pub(super) fn identity(&self, index: EntryIndex) -> EntryIdentity {
        self.settled[index].map_or(EntryIdentity::Left(IdentityDoubt::PlaceNotLookedAt), |(identity, _)| identity)
    }

    /// The item the entry is taken as; `None` for a new item, and for an entry that is left.
    pub(super) fn find_item(&self, index: EntryIndex) -> Option<LocalItemId> {
        match self.identity(index) {
            EntryIdentity::Item(item) => Some(item),
            EntryIdentity::NewItem | EntryIdentity::Left(_) => None,
        }
    }

    /// Whose the mark is of an entry that is an item, or a new one.
    pub(super) fn mark_owner(&self, index: EntryIndex) -> MarkOwner {
        self.settled[index].map_or(MarkOwner::Unmarked, |(_, mark_owner)| mark_owner)
    }

    /// The entry `item` is; `None` when no entry is taken as it.
    pub(super) fn find_entry(&self, item: LocalItemId) -> Option<EntryIndex> {
        self.entries.get(&item).copied()
    }

    /// Whether an entry of `kind` that carries the id `onedrive_id` is left: it may be any
    /// row that has the id.
    pub(super) fn has_left_entry_marked(&self, contents: &BackingDirectoryContents, onedrive_id: &str, kind: ItemKind) -> bool {
        contents.list_marked(onedrive_id, kind).any(|marked| matches!(self.identity(marked), EntryIdentity::Left(_)))
    }
}

#[cfg(test)]
mod tests;
