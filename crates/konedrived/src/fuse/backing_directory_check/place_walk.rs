//! The walk over the places of the folder ([`PlaceWalk`]): each directory's places, a
//! level of directories at a time, a directory before what is in it; at each place the one
//! decision ([`decide_finding`]) of what the rows say there and what the disk has there,
//! and its finding written.
//!
//! A place is a directory and a name. What is decided of it comes from that place alone:
//! the row the folder shows there, the entry the backing directory has there. No row is
//! looked for by a mark, and no entry by a row: of a mark the walk asks only whether the
//! id is the one of the row at that very place, and otherwise whether any row has it, and
//! whether more than one entry carries it — an id that several entries carry binds none
//! of them: each is a new item of the user's own.
//!
//! **State owned:** the directories still to be gone through, the OneDrive item ids the
//! walk itself bound, and what was found and left so far.
//!
//! **The order of the writes** needs no search, because no row is moved. At a place: a row
//! that leaves the folder leaves first, with what the rows say is below it, deepest first;
//! then the entry there, when no row has the place now, is made a new item; and what is in
//! a directory is gone through after the directory's own place. So a parent's row is
//! there before its child's is made, and a name is free before it is taken.
//!
//! **What is left as it is:** a directory that was not read, with every row the folder
//! shows in it and below it; the row at a name that could not be looked at; the rows and
//! the places a place change that stands on record names. Each is held and said, and
//! nothing else is held for them.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use konedrive_tree::{ContentPresence, DriveId, Item, ItemKind, LocalItemId};

use super::backing_directory_contents::{BackingDirectoryContents, BackingDirectoryEntry, EntryIndex};
use super::disk_finding::{compare_content, decide_finding, decide_finding_without_place, ContentComparison, DiskFinding, EntryMark, ItemBinding, PlaceEntry, PlaceRow, UnresolvedReason, WriterAtStop};
use super::item_rows::ItemRows;
use super::mark_change::{decide_mark_change, EntryOwner, MarkChange};
use super::row_writer::{tell_made_content, RowWriter};
use super::standing_place_changes::StandingPlaceChanges;
use super::DiskFindings;
use crate::fuse::backing_directory::BackingDirectory;

/// Where a directory of the rows is on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiskDirectory {
    /// The backing directory itself.
    Root,
    Entry(EntryIndex),
    /// A directory of OneDrive that has none on disk: nothing is at any place in it.
    NotOnDisk,
}

/// A directory whose places are still to be gone through.
struct DirectoryVisit {
    item: LocalItemId,
    disk: DiskDirectory,
    /// Relative to the backing directory.
    path: PathBuf,
    /// The drive of the nearest directory of OneDrive from this one up, or the folder's
    /// own: the drive an entry in it is bound in when its mark names an id no row has.
    drive: DriveId,
}

/// What the rows say of the place of `row`.
fn tell_place_row(row: &Item) -> PlaceRow {
    let binding = tell_binding(row);
    match row.kind {
        ItemKind::File => PlaceRow::File { binding, content_presence: row.content_presence, writer: tell_writer_at_stop(row) },
        ItemKind::Directory => PlaceRow::Directory { binding },
    }
}

/// Whether a writer held the file of `row` open when the daemon stopped: only a last
/// close takes the content of before the write from a row.
fn tell_writer_at_stop(row: &Item) -> WriterAtStop {
    match row.content_before_write {
        Some(_) => WriterAtStop::Open,
        None => WriterAtStop::Closed,
    }
}

fn tell_binding(row: &Item) -> ItemBinding {
    if row.onedrive_item.is_some() {
        ItemBinding::Bound
    } else {
        ItemBinding::Unbound
    }
}

pub(super) struct PlaceWalk<'a, 'store> {
    /// The marks of an entry are written and taken off through it.
    backing_directory: &'a BackingDirectory,
    /// The drive of the folder.
    drive_id: &'a DriveId,
    rows: &'a ItemRows,
    contents: &'a BackingDirectoryContents,
    standing_place_changes: &'a StandingPlaceChanges,
    writer: RowWriter<'a, 'store>,
    /// The OneDrive item ids this walk bound an entry to: a row has each of them now.
    bound_onedrive_ids: HashSet<String>,
    outcome: DiskFindings,
}

