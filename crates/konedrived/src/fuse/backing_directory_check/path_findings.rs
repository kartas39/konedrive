//! The finding of each path ([`PathFindings`]): of each entry of the backing directory, and
//! of each row no entry is taken as. Here the rows and the contents are put into the typed
//! cases the decision takes ([`decide_finding`]); nothing is written and nothing is read.
//! Which entry is which item is settled before ([`EntryIdentities`]) and is not asked again
//! here: an entry that was left there is left here, with why.
//!
//! A row no entry is taken as is decided by itself, a directory before what is in it. When
//! the row of a directory goes — removed here, or its place lost — each row the rows say is
//! in it is decided as one whose place the folder cannot keep
//! ([`decide_finding_without_place`]): a row cannot stay in a directory whose row goes. Not
//! the rows that are found at another path: those are moved out. A row at a name the check
//! sets items aside to is decided the same way: no entry has such a name.
//!
//! What may be in a part of the backing directory that was not read is left alone, and so
//! is what cannot be told apart. A row is left as it is, with what is below it:
//!
//! - when its own place could not be looked at: nothing is decided of it;
//! - when the entry at its place is one that was left: that entry may be the row's;
//! - when it would be found removed, to have lost its place, or to have its content on disk
//!   no longer, and an entry that carries its item id is left: that entry may be the item;
//! - when it is a directory that would leave the folder and what the rows say is below it
//!   holds such a row: a directory leaves whole, or stays whole;
//! - when it would be found so and any part of the backing directory was not read
//!   ([`PathFindings::has_unread_part`]): what is missing may be in that part.
//!
//! The rows that are left are said ([`PathFindings::list_left_items`]): nothing of them is
//! sent before a check has decided them.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use konedrive_tree::{ContentVersion, Item, ItemKind, LocalItemId};

use super::backing_directory_contents::{BackingDirectoryContents, EntryIndex};
use super::disk_finding::{compare_content, decide_finding, decide_finding_without_place, ContentComparison, ContentOrigin, DiskFinding, ItemBinding, MarkOwner, PathEntry, PathRow, UnresolvedReason};
use super::entry_identities::{may_have_moved, EntryIdentities, EntryIdentity};
use super::item_rows::{BoundRows, ItemRows};
use crate::folder::disk::SET_ASIDE_PREFIX;

/// What was found of one entry of the backing directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EntryFinding {
    /// What the entry is found to be, or why it is left.
    pub(super) finding: Result<DiskFinding, UnresolvedReason>,
    /// Of a file that the finding makes a row's own — moved here, or bound here: what is
    /// found of its content, decided as that row's.
    pub(super) content_finding: Option<DiskFinding>,
}

pub(super) struct PathFindings {
    entries: Vec<EntryFinding>,
    /// The rows no entry is taken as, each with its finding: a directory before what is in
    /// it.
    rows: Vec<(LocalItemId, DiskFinding)>,
    finding_of_row: HashMap<LocalItemId, DiskFinding>,
    /// The rows no entry is taken as that are left as they are, with what is below them,
    /// each with why.
    held_back: Vec<(LocalItemId, UnresolvedReason)>,
    /// The rows whose own place could not be looked at: nothing is decided of them, nor of
    /// what is below them. The place is said where it was read.
    unseen: Vec<LocalItemId>,
    has_unread_part: bool,
}

/// Where the directory is whose rows are decided.
#[derive(Debug, Clone, Copy)]
enum DirectoryPlace {
    /// In the backing directory, as this entry: `None` for the backing directory itself.
    OnDisk(Option<EntryIndex>),
    /// Its row stays, and nothing of it is on disk.
    NotOnDisk,
    /// Its row goes: no path below it is the folder's any more.
    Gone,
}

/// What the row of `item` says of the path it has. A file's local content is a copy of what
/// OneDrive has when it is hashed and is the content the two sides agreed on, by its hash,
/// or when it has no hash: the rows took it from the cTag mark of a copy, and nothing is
/// agreed on yet. A content hashed here that is not the one agreed on is here alone, like
/// one a program changed here, which is told by a count. A file with no binding is here
/// alone.
fn say_of(item: &Item) -> PathRow {
    let binding = if item.onedrive_item.is_some() { ItemBinding::Bound } else { ItemBinding::Unbound };
    match item.kind {
        ItemKind::Directory => PathRow::Directory { binding },
        ItemKind::File => {
            let local_content = item.local.as_ref().and_then(|local| local.content.as_ref());
            let origin = match (binding, local_content) {
                (ItemBinding::Bound, Some(ContentVersion::Hashed { hash: None, .. })) => ContentOrigin::FromOneDrive,
                (ItemBinding::Bound, Some(ContentVersion::Hashed { hash: Some(local), .. })) => match &item.synced_content {
                    Some(ContentVersion::Hashed { hash: Some(synced), .. }) if synced == local => ContentOrigin::FromOneDrive,
                    _ => ContentOrigin::ChangedHere,
                },
                _ => ContentOrigin::ChangedHere,
            };
            PathRow::File { binding, content_presence: item.content_presence, origin }
        }
    }
}

