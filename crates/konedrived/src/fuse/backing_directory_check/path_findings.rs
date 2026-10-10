//! The finding of each path ([`PathFindings`]): of each entry of the backing directory, and
//! of each row no entry is taken as. Here the rows and the contents are put into the typed
//! cases the decision takes ([`decide_finding`]); nothing is written and nothing is read.
//!
//! A row no entry is taken as is decided by itself, a directory before what is in it. When
//! a directory is removed here, all the rows say is in it is removed with it — also an item
//! of OneDrive, also one whose content was not on disk: a row cannot stay in a directory
//! whose row goes. Not the rows that are found at another path: those are moved out.
//! Nothing is decided of a row whose directory could not be read.

use std::collections::HashSet;

use konedrive_tree::{Item, ItemKind, LocalItemId};

use super::backing_directory_contents::{BackingDirectoryContents, EntryIndex};
use super::disk_finding::{compare_content, decide_finding, ContentAgreement, ContentComparison, DiskFinding, ItemBinding, MarkOwner, PathEntry, PathRow, UnresolvedReason};
use super::entry_identities::{EntryIdentities, EntryIdentity};
use super::item_rows::ItemRows;

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
    removed: HashSet<LocalItemId>,
}

/// What the row of `item` says of the path it has.
fn say_of(item: &Item) -> PathRow {
    let binding = if item.onedrive_item.is_some() { ItemBinding::Bound } else { ItemBinding::Unbound };
    match item.kind {
        ItemKind::Directory => PathRow::Directory { binding },
        ItemKind::File => {
            let local_content = item.local.as_ref().and_then(|local| local.content.as_ref());
            let is_synced = item.onedrive_item.is_some() && item.synced_content.is_some() && local_content == item.synced_content.as_ref();
            let agreement = if is_synced { ContentAgreement::Synced } else { ContentAgreement::ChangedHere };
            PathRow::File { binding, content_presence: item.content_presence, agreement }
        }
    }
}

impl PathFindings {
    pub(super) fn decide(rows: &ItemRows, contents: &BackingDirectoryContents, identities: &EntryIdentities) -> Self {
        let mut findings = PathFindings { entries: Vec::with_capacity(contents.len()), rows: Vec::new(), removed: HashSet::new() };
        for index in 0..contents.len() {
            findings.entries.push(Self::decide_entry(rows, contents, identities, index));
        }
        findings.decide_rows_inside(rows, contents, identities, LocalItemId::ROOT, Some(None));
        findings
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
            (_, Some(row)) if identities.find_entry(row.local_item_id).is_none() => PathRow::AnotherItem,
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
        let finding = decide_finding(path_row, path_entry(identities.mark_owner(index), comparison));
        // The file is the row's own by the finding: its content is decided as that row's.
        let content_finding = match (&finding, own) {
            (Ok(DiskFinding::Moved | DiskFinding::MovedOverAnotherItem), Some(own)) if own.kind == ItemKind::File => Some(say_of(own)),
            (Ok(DiskFinding::MarkedWithoutRow), Some(own)) if own.kind == ItemKind::File => match say_of(own) {
                PathRow::File { content_presence, agreement, .. } => Some(PathRow::File { binding: ItemBinding::Bound, content_presence, agreement }),
                said => Some(said),
            },
            _ => None,
        }
        .and_then(|as_own| decide_finding(as_own, path_entry(MarkOwner::SameItem, own.map_or(ContentComparison::Different, compare_with))).ok());
        EntryFinding { finding, content_finding }
    }

    /// Decides the rows in the directory `directory` that no entry is taken as, and those
    /// below them. `on_disk` is where the directory is in the backing directory: `None`
    /// when nothing of it is there.
    fn decide_rows_inside(&mut self, rows: &ItemRows, contents: &BackingDirectoryContents, identities: &EntryIdentities, directory: LocalItemId, on_disk: Option<Option<EntryIndex>>) {
        for row in rows.list_inside(directory) {
            if let Some(entry) = identities.find_entry(row.local_item_id) {
                if row.kind == ItemKind::Directory {
                    self.decide_rows_inside(rows, contents, identities, row.local_item_id, Some(Some(entry)));
                }
                continue;
            }
            let name = row.local.as_ref().and_then(|local| local.place.as_ref()).map(|place| place.name.as_os_str()).unwrap_or_default();
            let path_entry = match on_disk {
                // Whether it is there is not known: it is left as it is, with what is below it.
                Some(on_disk) if contents.is_unread(on_disk) || contents.has_unknown_name(on_disk, name) => continue,
                Some(on_disk) if contents.find_inside(on_disk, name).is_some() => PathEntry::AnotherItem,
                _ => PathEntry::Nothing,
            };
            // Nothing and another item's entry are never left unresolved.
            let finding = decide_finding(say_of(row), path_entry).unwrap_or(DiskFinding::NothingChanged);
            self.rows.push((row.local_item_id, finding));
            match (finding, row.kind) {
                (DiskFinding::Removed, kind) => {
                    self.removed.insert(row.local_item_id);
                    if kind == ItemKind::Directory {
                        self.remove_rows_inside(rows, contents, identities, row.local_item_id);
                    }
                }
                (_, ItemKind::Directory) => self.decide_rows_inside(rows, contents, identities, row.local_item_id, None),
                (_, ItemKind::File) => {}
            }
        }
    }

    /// All the rows say is in the removed directory `directory` is removed with it; what is
    /// found at another path is moved out, and decided where it is.
    fn remove_rows_inside(&mut self, rows: &ItemRows, contents: &BackingDirectoryContents, identities: &EntryIdentities, directory: LocalItemId) {
        for row in rows.list_inside(directory) {
            match identities.find_entry(row.local_item_id) {
                Some(entry) if row.kind == ItemKind::Directory => self.decide_rows_inside(rows, contents, identities, row.local_item_id, Some(Some(entry))),
                Some(_) => {}
                None => {
                    self.rows.push((row.local_item_id, DiskFinding::Removed));
                    self.removed.insert(row.local_item_id);
                    if row.kind == ItemKind::Directory {
                        self.remove_rows_inside(rows, contents, identities, row.local_item_id);
                    }
                }
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

    /// Whether the row of `item` is found removed here.
    pub(super) fn is_removed(&self, item: LocalItemId) -> bool {
        self.removed.contains(&item)
    }
}
