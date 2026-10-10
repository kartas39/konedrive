//! The order in which the findings are written to the rows ([`WriteOrder`]), so that the
//! schema accepts each write: a parent's row before its child's, a name freed before it is
//! taken, what leaves the folder deepest first.
//!
//! What is written for a finding is [`RowWriter`]'s; here is which write waits for which.
//! The entries are gone through as they were read, a directory before what is in it. Where
//! the place an item is to have is another item's, that one leaves first: it is moved to
//! where it was found, or leaves the folder with what is in it — removed, or its place
//! lost, as it was found — each by the same steps. Items that wait for each other's places
//! — two exchanged, three or more turned round — are written in one transaction. Only where
//! such a ring goes through a directory that is leaving the folder, or through a directory
//! that is not made yet, is an item set aside by a write of its own, under a name of the
//! daemon's own in the root of the folder; a next check moves it on from there, or takes
//! its row from the folder.
//!
//! A row that is left as it is — it may be in a part of the backing directory that was not
//! read, or be an entry that was left — is not written, and neither is what waits for its
//! place: both are said, and the rows among them are held ([`DiskFindings::list_held_items`]).
//! A directory leaves the folder with every row that is to leave with it, or with none:
//! what is found at another path is moved out of it first, and where that cannot be
//! written, or a row below it is left as it is, the directory stays whole. An item whose
//! entry could not be written keeps its row where it was, and that path is said too.
//!
//! An entry that is taken as the own entry of an item OneDrive has and does not carry that
//! item's id is marked with it, after its row is written: a later move of it by hand is
//! followed then. A death between the two is finished by the next check, which finds the
//! row as it was written and the entry unmarked.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};

use konedrive_tree::{ContentPresence, DriveId, ItemKind, ItemStore, LocalItemId};

use super::backing_directory_contents::{BackingDirectoryContents, EntryIndex};
use super::disk_finding::{DiskContent, DiskFinding, MarkOwner, UnresolvedReason};
use super::entry_identities::{EntryIdentities, EntryIdentity};
use super::item_rows::{BoundRows, ItemRows};
use super::path_findings::PathFindings;
use super::row_writer::{tell_made_content, RowWriter};
use super::DiskFindings;
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
    DirectoryUnwritten,
}

pub(super) struct WriteOrder<'a, 'store> {
    /// The marks of an entry that are not its row's own are taken off through it, and the
    /// id of its item is written on an entry that does not carry it.
    backing_directory: &'a BackingDirectory,
    rows: &'a ItemRows,
    contents: &'a BackingDirectoryContents,
    identities: &'a EntryIdentities,
    findings: &'a PathFindings,
    /// The rows as written so far, and what writes them.
    written: RowWriter<'a, 'store>,
    progress: Vec<EntryProgress>,
    /// The items being moved now, the one that waits for the next before it.
    moving: Vec<LocalItemId>,
    /// The directories that are leaving the folder now: what is in them leaves first.
    leaving: HashSet<LocalItemId>,
    /// The items that were set aside by a write of their own, each with the name it was
    /// given in the root of the folder.
    set_aside: Vec<(LocalItemId, OsString)>,
    /// The entries that were left and that are an item with a row, each with that item.
    unwritten_items: Vec<(EntryIndex, LocalItemId)>,
    /// What was written so far, and what was left.
    outcome: DiskFindings,
}