/// Whether the check sets items aside to `name`: a row may have been left there, and no
/// entry of the backing directory is.
fn is_set_aside_name(name: &OsStr) -> bool {
    name.as_bytes().starts_with(SET_ASIDE_PREFIX.as_bytes())
}

impl PathFindings {
    pub(super) fn decide(rows: &ItemRows, contents: &BackingDirectoryContents, identities: &EntryIdentities) -> Self {
        let has_unread_part = contents.has_unread_directory() || Self::is_subtree_hidden(rows, contents, identities);
        let mut findings = PathFindings { entries: Vec::with_capacity(contents.len()), rows: Vec::new(), finding_of_row: HashMap::new(), held_back: Vec::new(), unseen: Vec::new(), has_unread_part };
        for index in 0..contents.len() {
            findings.entries.push(Self::decide_entry(rows, contents, identities, index));
        }
        findings.decide_rows_inside(rows, contents, identities, LocalItemId::ROOT, DirectoryPlace::OnDisk(None));
        findings
    }

    /// Whether a name that was not looked into may hide entries: one that could not be
    /// looked at may be a directory, and one that is no file and no directory where the
    /// rows have a directory stands where what is below that row would be.
    fn is_subtree_hidden(rows: &ItemRows, contents: &BackingDirectoryContents, identities: &EntryIdentities) -> bool {
        contents.list_unknown_names().any(|(directory, name, reason)| {
            let directory = match directory {
                Some(directory) => identities.identity(directory),
                None => EntryIdentity::Item(LocalItemId::ROOT),
            };
            let row_at = match directory {
                EntryIdentity::Item(directory) => rows.find_at(directory, name),
                EntryIdentity::NewItem | EntryIdentity::Left(_) => None,
            };
            matches!(reason, UnresolvedReason::Unreadable(_)) || row_at.is_some_and(|row| row.kind == ItemKind::Directory)
        })
    }

    fn decide_entry(rows: &ItemRows, contents: &BackingDirectoryContents, identities: &EntryIdentities, index: EntryIndex) -> EntryFinding {
        let entry = contents.entry(index);
        let own = match identities.identity(index) {
            EntryIdentity::Item(item) => rows.find(item),
            EntryIdentity::NewItem => None,
            // Which item it is was not settled: nothing is decided of it.
            EntryIdentity::Left(doubt) => return EntryFinding { finding: Err(doubt.to_reason()), content_finding: None },
        };
        let directory = match contents.parent(index) {
            Some(directory) => identities.identity(directory),
            None => EntryIdentity::Item(LocalItemId::ROOT),
        };
        let row_at = match directory {
            EntryIdentity::Item(directory) => rows.find_at(directory, &entry.name),
            EntryIdentity::NewItem | EntryIdentity::Left(_) => None,
        };
        let is_where_its_row_says = own.is_some_and(|own| row_at.is_some_and(|row| row.local_item_id == own.local_item_id));
        let path_row = match (own, row_at) {
            (Some(own), _) if is_where_its_row_says => say_of(own),
            // A row that is found at another path is moved there: it does not have this one.
            (_, Some(row)) if identities.find_entry(row.local_item_id).is_none() => PathRow::AnotherItemsRow,
            _ => PathRow::Nothing,
        };
        let compare_with = |item: &Item| match item.local.as_ref().and_then(|local| local.content.as_ref()) {
            Some(content) => compare_content(content, &entry.file),
            None => ContentComparison::Different,
        };
        let path_entry = |mark: MarkOwner, comparison: ContentComparison| match entry.kind {
            ItemKind::File => PathEntry::File { mark, wholeness: entry.wholeness, comparison },
            ItemKind::Directory => PathEntry::Directory { mark },
        };
        let comparison = own.filter(|_| is_where_its_row_says).map_or(ContentComparison::Different, compare_with);
        let finding = match contents.find_unread_reason(Some(index)) {
            // A directory that could not be read and that neither a mark nor a row says is
            // an item: what it is is not known, and it is not made as a new one.
            Some(reason @ UnresolvedReason::Unreadable(_)) if own.is_none() && identities.mark_owner(index) == MarkOwner::Unmarked => Err(reason.clone()),
            _ => decide_finding(path_row, path_entry(identities.mark_owner(index), comparison)),
        };
        // The file is the row's own by the finding: its content is decided as that row's.
        let content_finding = match (&finding, own) {
            (Ok(DiskFinding::Moved | DiskFinding::MovedOverAnotherItem), Some(own)) if own.kind == ItemKind::File => Some(say_of(own)),
            (Ok(DiskFinding::MarkedWithoutRow), Some(own)) if own.kind == ItemKind::File => match say_of(own) {
                PathRow::File { content_presence, origin, .. } => Some(PathRow::File { binding: ItemBinding::Bound, content_presence, origin }),
                said => Some(said),
            },
            _ => None,
        }
        .and_then(|as_own| decide_finding(as_own, path_entry(MarkOwner::RowAtPath, own.map_or(ContentComparison::Different, compare_with))).ok());
        EntryFinding { finding, content_finding }
    }