impl<'a, 'store> PlaceWalk<'a, 'store> {
    pub(super) fn new(
        backing_directory: &'a BackingDirectory,
        drive_id: &'a DriveId,
        rows: &'a ItemRows,
        contents: &'a BackingDirectoryContents,
        standing_place_changes: &'a StandingPlaceChanges,
        writer: RowWriter<'a, 'store>,
    ) -> Self {
        let unresolved = contents.list_unresolved().iter().chain(standing_place_changes.list_unresolved()).cloned().collect();
        let outcome = DiskFindings::begin(unresolved, contents.has_unread_part() || !standing_place_changes.is_settled());
        PlaceWalk { backing_directory, drive_id, rows, contents, standing_place_changes, writer, bound_onedrive_ids: HashSet::new(), outcome }
    }

    /// Goes through every place, writes what is found, and answers how many of each
    /// finding were written and what was left.
    pub(super) fn walk(mut self) -> DiskFindings {
        let mut level = vec![DirectoryVisit { item: LocalItemId::ROOT, disk: DiskDirectory::Root, path: PathBuf::new(), drive: self.drive_id.clone() }];
        while !level.is_empty() {
            let mut next_level = Vec::new();
            for directory in &level {
                self.visit(directory, &mut next_level);
            }
            level = next_level;
        }
        self.outcome.finish()
    }

    /// Goes through the places of one directory: every name the rows show in it or the
    /// disk has in it, by name.
    fn visit(&mut self, directory: &DirectoryVisit, next_level: &mut Vec<DirectoryVisit>) {
        let on_disk = match directory.disk {
            DiskDirectory::Root => Some(None),
            DiskDirectory::Entry(entry) => Some(Some(entry)),
            DiskDirectory::NotOnDisk => None,
        };
        // What is in a directory that was not read is not known: every row in it and below
        // it is left as it is, by the directory's own row.
        if on_disk.is_some_and(|on_disk| self.contents.is_unread(on_disk)) {
            self.outcome.hold(directory.item);
            return;
        }
        let (rows, contents) = (self.rows, self.contents);
        let mut places: BTreeMap<&OsStr, (Option<&Item>, Option<EntryIndex>)> = BTreeMap::new();
        for row in rows.list_inside(directory.item) {
            let Some(place) = row.local.as_ref().and_then(|local| local.place.as_ref()) else { continue };
            places.entry(place.name.as_os_str()).or_default().0 = Some(row);
        }
        for entry in on_disk.map_or(0..0, |on_disk| contents.list_inside(on_disk)) {
            places.entry(contents.entry(entry).name.as_os_str()).or_default().1 = Some(entry);
        }
        for (name, (row, entry)) in places {
            // A place that could not be looked at, or that a place change on record names:
            // the row there is left as it is, and what is below it with it.
            let is_not_looked_at = on_disk.is_some_and(|on_disk| contents.has_unknown_name(on_disk, name));
            let is_named_by_record = self.standing_place_changes.names_place(directory.item, name) || row.is_some_and(|row| self.standing_place_changes.names_item(row.local_item_id));
            if is_not_looked_at || is_named_by_record {
                if let Some(row) = row {
                    self.outcome.hold(row.local_item_id);
                }
                continue;
            }
            self.decide_place(directory, name, row, entry, next_level);
        }
    }

