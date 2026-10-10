//! Which item each entry of the backing directory is ([`EntryIdentities`]): an item that has
//! a row, or a new one ([`EntryIdentity`]); and so whose the mark on the entry is
//! ([`MarkOwner`]). Settled for the whole backing directory before anything is decided, from
//! the rows and the contents as they were read: no disk and no store here.
//!
//! An entry is looked up among the rows of the item its directory is, not at its path: the
//! rows below a directory that was moved by hand name it by its id, and are found where
//! they are.
//!
//! - A mark names an item id and no drive. An entry is first the item of the row at its own
//!   place, when that row has the mark's id, whichever drive the row is of. Elsewhere the
//!   mark is looked for in the drive of the row of the directory the entry is in: of the
//!   nearest directory above it that has a binding, or the folder's own drive.
//! - An entry marked with the id of a row the folder shows, of its kind, is that item: where
//!   the row says, or — when no other entry carries the mark — moved here.
//! - Of several entries that carry one mark, the one where the row says is the item. When
//!   none is there, the first the walk comes to is the item, moved; the walk goes a
//!   directory before what is in it, and a directory's entries by name. The others are
//!   copies made by hand. Such an entry waits until the walk has seen every entry it can
//!   reach, and a directory that waits is settled before a file that does: the entries in
//!   it may be where their rows say.
//! - An entry that is not taken as an item by its mark is the item of the row its path has,
//!   when that row is of its kind and is not found at another path by its own mark; else it
//!   is a new item.

use std::collections::{HashMap, HashSet, VecDeque};

use konedrive_tree::{DriveId, Item, ItemKind, LocalItemId};

use super::backing_directory_contents::{BackingDirectoryContents, EntryIndex};
use super::disk_finding::MarkOwner;
use super::item_rows::ItemRows;

/// Which item an entry of the backing directory is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryIdentity {
    /// The item of this row.
    Item(LocalItemId),
    /// An item no row knows.
    NewItem,
}

pub(super) struct EntryIdentities {
    /// By entry; `None` only while they are being settled.
    settled: Vec<Option<(EntryIdentity, MarkOwner)>>,
    /// The entry each item is.
    entries: HashMap<LocalItemId, EntryIndex>,
    /// The ids no row the folder shows has, each with the drive it was looked for in, that
    /// an entry was taken as the item of.
    unknown_ids: HashSet<(DriveId, String)>,
}

impl EntryIdentities {
    /// `drive_id` is the folder's own drive: where a mark is looked for in a directory that
    /// has no directory of OneDrive above it.
    pub(super) fn settle(rows: &ItemRows, contents: &BackingDirectoryContents, drive_id: &DriveId) -> Self {
        let mut identities = EntryIdentities { settled: vec![None; contents.len()], entries: HashMap::new(), unknown_ids: HashSet::new() };
        // The directories whose entries are next, and the entries that wait for the walk to
        // have seen every other entry of their mark.
        let mut directories: VecDeque<Option<EntryIndex>> = VecDeque::from([None]);
        let mut waiting_directories: VecDeque<EntryIndex> = VecDeque::new();
        let mut waiting_files: VecDeque<EntryIndex> = VecDeque::new();
        loop {
            while let Some(directory) = directories.pop_front() {
                let directory_identity = identities.identity_of_directory(directory);
                let drive = identities.find_drive_inside(rows, contents, directory, drive_id);
                for index in contents.list_inside(directory) {
                    match identities.take_by_mark(rows, contents, index, directory_identity, drive) {
                        Some((identity, mark_owner)) => identities.set(contents, index, identity, mark_owner, &mut directories),
                        None if contents.entry(index).kind == ItemKind::Directory => waiting_directories.push_back(index),
                        None => waiting_files.push_back(index),
                    }
                }
            }
            // A directory first: what is in it may be where its row says.
            let Some(index) = waiting_directories.pop_front().or_else(|| waiting_files.pop_front()) else { break };
            let directory = contents.parent(index);
            let directory_identity = identities.identity_of_directory(directory);
            let drive = identities.find_drive_inside(rows, contents, directory, drive_id);
            let marked_as = contents.entry(index).onedrive_id.as_deref().and_then(|id| rows.find_shown_bound(drive, id));
            let (identity, mark_owner) = match marked_as {
                // No entry is where the row says, and none was taken as the item before.
                Some(item) if !identities.entries.contains_key(&item.local_item_id) => (EntryIdentity::Item(item.local_item_id), MarkOwner::RowElsewhere),
                _ => (identities.take_by_place(rows, contents, index, directory_identity), MarkOwner::RowOfAnotherEntry),
            };
            identities.set(contents, index, identity, mark_owner, &mut directories);
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

    /// The drive a mark on an entry in `directory` is looked for in: that of the nearest
    /// directory, from `directory` up, that is an item with a binding; `drive_id`, the
    /// folder's own, when there is none below the root.
    fn find_drive_inside<'a>(&self, rows: &'a ItemRows, contents: &BackingDirectoryContents, directory: Option<EntryIndex>, drive_id: &'a DriveId) -> &'a DriveId {
        let mut at = directory;
        while let Some(entry) = at {
            let bound = match self.identity(entry) {
                EntryIdentity::Item(item) => rows.find(item).and_then(|row| row.onedrive_item.as_ref()),
                EntryIdentity::NewItem => None,
            };
            if let Some(reference) = bound {
                return &reference.drive_id;
            }
            at = contents.parent(entry);
        }
        drive_id
    }

    fn set(&mut self, contents: &BackingDirectoryContents, index: EntryIndex, identity: EntryIdentity, mark_owner: MarkOwner, directories: &mut VecDeque<Option<EntryIndex>>) {
        self.settled[index] = Some((identity, mark_owner));
        if let EntryIdentity::Item(item) = identity {
            self.entries.insert(item, index);
        }
        if contents.entry(index).kind == ItemKind::Directory {
            directories.push_back(Some(index));
        }
    }

    /// The row the path of the entry has: the one `name` has in the item its directory is.
    fn find_row_at<'a>(rows: &'a ItemRows, contents: &BackingDirectoryContents, index: EntryIndex, directory: EntryIdentity) -> Option<&'a Item> {
        match directory {
            EntryIdentity::Item(directory) => rows.find_at(directory, &contents.entry(index).name),
            EntryIdentity::NewItem => None,
        }
    }