    /// Decides the rows in the directory `directory` that no entry is taken as, and those
    /// below them. A row that an entry is taken as is decided with that entry; what the
    /// rows say is in it is decided here, where the entry is.
    fn decide_rows_inside(&mut self, rows: &ItemRows, contents: &BackingDirectoryContents, identities: &EntryIdentities, directory: LocalItemId, place: DirectoryPlace) {
        for row in rows.list_inside(directory) {
            if let Some(entry) = identities.find_entry(row.local_item_id) {
                if row.kind == ItemKind::Directory {
                    self.decide_rows_inside(rows, contents, identities, row.local_item_id, DirectoryPlace::OnDisk(Some(entry)));
                }
                continue;
            }
            let name = row.local.as_ref().and_then(|local| local.place.as_ref()).map(|place| place.name.as_os_str()).unwrap_or_default();
            let has_no_place = matches!(place, DirectoryPlace::Gone) || (directory == LocalItemId::ROOT && is_set_aside_name(name));
            let entry_at = match place {
                DirectoryPlace::OnDisk(on_disk) if !has_no_place => {
                    if contents.is_unread(on_disk) || contents.has_unknown_name(on_disk, name) {
                        // Whether it is there is not known: it is left as it is, with what
                        // is below it, but for what is found at another path.
                        self.unseen.push(row.local_item_id);
                        self.decide_rows_found_below(rows, contents, identities, row);
                        continue;
                    }
                    contents.find_inside(on_disk, name)
                }
                _ => None,
            };
            // The entry at its place was left: it may be this row's.
            if let Some(EntryIdentity::Left(doubt)) = entry_at.map(|entry| identities.identity(entry)) {
                self.hold_back(rows, contents, identities, row, doubt.to_reason());
                continue;
            }
            let finding = match (has_no_place, entry_at) {
                (true, _) => decide_finding_without_place(say_of(row)),
                // Nothing and another item's entry are never left unresolved.
                (false, Some(_)) => decide_finding(say_of(row), PathEntry::AnotherItemsEntry).unwrap_or(DiskFinding::NothingChanged),
                (false, None) => decide_finding(say_of(row), PathEntry::Nothing).unwrap_or(DiskFinding::NothingChanged),
            };
            let leaves_the_folder = matches!(finding, DiskFinding::Removed | DiskFinding::PlaceLost);
            if leaves_the_folder || finding == DiskFinding::ContentNoLongerOnDisk {
                // What is missing may be an entry that was left and carries its item id, or
                // be in the part that was not read. And a directory does not leave while
                // what the rows say is below it holds a row that is left so: it is left
                // with it, whole. (Below a directory that leaves, this was asked already.)
                let doubt = Self::find_doubt(rows, contents, identities, row).or_else(|| match (leaves_the_folder, place) {
                    (true, DirectoryPlace::OnDisk(_) | DirectoryPlace::NotOnDisk) => Self::find_doubt_below(rows, contents, identities, row),
                    _ => None,
                });
                let reason = doubt.or(self.has_unread_part.then_some(UnresolvedReason::MayBeUnread));
                if let Some(reason) = reason {
                    self.hold_back(rows, contents, identities, row, reason);
                    continue;
                }
            }
            self.rows.push((row.local_item_id, finding));
            self.finding_of_row.insert(row.local_item_id, finding);
            if row.kind == ItemKind::Directory {
                let place = if leaves_the_folder { DirectoryPlace::Gone } else { DirectoryPlace::NotOnDisk };
                self.decide_rows_inside(rows, contents, identities, row.local_item_id, place);
            }
        }
    }

