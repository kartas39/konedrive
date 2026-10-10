//! The order in which the findings are written to the rows ([`WriteOrder`]), so that the
//! schema accepts each write: a parent's row before its child's, a name freed before it is
//! taken, a removal deepest first.
//!
//! Each finding is written through an operation of [`ItemStore`], in a transaction of its
//! own: a daemon that dies between two leaves rows a next check finishes. The entries are
//! gone through as they were read, a directory before what is in it. Where the place an
//! item is to have is another item's, that one leaves first: it is moved to where it was
//! found, or removed with what is in it, each by the same steps. Items that wait for each
//! other's places — two exchanged, three or more turned round — are written in one
//! transaction: the two by the store's exchange, the others with the first set aside under
//! a name of the check's own in the root of the folder, which does not outlive the
//! transaction. Only where such a ring goes through a directory that is being removed, or
//! through a directory that is not made yet, is an item set aside by a write of its own.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};

use konedrive_tree::{ContentPresence, ContentVersion, DriveId, ItemKind, ItemPlace, ItemStore, LocalItemId, LocalVersionChange, OneDriveItemId, OneDriveItemReference, TreeError};

use super::backing_directory_contents::{BackingDirectoryContents, EntryIndex};
use super::disk_finding::{DiskFinding, FileFacts, MarkOwner, UnresolvedReason};
use super::entry_identities::{EntryIdentities, EntryIdentity};
use super::item_rows::ItemRows;
use super::local_places::LocalPlaces;
use super::path_findings::PathFindings;
use super::{DiskFindings, UnresolvedPath};
use crate::fuse::backing_directory::BackingDirectory;
use crate::fuse::upload_rules::UploadRules;

/// How far the writing of an entry's row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryProgress {
    Waiting,
    /// Being written: what it waits for is being written first.
    Begun,
    /// Written: the entry has this row.
    Written(LocalItemId),
    /// Left as it was found, with what is below it.
    Left,
}

/// An entry whose row is not written, and whether it will be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnwrittenEntry {
    /// It is left as it was found.
    Left,
    /// The directory it is in is being written further up.
    NotYet,
}

/// What the writes are found from, gathered before the first of them: the rows and the
/// contents as they were read, which item each entry is, and the finding of each path.
pub(super) struct WriteInputs<'a> {
    pub(super) rows: &'a ItemRows,
    pub(super) contents: &'a BackingDirectoryContents,
    pub(super) identities: &'a EntryIdentities,
    pub(super) findings: &'a PathFindings,
}

pub(super) struct WriteOrder<'a, 'store> {
    item_store: &'a mut ItemStore<'store>,
    backing_directory: &'a BackingDirectory,
    upload_rules: &'a UploadRules,
    /// The drive of the folder: of a mark no row has, in a directory that has no binding.
    drive_id: &'a DriveId,
    inputs: WriteInputs<'a>,
    /// Where each item is in the folder, as the rows say it after the writes so far.
    places: LocalPlaces,
    /// The drive of each item that has a binding, after the writes so far.
    drives: HashMap<LocalItemId, DriveId>,
    progress: Vec<EntryProgress>,
    /// The items being moved now, the one that waits for the next before it.
    moving: Vec<LocalItemId>,
    /// The directories being removed now.
    removing: HashSet<LocalItemId>,
    written: DiskFindings,
}

fn refuse(error: TreeError) -> UnresolvedReason {
    UnresolvedReason::Refused(error.to_string())
}

impl<'a, 'store> WriteOrder<'a, 'store> {
    pub(super) fn new(item_store: &'a mut ItemStore<'store>, backing_directory: &'a BackingDirectory, upload_rules: &'a UploadRules, drive_id: &'a DriveId, inputs: WriteInputs<'a>) -> Self {
        let places = LocalPlaces::new(inputs.rows.list());
        let drives = inputs.rows.list().filter_map(|item| Some((item.local_item_id, item.onedrive_item.as_ref()?.drive_id.clone()))).collect();
        let progress = vec![EntryProgress::Waiting; inputs.contents.len()];
        let written = DiskFindings { unresolved: inputs.contents.list_unresolved().to_vec(), ..DiskFindings::default() };
        WriteOrder { item_store, backing_directory, upload_rules, drive_id, inputs, places, drives, progress, moving: Vec::new(), removing: HashSet::new(), written }
    }