    /// Whether `item` is found at another path than that of the entry `index`: an entry of
    /// its kind that is not this one carries its mark, and one of those is taken as it.
    fn is_found_elsewhere(contents: &BackingDirectoryContents, item: &Item, index: EntryIndex) -> bool {
        let Some(reference) = &item.onedrive_item else { return false };
        contents.list_marked(reference.item_id.as_str(), item.kind).any(|marked| marked != index)
            && contents.entry(index).onedrive_id.as_deref() != Some(reference.item_id.as_str())
    }

    /// What the entry is when its mark does not say: the item of the row its path has, when
    /// that row is of its kind and is not found elsewhere; else a new item.
    fn take_by_place(&self, rows: &ItemRows, contents: &BackingDirectoryContents, index: EntryIndex, directory: EntryIdentity) -> EntryIdentity {
        match Self::find_row_at(rows, contents, index, directory) {
            Some(row) if row.kind == contents.entry(index).kind && !self.entries.contains_key(&row.local_item_id) && !Self::is_found_elsewhere(contents, row, index) => {
                EntryIdentity::Item(row.local_item_id)
            }
            _ => EntryIdentity::NewItem,
        }
    }

    /// What the entry is by its mark, which is looked for in `drive` when the row at the
    /// entry's own place does not have it. `None` when several entries carry the mark and
    /// this one is not where the row says: it waits until the walk has seen the others.
    fn take_by_mark(&mut self, rows: &ItemRows, contents: &BackingDirectoryContents, index: EntryIndex, directory: EntryIdentity, drive: &DriveId) -> Option<(EntryIdentity, MarkOwner)> {
        let entry = contents.entry(index);
        let Some(onedrive_id) = entry.onedrive_id.as_deref() else {
            return Some((self.take_by_place(rows, contents, index, directory), MarkOwner::Unmarked));
        };
        // The row at its own place first, whichever drive that row is of.
        let row_at = Self::find_row_at(rows, contents, index, directory);
        let in_place = row_at.filter(|row| row.kind == entry.kind && row.onedrive_item.as_ref().is_some_and(|reference| reference.item_id.as_str() == onedrive_id));
        if let Some(row) = in_place.filter(|row| !self.entries.contains_key(&row.local_item_id)) {
            return Some((EntryIdentity::Item(row.local_item_id), MarkOwner::RowAtPath));
        }
        match rows.find_shown_bound(drive, onedrive_id) {
            Some(item) if item.kind == entry.kind => {
                // Another entry is the item already: one that waited, and was the first.
                if self.entries.contains_key(&item.local_item_id) {
                    return Some((self.take_by_place(rows, contents, index, directory), MarkOwner::RowOfAnotherEntry));
                }
                let is_the_only_one = contents.list_marked(onedrive_id, entry.kind).nth(1).is_none();
                is_the_only_one.then_some((EntryIdentity::Item(item.local_item_id), MarkOwner::RowElsewhere))
            }
            // The mark of an item of the other kind: an item never changes its kind.
            Some(_) => Some((self.take_by_place(rows, contents, index, directory), MarkOwner::RowOfAnotherEntry)),
            None => {
                let is_of_another_kind = rows.find_unshown_bound(drive, onedrive_id).is_some_and(|unshown| unshown.kind != entry.kind);
                // One entry is bound to an id no row has; a second that carries it is a copy.
                let mark_owner = if is_of_another_kind || !self.unknown_ids.insert((drive.clone(), onedrive_id.to_owned())) { MarkOwner::RowOfAnotherEntry } else { MarkOwner::NoRow };
                Some((self.take_by_place(rows, contents, index, directory), mark_owner))
            }
        }
    }

    pub(super) fn identity(&self, index: EntryIndex) -> EntryIdentity {
        self.settled[index].map_or(EntryIdentity::NewItem, |(identity, _)| identity)
    }

    pub(super) fn mark_owner(&self, index: EntryIndex) -> MarkOwner {
        self.settled[index].map_or(MarkOwner::Unmarked, |(_, mark_owner)| mark_owner)
    }

    /// The entry `item` is; `None` when no entry is taken as it.
    pub(super) fn find_entry(&self, item: LocalItemId) -> Option<EntryIndex> {
        self.entries.get(&item).copied()
    }
}

#[cfg(test)]
mod tests;