impl<'a, 'store> WriteOrder<'a, 'store> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        item_store: &'a mut ItemStore<'store>,
        backing_directory: &'a BackingDirectory,
        upload_rules: &'a UploadRules,
        drive_id: &'a DriveId,
        rows: &'a ItemRows,
        contents: &'a BackingDirectoryContents,
        identities: &'a EntryIdentities,
        findings: &'a PathFindings,
    ) -> Self {
        let written = RowWriter::new(item_store, upload_rules, drive_id, rows);
        let progress = vec![EntryProgress::Waiting; contents.len()];
        let outcome = DiskFindings::begin(contents.list_unresolved().to_vec(), findings.has_unread_part());
        WriteOrder { backing_directory, rows, contents, identities, findings, written, progress, moving: Vec::new(), leaving: HashSet::new(), set_aside: Vec::new(), unwritten_items: Vec::new(), outcome }
    }

    /// Writes every finding, and answers how many of each were written and what was left.
    pub(super) fn write(mut self) -> DiskFindings {
        for index in 0..self.contents.len() {
            let _ = self.write_entry(index);
        }
        // The directories that were to leave the folder and could not: what the rows say is
        // in them stays with them, and is said with them.
        let mut staying: HashSet<LocalItemId> = HashSet::new();
        for &(item, finding) in self.findings.list_rows() {
            let directory = self.written.places().find_place(item).map(|(directory, _)| directory);
            let written = match finding {
                // One that went with its directory, or made way for an entry, is gone already.
                DiskFinding::Removed | DiskFinding::PlaceLost if directory.is_none() => Ok(()),
                DiskFinding::Removed | DiskFinding::PlaceLost if directory.is_some_and(|directory| staying.contains(&directory)) => {
                    staying.insert(item);
                    Ok(())
                }
                DiskFinding::Removed | DiskFinding::PlaceLost => self.take_from_folder(item, finding).inspect_err(|_| {
                    staying.insert(item);
                }),
                DiskFinding::ContentNoLongerOnDisk => self.written.write_content(item, finding, &DiskContent::default(), false, true).map(|()| self.outcome.add(finding)),
                _ => {
                    self.outcome.add(finding);
                    Ok(())
                }
            };
            if let Err(reason) = written {
                self.outcome.leave(self.written.find_path(item), reason);
                self.outcome.hold(item);
            }
        }
        // What was left as it is keeps its row, and is said with why.
        for (item, reason) in self.findings.list_held_back() {
            self.outcome.leave(self.written.find_path(*item), reason.clone());
        }
        for item in self.findings.list_left_items() {
            self.outcome.hold(item);
        }
        // An item whose entry was left has its row where it was: at another path than the
        // entry, where the disk does not have what the row says.
        for (index, item) in &self.unwritten_items {
            let row_path = self.written.find_path(*item);
            if row_path != self.contents.resolve_path(*index) && !row_path.as_os_str().is_empty() {
                self.outcome.leave(row_path, UnresolvedReason::Blocked);
            }
        }
        // An item that was set aside and could not be moved on has a row at a name the disk
        // does not have: it is said, and a next check finds the item by its mark.
        for (item, name) in &self.set_aside {
            if self.written.places().is_at(*item, LocalItemId::ROOT, name) {
                self.outcome.leave(name.into(), UnresolvedReason::Blocked);
            }
        }
        self.outcome.finish()
    }

    /// Writes the row of the entry `index`, after the row of the directory it is in, and
    /// answers the row the entry has then. An entry that cannot be written is left as it
    /// was found, which is said once, at the path that could not be written, and not of
    /// each entry below it.
    fn write_entry(&mut self, index: EntryIndex) -> Result<LocalItemId, UnwrittenEntry> {
        match self.progress[index] {
            EntryProgress::Written(item) => return Ok(item),
            EntryProgress::Left => return Err(UnwrittenEntry::Left),
            EntryProgress::Begun => return Err(UnwrittenEntry::DirectoryUnwritten),
            EntryProgress::Waiting => {}
        }
        self.progress[index] = EntryProgress::Begun;
        let directory = match self.contents.parent(index) {
            None => Ok(LocalItemId::ROOT),
            Some(directory) => self.write_entry(directory),
        };
        let written = match directory {
            Ok(directory) => self.write_entry_in(index, directory).map_err(Some),
            // Its directory is being made further up: its turn comes again.
            Err(UnwrittenEntry::DirectoryUnwritten) => {
                self.progress[index] = EntryProgress::Waiting;
                return Err(UnwrittenEntry::DirectoryUnwritten);
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
                self.leave_entry(index, reason);
                Err(UnwrittenEntry::Left)
            }
        }
    }

    /// The entry `index` is left as it was found: for `reason`, which is said at its path,
    /// or with the directory it is in, which was said. The row the entry is, when it has
    /// one, is held; where it is at another path than the entry once all is written, that
    /// path is said too ([`write`](Self::write)).
    fn leave_entry(&mut self, index: EntryIndex, reason: Option<UnresolvedReason>) {
        let item = self.identities.find_item(index);
        let is_left_with_its_directory = reason.is_none();
        if let Some(reason) = reason {
            self.outcome.leave(self.contents.resolve_path(index), reason);
        }
        let Some(item) = item else { return };
        // In a directory that was left, a row that is where the entry is in it is left
        // with that directory's.
        let directory = self.contents.parent(index).and_then(|directory| self.identities.find_item(directory));
        let is_in_its_directory = directory.is_some_and(|directory| self.written.places().is_at(item, directory, &self.contents.entry(index).name));
        if is_left_with_its_directory && is_in_its_directory {
            return;
        }
        self.outcome.hold(item);
        self.unwritten_items.push((index, item));
    }

    fn write_entry_in(&mut self, index: EntryIndex, directory: LocalItemId) -> Result<LocalItemId, UnresolvedReason> {
        let entry = self.contents.entry(index);
        let found = self.findings.entry(index);
        let finding = found.finding.clone()?;
        let item = match (finding, self.identities.identity(index)) {
            (DiskFinding::Made | DiskFinding::MadeWithStaleMark | DiskFinding::MarkedWithoutRow, EntryIdentity::NewItem) => {
                self.free_place(directory, &entry.name)?;
                if finding == DiskFinding::MadeWithStaleMark {
                    self.remove_stale_marks(index)?;
                }
                // A file made where one was removed here, whose removal is not sent, is that
                // file with another content, as the mount's own make has it.
                let replaced = if finding == DiskFinding::Made && entry.kind == ItemKind::File { self.rows.find_file_removed_at(directory, &entry.name) } else { None };
                let item = self.written.make(directory, entry, tell_made_content(entry, finding), replaced)?;
                if finding == DiskFinding::MarkedWithoutRow {
                    self.bind(item, directory, index)?;
                }
                // The file that was removed here is this one now: its binding went over.
                if let Some(replaced) = replaced {
                    self.mark_as_own_entry(index, replaced)?;
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
                // The row first: a mark on an entry whose row does not say yet that its
                // content is here alone would say the content is OneDrive's.
                self.write_content(item, finding, &entry.file)?;
                match self.identities.mark_owner(index) {
                    // A mark that is not the row's own does not outlive the check.
                    MarkOwner::AnotherItem | MarkOwner::NoRow => {
                        self.remove_stale_marks(index)?;
                        self.mark_as_own_entry(index, item)?;
                    }
                    MarkOwner::Unmarked => self.mark_as_own_entry(index, item)?,
                    MarkOwner::RowAtPath | MarkOwner::RowElsewhere => {}
                }
                item
            }
            (finding, identity) => return Err(UnresolvedReason::Refused(format!("{finding:?} is no finding of an entry that is {identity:?}"))),
        };
        self.outcome.add(finding);
        if let Some(content_finding) = found.content_finding {
            self.write_content(item, content_finding, &entry.file)?;
            if content_finding != DiskFinding::NothingChanged {
                self.outcome.add(content_finding);
            }
        }
        Ok(item)
    }

    /// `item`, which is the entry `index` in `directory`, is the item of OneDrive its mark
    /// names: the one of the row that has that id, a row the folder does not show; or,
    /// when no row has the id, an item of the drive of the directory it is in.
    fn bind(&mut self, item: LocalItemId, directory: LocalItemId, index: EntryIndex) -> Result<(), UnresolvedReason> {
        let onedrive_id = self.contents.entry(index).onedrive_id.as_deref().unwrap_or_default();
        match self.rows.find_bound(onedrive_id) {
            BoundRows::One(row) => match &row.onedrive_item {
                Some(reference) => self.written.bind(item, reference),
                None => Err(UnresolvedReason::Blocked),
            },
            BoundRows::None => self.written.bind_in_drive_of_directory(item, directory, onedrive_id),
            // Never bound: which of them is meant the mark does not say.
            BoundRows::OfTwoDrives => Err(UnresolvedReason::ItemIdInTwoDrives),
        }
    }

    /// The entry `index` is the own entry of `item`, as the rows had it when they were
    /// read: when that is an item OneDrive has, the entry is marked with its id, and with
    /// nothing else. An entry of an item OneDrive does not have carries no mark.
    fn mark_as_own_entry(&self, index: EntryIndex, item: LocalItemId) -> Result<(), UnresolvedReason> {
        let Some(reference) = self.rows.find(item).and_then(|row| row.onedrive_item.as_ref()) else { return Ok(()) };
        self.backing_directory.write_onedrive_id_mark(&self.contents.resolve_path(index), reference.item_id.as_str()).map_err(|e| UnresolvedReason::Unreadable(e.to_string()))
    }

    fn remove_stale_marks(&self, index: EntryIndex) -> Result<(), UnresolvedReason> {
        self.backing_directory.remove_onedrive_marks(&self.contents.resolve_path(index)).map_err(|e| UnresolvedReason::Unreadable(e.to_string()))
    }

    /// What a finding of a file's content writes to the row of `item`, as that row was read.
    fn write_content(&mut self, item: LocalItemId, finding: DiskFinding, file: &DiskContent) -> Result<(), UnresolvedReason> {
        let row = self.rows.find(item);
        let was_open_for_writing = row.is_some_and(|row| row.content_before_write.is_some());
        let was_on_disk = row.is_none_or(|row| row.content_presence == ContentPresence::OnDisk);
        self.written.write_content(item, finding, file, was_open_for_writing, was_on_disk)
    }

    /// The place `name` in `directory` is nobody's: the item that has it has left.
    fn free_place(&mut self, directory: LocalItemId, name: &OsStr) -> Result<(), UnresolvedReason> {
        let Some(holder) = self.written.places().find_at(directory, name) else { return Ok(()) };
        self.make_leave(holder)?;
        match self.written.places().find_at(directory, name) {
            Some(_) => Err(UnresolvedReason::Blocked),
            None => Ok(()),
        }
    }

    /// `item` is at `name` in `directory`.
    fn move_to(&mut self, item: LocalItemId, directory: LocalItemId, name: &OsStr) -> Result<(), UnresolvedReason> {
        if self.written.places().is_at(item, directory, name) {
            return Ok(());
        }
        self.moving.push(item);
        let freed = match self.written.places().find_at(directory, name) {
            Some(holder) => self.make_leave(holder),
            None => Ok(()),
        };
        self.moving.pop();
        freed?;
        // A ring of moves wrote this one with the others.
        if self.written.places().is_at(item, directory, name) {
            return Ok(());
        }
        if self.written.places().find_at(directory, name).is_some() {
            return Err(UnresolvedReason::Blocked);
        }
        self.written.move_to_free_place(item, directory, name)
    }

    /// `item` leaves the place it has, which another item is to take: it is moved to where
    /// it was found, or leaves the folder with what is in it. One that is held back does
    /// not leave, and what waits for its place waits on.
    fn make_leave(&mut self, item: LocalItemId) -> Result<(), UnresolvedReason> {
        let entry = self.identities.find_entry(item);
        let is_begun = self.leaving.contains(&item) || entry.is_some_and(|entry| self.progress[entry] == EntryProgress::Begun);
        if is_begun {
            // It waits, further up, for what is being written now.
            return self.write_ring(item);
        }
        match (entry, self.findings.find_row_finding(item)) {
            (Some(entry), _) => match self.write_entry(entry) {
                Ok(_) => Ok(()),
                // The directory it is to be in is not made yet, and waits for this: it is
                // set aside, and moved into the directory when its turn comes.
                Err(UnwrittenEntry::DirectoryUnwritten) => self.set_aside(item),
                Err(UnwrittenEntry::Left) => Err(UnresolvedReason::Blocked),
            },
            (None, Some(finding @ (DiskFinding::Removed | DiskFinding::PlaceLost))) => self.take_from_folder(item, finding),
            (None, _) => Err(UnresolvedReason::Blocked),
        }
    }

    /// `item` leaves the folder as `finding` says, removed here or its place lost, after
    /// all that is in it has left. First what is found at another path is moved there, of
    /// all that the rows say is below it; only when all of that is written does a row leave
    /// the folder. So a directory that cannot go — a row below it is left as it is, or
    /// cannot be moved where it was found — keeps every row that is to leave with it.
    fn take_from_folder(&mut self, item: LocalItemId, finding: DiskFinding) -> Result<(), UnresolvedReason> {
        self.leaving.insert(item);
        let emptied = self.move_out_of(item).and_then(|()| self.written.places().list_inside(item).into_iter().try_for_each(|inside| self.make_leave(inside)));
        self.leaving.remove(&item);
        emptied.map_err(|_| UnresolvedReason::Blocked)?;
        self.written.take_from_folder(item, finding)?;
        self.outcome.add(finding);
        Ok(())
    }

    /// Of all that the rows say is below `directory`, which is leaving the folder: what is
    /// found at another path is moved there, and every other row is one that leaves too.
    /// Nothing is taken from the folder here.
    fn move_out_of(&mut self, directory: LocalItemId) -> Result<(), UnresolvedReason> {
        for inside in self.written.places().list_inside(directory) {
            match (self.identities.find_entry(inside), self.findings.find_row_finding(inside)) {
                (Some(_), _) => self.make_leave(inside)?,
                (None, Some(DiskFinding::Removed | DiskFinding::PlaceLost)) => self.move_out_of(inside)?,
                // A row that is left as it is, or stays: its directory stays with it.
                (None, _) => return Err(UnresolvedReason::Blocked),
            }
        }
        Ok(())
    }

    /// Where `item` is to be: the name of the entry it is, in the row of that entry's
    /// directory. `None` while that directory has no row.
    fn find_target(&self, item: LocalItemId) -> Option<(LocalItemId, &'a OsStr)> {
        let entry = self.identities.find_entry(item)?;
        let directory = match self.contents.parent(entry) {
            None => LocalItemId::ROOT,
            Some(directory) => match self.progress[directory] {
                EntryProgress::Written(directory) => directory,
                _ => return None,
            },
        };
        Some((directory, self.contents.entry(entry).name.as_os_str()))
    }

    /// `first`, whose move or leaving was begun further up and waits for what is being
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
            let takes_the_next_ones_place = |at: usize| self.written.places().is_at(ring[(at + 1) % ring.len()], targets[at].0, targets[at].1);
            ring.len() > 1 && (0..ring.len()).all(takes_the_next_ones_place)
        });
        let (Some(targets), true) = (targets, is_ring) else { return self.set_aside(first) };
        match ring[..] {
            [first, second] => self.written.exchange(first, second),
            _ => self.written.turn_round(&ring.iter().copied().zip(targets).collect::<Vec<_>>()),
        }
    }

    fn set_aside(&mut self, item: LocalItemId) -> Result<(), UnresolvedReason> {
        let name = self.written.set_aside(item)?;
        self.set_aside.push((item, name));
        Ok(())
    }
}