    /// Writes every finding, and answers how many of each were written and what was left.
    pub(super) fn write(mut self) -> DiskFindings {
        for index in 0..self.inputs.contents.len() {
            let _ = self.write_entry(index);
        }
        for &(item, finding) in self.inputs.findings.list_rows() {
            let written = match finding {
                // One that went with its directory is gone already.
                DiskFinding::Removed if self.places.find_place(item).is_none() => Ok(()),
                DiskFinding::Removed => self.remove(item),
                DiskFinding::ContentNoLongerOnDisk => self.item_store.set_content_presence(item, ContentPresence::NotOnDisk).map_err(refuse).map(|()| self.written.add(finding)),
                _ => {
                    self.written.add(finding);
                    Ok(())
                }
            };
            if let Err(reason) = written {
                let path = self.item_store.find_path(item).ok().flatten().unwrap_or_default();
                self.written.unresolved.push(UnresolvedPath { path, reason });
            }
        }
        self.written.unresolved.sort_by(|a, b| a.path.cmp(&b.path));
        self.written
    }

    /// Writes the row of the entry `index`, after the row of the directory it is in, and
    /// answers the row the entry has then. An entry that cannot be written is left as it
    /// was found, which is said once, at the path that could not be written, and not of
    /// each entry below it.
    fn write_entry(&mut self, index: EntryIndex) -> Result<LocalItemId, UnwrittenEntry> {
        match self.progress[index] {
            EntryProgress::Written(item) => return Ok(item),
            EntryProgress::Left => return Err(UnwrittenEntry::Left),
            EntryProgress::Begun => return Err(UnwrittenEntry::NotYet),
            EntryProgress::Waiting => {}
        }
        self.progress[index] = EntryProgress::Begun;
        let directory = match self.inputs.contents.parent(index) {
            None => Ok(LocalItemId::ROOT),
            Some(directory) => self.write_entry(directory),
        };
        let written = match directory {
            Ok(directory) => self.write_entry_in(index, directory).map_err(Some),
            // Its directory is being made further up: its turn comes again.
            Err(UnwrittenEntry::NotYet) => {
                self.progress[index] = EntryProgress::Waiting;
                return Err(UnwrittenEntry::NotYet);
            }
            Err(UnwrittenEntry::Left) => Err(None),
        };
        match written {
            Ok(item) => {
                self.progress[index] = EntryProgress::Written(item);
                Ok(item)
            }
            Err(reason) => {
                self.progress[index] = EntryProgress::Left;
                if let Some(reason) = reason {
                    self.written.unresolved.push(UnresolvedPath { path: self.inputs.contents.resolve_path(index), reason });
                }
                Err(UnwrittenEntry::Left)
            }
        }
    }