    /// Decides one place and writes its finding.
    fn decide_place(&mut self, directory: &DirectoryVisit, name: &OsStr, row: Option<&'a Item>, entry: Option<EntryIndex>, next_level: &mut Vec<DirectoryVisit>) {
        let path = directory.path.join(name);
        let Some(row) = row else {
            if let Some(entry) = entry {
                self.make_item(directory, entry, path, next_level);
            }
            return;
        };
        let place_entry = entry.map_or(PlaceEntry::Nothing, |entry| self.tell_place_entry(Some(row), self.contents.entry(entry)));
        let finding = decide_finding(tell_place_row(row), place_entry);
        match (finding, row.kind, entry) {
            // Nothing of the item is at its place, or an entry of the other kind is: the
            // row leaves, and what is there is an entry no row has.
            (DiskFinding::NotShownAnyMore | DiskFinding::Removed, _, _) => match (self.take_from_folder(row, finding, &path), entry) {
                (true, Some(entry)) => self.make_item(directory, entry, path, next_level),
                (false, Some(_)) => self.outcome.leave(path, UnresolvedReason::Blocked),
                (_, None) => {}
            },
            (_, ItemKind::File, Some(entry)) => self.write_file(row, entry, finding, &path),
            (_, ItemKind::File, None) => self.outcome.add(finding),
            (_, ItemKind::Directory, entry) => {
                self.outcome.add(finding);
                if let Some(entry) = entry {
                    self.change_marks(row, self.contents.entry(entry), &path);
                }
                let drive = match &row.onedrive_item {
                    Some(reference) => reference.drive_id.clone(),
                    None => directory.drive.clone(),
                };
                next_level.push(DirectoryVisit { item: row.local_item_id, disk: entry.map_or(DiskDirectory::NotOnDisk, DiskDirectory::Entry), path, drive });
            }
        }
    }

    /// What the disk has at a place, for the decision: `row` is the row at that place.
    fn tell_place_entry(&self, row: Option<&Item>, entry: &BackingDirectoryEntry) -> PlaceEntry {
        let mark = self.tell_mark(row, entry);
        match entry.kind {
            ItemKind::Directory => PlaceEntry::Directory { mark },
            ItemKind::File => {
                let row_content = row.and_then(|row| row.local.as_ref()).and_then(|local| local.content.as_ref());
                let comparison = row_content.map_or(ContentComparison::Different, |content| compare_content(content, &entry.file, entry.links));
                PlaceEntry::File { mark, wholeness: entry.wholeness, comparison }
            }
        }
    }

    /// Whose the id is that `entry` is marked with, as far as its own place tells: the one
    /// of `row`, the row at its place; of no row; or of some row.
    fn tell_mark(&self, row: Option<&Item>, entry: &BackingDirectoryEntry) -> EntryMark {
        let Some(onedrive_id) = entry.onedrive_id.as_deref() else { return EntryMark::Unmarked };
        let is_of_row_at_place = row.and_then(|row| row.onedrive_item.as_ref()).is_some_and(|reference| reference.item_id.as_str() == onedrive_id);
        if is_of_row_at_place {
            EntryMark::OfRowAtPlace
        } else if self.rows.has_onedrive_id(onedrive_id) || self.bound_onedrive_ids.contains(onedrive_id) {
            EntryMark::OfAnotherRow
        } else if self.contents.count_entries_marked(onedrive_id) > 1 {
            EntryMark::OfSeveralEntries
        } else {
            EntryMark::OfNoRow
        }
    }

    /// The file `entry` is the item of `row`: what was found of its content is written,
    /// and then its marks.
    fn write_file(&mut self, row: &Item, entry: EntryIndex, finding: DiskFinding, path: &Path) {
        let entry = self.contents.entry(entry);
        let (was_open_for_writing, was_on_disk) = (row.content_before_write.is_some(), row.content_presence == ContentPresence::OnDisk);
        if let Err(reason) = self.writer.write_content(row.local_item_id, finding, &entry.file, was_open_for_writing, was_on_disk) {
            self.outcome.leave(path.to_owned(), reason);
            self.outcome.hold(row.local_item_id);
            return;
        }
        self.outcome.add(finding);
        // The row first: a mark says nothing of a content the row does not have yet.
        self.change_marks(row, entry, path);
    }

    /// The marks of `entry`, which is the own entry of the item of `row`.
    fn change_marks(&mut self, row: &Item, entry: &BackingDirectoryEntry, path: &Path) {
        let (owner, onedrive_id) = match &row.onedrive_item {
            Some(reference) => (EntryOwner::BoundRow, Some(reference.item_id.as_str())),
            None => (EntryOwner::UnboundRow, None),
        };
        self.write_mark_change(decide_mark_change(owner, self.tell_mark(Some(row), entry), entry.links), onedrive_id, path);
    }