    /// Why the row `row`, which no entry is taken as and which is not where it says, may be
    /// an entry that was left: one carries its item id, and something of the item was on
    /// disk to be moved. What was not on disk was not moved: no entry is it.
    fn find_doubt(rows: &ItemRows, contents: &BackingDirectoryContents, identities: &EntryIdentities, row: &Item) -> Option<UnresolvedReason> {
        let onedrive_id = row.onedrive_item.as_ref().map(|reference| reference.item_id.as_str())?;
        if !may_have_moved(row) || !identities.has_left_entry_marked(contents, onedrive_id, row.kind) {
            return None;
        }
        Some(match rows.find_bound(onedrive_id) {
            BoundRows::OfTwoDrives => UnresolvedReason::ItemIdInTwoDrives,
            BoundRows::One(_) | BoundRows::None => UnresolvedReason::MayBeUnread,
        })
    }

    /// [`find_doubt`](Self::find_doubt) of the rows the rows say are below `directory` that
    /// no entry is taken as: the first there is.
    fn find_doubt_below(rows: &ItemRows, contents: &BackingDirectoryContents, identities: &EntryIdentities, directory: &Item) -> Option<UnresolvedReason> {
        if directory.kind != ItemKind::Directory {
            return None;
        }
        rows.list_inside(directory.local_item_id).into_iter().filter(|row| identities.find_entry(row.local_item_id).is_none()).find_map(|row| Self::find_doubt(rows, contents, identities, row).or_else(|| Self::find_doubt_below(rows, contents, identities, row)))
    }

    /// The row `row` is left as it is, with what is below it, and why is said.
    fn hold_back(&mut self, rows: &ItemRows, contents: &BackingDirectoryContents, identities: &EntryIdentities, row: &Item, reason: UnresolvedReason) {
        self.held_back.push((row.local_item_id, reason));
        self.decide_rows_found_below(rows, contents, identities, row);
    }

    /// Of what the rows say is below `left`, a row that is left as it is: only what is
    /// found at another path is decided, there. The rest is left with it.
    fn decide_rows_found_below(&mut self, rows: &ItemRows, contents: &BackingDirectoryContents, identities: &EntryIdentities, left: &Item) {
        if left.kind != ItemKind::Directory {
            return;
        }
        for row in rows.list_inside(left.local_item_id) {
            match identities.find_entry(row.local_item_id) {
                Some(entry) if row.kind == ItemKind::Directory => self.decide_rows_inside(rows, contents, identities, row.local_item_id, DirectoryPlace::OnDisk(Some(entry))),
                Some(_) => {}
                None => self.decide_rows_found_below(rows, contents, identities, row),
            }
        }
    }

    pub(super) fn entry(&self, index: EntryIndex) -> &EntryFinding {
        &self.entries[index]
    }

    /// The rows no entry is taken as, each with its finding: a directory before what is in
    /// it.
    pub(super) fn list_rows(&self) -> &[(LocalItemId, DiskFinding)] {
        &self.rows
    }

    /// The finding of the row of `item`, which no entry is taken as; `None` for a row an
    /// entry is taken as, and for one that is left as it is.
    pub(super) fn find_row_finding(&self, item: LocalItemId) -> Option<DiskFinding> {
        self.finding_of_row.get(&item).copied()
    }

    /// The rows no entry is taken as that are left as they are with what is below them,
    /// each with why: it may be in the part of the backing directory that was not read, or
    /// be an entry that was left.
    pub(super) fn list_held_back(&self) -> &[(LocalItemId, UnresolvedReason)] {
        &self.held_back
    }

    /// Every row that is left as it was found: those that are held back, and those whose
    /// own place could not be looked at. What is below one of them is left with it.
    pub(super) fn list_left_items(&self) -> impl Iterator<Item = LocalItemId> + '_ {
        self.held_back.iter().map(|(item, _)| *item).chain(self.unseen.iter().copied())
    }

    /// Whether any part of the backing directory was not read: a directory was left unread,
    /// or a name that was not looked into may hide entries.
    pub(super) fn has_unread_part(&self) -> bool {
        self.has_unread_part
    }
}

#[cfg(test)]
mod tests;