    fn write_entry_in(&mut self, index: EntryIndex, directory: LocalItemId) -> Result<LocalItemId, UnresolvedReason> {
        let entry = self.inputs.contents.entry(index);
        let found = self.inputs.findings.entry(index);
        let finding = found.finding.clone()?;
        let item = match (finding, self.inputs.identities.identity(index)) {
            (DiskFinding::Made | DiskFinding::MadeWithStaleMark | DiskFinding::MarkedWithoutRow, EntryIdentity::NewItem) => {
                self.free_place(directory, &entry.name)?;
                if finding == DiskFinding::MadeWithStaleMark {
                    self.remove_stale_marks(index)?;
                }
                let item = self.make(index, directory, finding == DiskFinding::Made)?;
                if finding == DiskFinding::MarkedWithoutRow {
                    self.bind(item, directory, index)?;
                }
                item
            }
            (DiskFinding::Moved | DiskFinding::MovedOverAnotherItem, EntryIdentity::Item(item)) => {
                self.move_to(item, directory, &entry.name)?;
                item
            }
            (DiskFinding::MarkedWithoutRow, EntryIdentity::Item(item)) => {
                self.bind(item, directory, index)?;
                item
            }
            (DiskFinding::NothingChanged | DiskFinding::ContentChanged | DiskFinding::CopyLandedBeforeRow | DiskFinding::ContentNoLongerOnDisk, EntryIdentity::Item(item)) => {
                self.write_content(item, finding, &entry.file)?;
                // A mark that is not the row's own does not outlive the check.
                if matches!(self.inputs.identities.mark_owner(index), MarkOwner::CopiedItem | MarkOwner::UnknownItem) {
                    self.remove_stale_marks(index)?;
                }
                item
            }
            (finding, identity) => return Err(UnresolvedReason::Refused(format!("{finding:?} is no finding of an entry that is {identity:?}"))),
        };
        self.written.add(finding);
        if let Some(content_finding) = found.content_finding {
            self.write_content(item, content_finding, &entry.file)?;
            if content_finding != DiskFinding::NothingChanged {
                self.written.add(content_finding);
            }
        }
        Ok(item)
    }

    fn remove_stale_marks(&self, index: EntryIndex) -> Result<(), UnresolvedReason> {
        self.backing_directory.remove_onedrive_marks(&self.inputs.contents.resolve_path(index)).map_err(|e| UnresolvedReason::Unreadable(e.to_string()))
    }

    /// A new item for the entry `index`, at its name in `directory`, its content on disk.
    fn make(&mut self, index: EntryIndex, directory: LocalItemId, is_unmarked: bool) -> Result<LocalItemId, UnresolvedReason> {
        let entry = self.inputs.contents.entry(index);
        let content = (entry.kind == ItemKind::File).then(|| ContentVersion::ChangedHere { size: entry.file.size, mtime: entry.file.mtime, mtime_nsec: entry.file.mtime_nsec, change_count: 1 });
        // A file made where one was removed here, whose removal is not sent, is that file
        // with another content, as the mount's own make has it.
        let replaced = if is_unmarked && entry.kind == ItemKind::File { self.inputs.rows.find_file_removed_at(directory, &entry.name) } else { None };
        let place = ItemPlace { parent_local_item_id: directory, name: entry.name.clone() };
        let is_name_ignored = self.upload_rules.is_name_ignored(&entry.name);
        let item = self.item_store.apply(&LocalVersionChange::Made { kind: entry.kind, place, content, is_name_ignored, replaced }).map_err(refuse)?;
        self.places.set(item, directory, &entry.name);
        if let Some(drive) = replaced.and_then(|replaced| self.drives.get(&replaced).cloned()) {
            self.drives.insert(item, drive);
        }
        Ok(item)
    }

    /// `item`, which is the entry `index`, is the item of OneDrive its mark names: the one
    /// a row the folder does not show has under that id, or one of the drive of the
    /// directory it is in.
    fn bind(&mut self, item: LocalItemId, directory: LocalItemId, index: EntryIndex) -> Result<(), UnresolvedReason> {
        let onedrive_id = self.inputs.contents.entry(index).onedrive_id.as_deref().unwrap_or_default();
        let reference = match self.inputs.rows.find_unshown_by_onedrive_id(onedrive_id).and_then(|unshown| unshown.onedrive_item.clone()) {
            Some(reference) => reference,
            None => OneDriveItemReference { drive_id: self.drives.get(&directory).unwrap_or(self.drive_id).clone(), item_id: OneDriveItemId::new(onedrive_id) },
        };
        self.item_store.bind_item(item, &reference).map_err(refuse)?;
        self.drives.insert(item, reference.drive_id);
        Ok(())
    }