    fn write_mark_change(&mut self, change: MarkChange, onedrive_id: Option<&str>, path: &Path) {
        let written = match (change, onedrive_id) {
            (MarkChange::Nothing, _) => Ok(()),
            (MarkChange::WriteId, Some(onedrive_id)) => self.backing_directory.write_onedrive_id_mark(path, onedrive_id),
            (MarkChange::ReplaceWithId, Some(onedrive_id)) => self.backing_directory.remove_onedrive_marks(path).and_then(|()| self.backing_directory.write_onedrive_id_mark(path, onedrive_id)),
            (MarkChange::TakeOff, _) => self.backing_directory.remove_onedrive_marks(path),
            // An id is written for an item that has one.
            (MarkChange::WriteId | MarkChange::ReplaceWithId, None) => Ok(()),
        };
        if let Err(e) = written {
            self.outcome.leave(path.to_owned(), UnresolvedReason::Unreadable(e.to_string()));
        }
    }

    /// The entry `entry`, at a place no row has, is a new item: of the user's own, or
    /// bound to the id of its mark when no row has that id.
    fn make_item(&mut self, directory: &DirectoryVisit, entry: EntryIndex, path: PathBuf, next_level: &mut Vec<DirectoryVisit>) {
        // A directory that was not read is left as it is, with no row: it is said already.
        if self.contents.is_unread(Some(entry)) {
            return;
        }
        let made = self.contents.entry(entry);
        let mark = self.tell_mark(None, made);
        let finding = decide_finding(PlaceRow::Nothing, self.tell_place_entry(None, made));
        let item = match self.writer.make(directory.item, made, tell_made_content(made, finding)) {
            Ok(item) => item,
            Err(reason) => return self.outcome.leave(path, reason),
        };
        let owner = match (finding, made.onedrive_id.as_deref()) {
            (DiskFinding::MarkedWithoutRow, Some(onedrive_id)) => {
                match self.writer.bind(item, &directory.drive, onedrive_id) {
                    Ok(()) => {
                        self.bound_onedrive_ids.insert(onedrive_id.to_owned());
                    }
                    // It stays an item of the user's own, and keeps its mark.
                    Err(reason) => self.outcome.leave(path.clone(), reason),
                }
                EntryOwner::NewItemBoundByMark
            }
            _ => EntryOwner::NewItem,
        };
        self.outcome.add(finding);
        self.write_mark_change(decide_mark_change(owner, mark, made.links), None, &path);
        if made.kind == ItemKind::Directory {
            // What is in it is in the drive it was bound in.
            next_level.push(DirectoryVisit { item, disk: DiskDirectory::Entry(entry), path, drive: directory.drive.clone() });
        }
    }

    /// The item of `row` leaves the folder as `finding` says, after everything the rows say
    /// is below it, deepest first: each as the decision of a row with no place finds it,
    /// an item of OneDrive not shown any more and one OneDrive never had gone. Whether it
    /// left: a row below it that is left as it is, or whose write is refused, keeps it.
    fn take_from_folder(&mut self, row: &Item, finding: DiskFinding, path: &Path) -> bool {
        let mut is_empty = true;
        for inside in self.rows.list_inside(row.local_item_id) {
            let Some(place) = inside.local.as_ref().and_then(|local| local.place.as_ref()) else { continue };
            let inside_path = path.join(&place.name);
            if self.standing_place_changes.names_item(inside.local_item_id) || self.standing_place_changes.names_place(row.local_item_id, &place.name) {
                self.outcome.hold(inside.local_item_id);
                is_empty = false;
            } else if !self.take_from_folder(inside, decide_finding_without_place(tell_place_row(inside)), &inside_path) {
                is_empty = false;
            }
        }
        if !is_empty {
            self.outcome.leave(path.to_owned(), UnresolvedReason::Blocked);
            self.outcome.hold(row.local_item_id);
            return false;
        }
        match self.writer.take_from_folder(row.local_item_id, finding, tell_binding(row)) {
            Ok(()) => {
                self.outcome.add(finding);
                true
            }
            Err(reason) => {
                self.outcome.leave(path.to_owned(), reason);
                self.outcome.hold(row.local_item_id);
                false
            }
        }
    }
}
