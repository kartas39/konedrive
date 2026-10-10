//! The finding of each path ([`PathFindings`]): of each entry of the backing directory, and
//! of each row no entry is taken as. Here the rows and the contents are put into the typed
//! cases the decision takes ([`decide_finding`]); nothing is written and nothing is read.
//!
//! A row no entry is taken as is decided by itself, a directory before what is in it. When
//! the row of a directory goes — removed here, or its place lost — each row the rows say is
//! in it is decided as one whose path is no more: a row cannot stay in a directory whose row
//! goes. Not the rows that are found at another path: those are moved out. A row at a name
//! the check sets items aside to is decided the same way: no entry has such a name.
//!
//! Nothing is decided of a row whose directory could not be read. And while any part of the
//! backing directory was not read ([`PathFindings::is_read_whole`]), a row that would be
//! found removed, or to have lost its place, is held back with what is below it: what is
//! missing may be in the part that was not read.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use konedrive_tree::{ContentVersion, Item, ItemKind, LocalItemId};

use super::backing_directory_contents::{BackingDirectoryContents, EntryIndex};
use super::disk_finding::{compare_content, decide_finding, ContentComparison, ContentOrigin, DiskFinding, ItemBinding, MarkOwner, PathEntry, PathRow, UnresolvedReason};
use super::entry_identities::{EntryIdentities, EntryIdentity};
use super::item_rows::ItemRows;
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
    /// The rows that would be found removed, or to have lost their place, and are left as
    /// they are, with what is below them.
    held_back: Vec<LocalItemId>,
    is_read_whole: bool,
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

/// What the row of `item` says of the path it has. Where the local content of a file came
/// from is asked of the content itself: one a program changed here is told by a count, and
/// every other is a copy of what OneDrive has — also one the rows took from a cTag mark,
/// which the two sides have not agreed on yet. A file with no binding is changed here.
fn say_of(item: &Item) -> PathRow {
    let binding = if item.onedrive_item.is_some() { ItemBinding::Bound } else { ItemBinding::Unbound };
    match item.kind {
        ItemKind::Directory => PathRow::Directory { binding },
        ItemKind::File => {
            let local_content = item.local.as_ref().and_then(|local| local.content.as_ref());
            let origin = match (binding, local_content) {
                (ItemBinding::Bound, Some(ContentVersion::Hashed { .. })) => ContentOrigin::FromOneDrive,
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
        let is_read_whole = !contents.has_unread_directory() && !Self::is_subtree_hidden(rows, contents, identities);
        let mut findings = PathFindings { entries: Vec::with_capacity(contents.len()), rows: Vec::new(), finding_of_row: HashMap::new(), held_back: Vec::new(), is_read_whole };
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
                EntryIdentity::NewItem => None,
            };
            matches!(reason, UnresolvedReason::Unreadable(_)) || row_at.is_some_and(|row| row.kind == ItemKind::Directory)
        })
    }

    fn decide_entry(rows: &ItemRows, contents: &BackingDirectoryContents, identities: &EntryIdentities, index: EntryIndex) -> EntryFinding {
        let entry = contents.entry(index);
        let directory = match contents.parent(index) {
            Some(directory) => identities.identity(directory),
            None => EntryIdentity::Item(LocalItemId::ROOT),
        };
        let row_at = match directory {
            EntryIdentity::Item(directory) => rows.find_at(directory, &entry.name),
            EntryIdentity::NewItem => None,
        };
        let own = match identities.identity(index) {
            EntryIdentity::Item(item) => rows.find(item),
            EntryIdentity::NewItem => None,
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
            let path_entry = match place {
                // Whether it is there is not known: it is left as it is, with what is below
                // it, but for what is found at another path.
                DirectoryPlace::OnDisk(on_disk) if contents.is_unread(on_disk) || contents.has_unknown_name(on_disk, name) => {
                    self.decide_rows_found_below(rows, contents, identities, row);
                    continue;
                }
                _ if directory == LocalItemId::ROOT && is_set_aside_name(name) => PathEntry::NoSuchPath,
                DirectoryPlace::OnDisk(on_disk) if contents.find_inside(on_disk, name).is_some() => PathEntry::AnotherItemsEntry,
                DirectoryPlace::OnDisk(_) | DirectoryPlace::NotOnDisk => PathEntry::Nothing,
                DirectoryPlace::Gone => PathEntry::NoSuchPath,
            };
            // Nothing and another item's entry are never left unresolved.
            let finding = decide_finding(say_of(row), path_entry).unwrap_or(DiskFinding::NothingChanged);
            let leaves_the_folder = matches!(finding, DiskFinding::Removed | DiskFinding::PlaceLost);
            if leaves_the_folder && !self.is_read_whole {
                // It may be in the part that was not read: it is left, with what is below it.
                self.held_back.push(row.local_item_id);
                self.decide_rows_found_below(rows, contents, identities, row);
                continue;
            }
            self.rows.push((row.local_item_id, finding));
            self.finding_of_row.insert(row.local_item_id, finding);
            if row.kind == ItemKind::Directory {
                let place = if leaves_the_folder { DirectoryPlace::Gone } else { DirectoryPlace::NotOnDisk };
                self.decide_rows_inside(rows, contents, identities, row.local_item_id, place);
            }
        }
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

    /// The rows no entry is taken as that would be found removed, or to have lost their
    /// place, and are left as they are with what is below them: part of the backing
    /// directory was not read, and they may be there.
    pub(super) fn list_held_back(&self) -> &[LocalItemId] {
        &self.held_back
    }

    /// Whether the whole backing directory was read: no directory was left unread, and no
    /// name that was not looked into may hide entries.
    pub(super) fn is_read_whole(&self) -> bool {
        self.is_read_whole
    }
}

#[cfg(test)]
mod tests;