    /// What a finding of a file's content writes to the row of `item`.
    fn write_content(&mut self, item: LocalItemId, finding: DiskFinding, file: &FileFacts) -> Result<(), UnresolvedReason> {
        let row = self.inputs.rows.find(item);
        let was_open_for_writing = row.is_some_and(|row| row.content_before_write.is_some());
        match finding {
            // A daemon died with a writer open that wrote nothing: as its close leaves it.
            DiskFinding::NothingChanged if was_open_for_writing => self.item_store.apply(&LocalVersionChange::ClosedUnchanged { item }).map(|_| ()).map_err(refuse),
            DiskFinding::ContentChanged => {
                // Another content, its count raised: by the open that the dead daemon wrote,
                // or by one written here with the close.
                let closed = LocalVersionChange::ClosedChanged { item, size: file.size, mtime: file.mtime, mtime_nsec: file.mtime_nsec };
                let changes = if was_open_for_writing { vec![closed] } else { vec![LocalVersionChange::OpenedForWriting { item }, closed] };
                self.item_store.apply_all(&changes).map_err(refuse)?;
                if row.is_some_and(|row| row.content_presence == ContentPresence::NotOnDisk) {
                    self.item_store.set_content_presence(item, ContentPresence::OnDisk).map_err(refuse)?;
                }
                Ok(())
            }
            DiskFinding::CopyLandedBeforeRow => self.item_store.set_content_presence(item, ContentPresence::OnDisk).map_err(refuse),
            DiskFinding::ContentNoLongerOnDisk => self.item_store.set_content_presence(item, ContentPresence::NotOnDisk).map_err(refuse),
            _ => Ok(()),
        }
    }

    /// The place `name` in `directory` is nobody's: the item that has it has left.
    fn free_place(&mut self, directory: LocalItemId, name: &OsStr) -> Result<(), UnresolvedReason> {
        let Some(holder) = self.places.find_at(directory, name) else { return Ok(()) };
        self.make_leave(holder)?;
        match self.places.find_at(directory, name) {
            Some(_) => Err(UnresolvedReason::Blocked),
            None => Ok(()),
        }
    }

    /// `item` is at `name` in `directory`.
    fn move_to(&mut self, item: LocalItemId, directory: LocalItemId, name: &OsStr) -> Result<(), UnresolvedReason> {
        if self.places.is_at(item, directory, name) {
            return Ok(());
        }
        self.moving.push(item);
        let freed = match self.places.find_at(directory, name) {
            Some(holder) => self.make_leave(holder),
            None => Ok(()),
        };
        self.moving.pop();
        freed?;
        // A ring of moves wrote this one with the others.
        if self.places.is_at(item, directory, name) {
            return Ok(());
        }
        if self.places.find_at(directory, name).is_some() {
            return Err(UnresolvedReason::Blocked);
        }
        let place = ItemPlace { parent_local_item_id: directory, name: name.to_owned() };
        let is_name_ignored = self.upload_rules.is_name_ignored(name);
        self.item_store.apply(&LocalVersionChange::Moved { item, place, is_name_ignored }).map_err(refuse)?;
        self.places.set(item, directory, name);
        Ok(())
    }

    /// `item` leaves the place it has, which another item is to take: it is moved to where
    /// it was found, or removed with what is in it.
    fn make_leave(&mut self, item: LocalItemId) -> Result<(), UnresolvedReason> {
        let entry = self.inputs.identities.find_entry(item);
        let is_begun = self.removing.contains(&item) || entry.is_some_and(|entry| self.progress[entry] == EntryProgress::Begun);
        if is_begun {
            // It waits, further up, for what is being written now.
            return self.write_ring(item);
        }
        match entry {
            Some(entry) => match self.write_entry(entry) {
                Ok(_) => Ok(()),
                // The directory it is to be in is not made yet, and waits for this: it is
                // set aside, and moved into the directory when its turn comes.
                Err(UnwrittenEntry::NotYet) => self.set_aside(item),
                Err(UnwrittenEntry::Left) => Err(UnresolvedReason::Blocked),
            },
            None if self.inputs.findings.is_removed(item) => self.remove(item),
            None => Err(UnresolvedReason::Blocked),
        }
    }

    /// Removes `item` here, after all that is in it has left.
    fn remove(&mut self, item: LocalItemId) -> Result<(), UnresolvedReason> {
        self.removing.insert(item);
        let emptied = self.places.list_inside(item).into_iter().try_for_each(|inside| self.make_leave(inside));
        self.removing.remove(&item);
        emptied.map_err(|_| UnresolvedReason::Blocked)?;
        self.item_store.apply(&LocalVersionChange::Removed { item }).map_err(refuse)?;
        self.places.remove(item);
        self.written.add(DiskFinding::Removed);
        Ok(())
    }

    /// Where `item` is to be: the name of the entry it is, in the row of that entry's
    /// directory. `None` while that directory has no row.
    fn find_target(&self, item: LocalItemId) -> Option<(LocalItemId, &'a OsStr)> {
        let entry = self.inputs.identities.find_entry(item)?;
        let directory = match self.inputs.contents.parent(entry) {
            None => LocalItemId::ROOT,
            Some(directory) => match self.progress[directory] {
                EntryProgress::Written(directory) => directory,
                _ => return None,
            },
        };
        Some((directory, self.inputs.contents.entry(entry).name.as_os_str()))
    }

    /// `first`, whose move or removal was begun further up and waits for what is being
    /// written now, leaves its place. When it is the first of a ring of moves, each item to
    /// the place of the next and the last to the first's, the ring is written whole, in one
    /// transaction. Otherwise the item is set aside by a write of its own, and goes on from
    /// there when its turn comes.
    fn write_ring(&mut self, first: LocalItemId) -> Result<(), UnresolvedReason> {
        let ring: Vec<LocalItemId> = match self.moving.iter().position(|moved| *moved == first) {
            Some(at) => self.moving[at..].to_vec(),
            None => Vec::new(),
        };
        let targets: Option<Vec<(LocalItemId, &OsStr)>> = ring.iter().map(|item| self.find_target(*item)).collect();
        let is_ring = targets.as_ref().is_some_and(|targets| {
            let takes_the_next_ones_place = |at: usize| self.places.is_at(ring[(at + 1) % ring.len()], targets[at].0, targets[at].1);
            ring.len() > 1 && (0..ring.len()).all(takes_the_next_ones_place)
        });
        let (Some(targets), true) = (targets, is_ring) else { return self.set_aside(first) };
        if let [first, second] = ring[..] {
            self.item_store.exchange_local_places(first, second).map_err(refuse)?;
        } else {
            // The first steps aside, the last takes its place, and so on back to the first.
            let mut changes = vec![self.move_aside(first)];
            for at in (0..ring.len()).rev() {
                let place = ItemPlace { parent_local_item_id: targets[at].0, name: targets[at].1.to_owned() };
                changes.push(LocalVersionChange::Moved { item: ring[at], place, is_name_ignored: self.upload_rules.is_name_ignored(targets[at].1) });
            }
            self.item_store.apply_all(&changes).map_err(refuse)?;
        }
        for (item, (directory, name)) in ring.iter().zip(targets) {
            self.places.set(*item, directory, name);
        }
        Ok(())
    }

    /// The change that sets `item` aside: to a name in the root of the folder that no row
    /// and no entry of the backing directory has.
    fn move_aside(&self, item: LocalItemId) -> LocalVersionChange {
        let mut name = OsString::from(format!(".konedrive-check-{item}"));
        while self.places.find_at(LocalItemId::ROOT, &name).is_some() || self.inputs.contents.find_inside(None, &name).is_some() {
            name.push("-");
        }
        LocalVersionChange::Moved { item, place: ItemPlace { parent_local_item_id: LocalItemId::ROOT, name }, is_name_ignored: false }
    }

    fn set_aside(&mut self, item: LocalItemId) -> Result<(), UnresolvedReason> {
        let aside = self.move_aside(item);
        self.item_store.apply(&aside).map_err(refuse)?;
        if let LocalVersionChange::Moved { place, .. } = aside {
            self.places.set(item, LocalItemId::ROOT, &place.name);
        }
        Ok(())
    }
}
