//! The order in which the findings are written to the rows ([`WriteOrder`]), so that the
//! schema accepts each write: a parent's row before its child's, a name freed before it is
//! taken, what leaves the folder deepest first.
//!
//! What is written for a finding is [`WrittenRows`]'s; here is which write waits for which.
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
//! A row that is held back because part of the backing directory was not read is not
//! written, and neither is what waits for its place: both are said.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};

use konedrive_tree::{ContentPresence, DriveId, ItemKind, ItemStore, LocalItemId, OneDriveItemId, OneDriveItemReference};

use super::backing_directory_contents::{BackingDirectoryContents, EntryIndex};
use super::disk_finding::{DiskContent, DiskFinding, MarkOwner, UnresolvedReason};
use super::entry_identities::{EntryIdentities, EntryIdentity};
use super::item_rows::ItemRows;
use super::path_findings::PathFindings;
use super::written_rows::{tell_made_content, WrittenRows};
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
    /// The marks of an entry that are not its row's own are taken off through it.
    backing_directory: &'a BackingDirectory,
    rows: &'a ItemRows,
    contents: &'a BackingDirectoryContents,
    identities: &'a EntryIdentities,
    findings: &'a PathFindings,
    /// The rows as written so far, and what writes them.
    written: WrittenRows<'a, 'store>,
    progress: Vec<EntryProgress>,
    /// The items being moved now, the one that waits for the next before it.
    moving: Vec<LocalItemId>,
    /// The directories that are leaving the folder now: what is in them leaves first.
    leaving: HashSet<LocalItemId>,
    /// The items that were set aside by a write of their own, each with the name it was
    /// given in the root of the folder.
    set_aside: Vec<(LocalItemId, OsString)>,
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
        let written = WrittenRows::new(item_store, upload_rules, drive_id, rows);
        let progress = vec![EntryProgress::Waiting; contents.len()];
        let outcome = DiskFindings::begin(contents.list_unresolved().to_vec(), findings.is_read_whole());
        WriteOrder { backing_directory, rows, contents, identities, findings, written, progress, moving: Vec::new(), leaving: HashSet::new(), set_aside: Vec::new(), outcome }
    }

    /// Writes every finding, and answers how many of each were written and what was left.
    pub(super) fn write(mut self) -> DiskFindings {
        for index in 0..self.contents.len() {
            let _ = self.write_entry(index);
        }
        for &(item, finding) in self.findings.list_rows() {
            let written = match finding {
                // One that went with its directory, or made way for an entry, is gone already.
                DiskFinding::Removed | DiskFinding::PlaceLost if self.written.places().find_place(item).is_none() => Ok(()),
                DiskFinding::Removed | DiskFinding::PlaceLost => self.take_from_folder(item, finding),
                DiskFinding::ContentNoLongerOnDisk => self.written.write_content(item, finding, &DiskContent::default(), false, true).map(|()| self.outcome.add(finding)),
                _ => {
                    self.outcome.add(finding);
                    Ok(())
                }
            };
            if let Err(reason) = written {
                self.outcome.leave(self.written.find_path(item), reason);
            }
        }
        // What may be in the part that was not read keeps its row as it is.
        for &item in self.findings.list_held_back() {
            self.outcome.leave(self.written.find_path(item), UnresolvedReason::MayBeUnread);
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
                if let Some(reason) = reason {
                    self.outcome.leave(self.contents.resolve_path(index), reason);
                }
                Err(UnwrittenEntry::Left)
            }
        }
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
                if matches!(self.identities.mark_owner(index), MarkOwner::RowOfAnotherEntry | MarkOwner::NoRow) {
                    self.remove_stale_marks(index)?;
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
    /// names: the one a row the folder does not show has under that id, or one of the drive
    /// of the directory it is in.
    fn bind(&mut self, item: LocalItemId, directory: LocalItemId, index: EntryIndex) -> Result<(), UnresolvedReason> {
        let onedrive_id = self.contents.entry(index).onedrive_id.as_deref().unwrap_or_default();
        let drive = self.written.find_drive_inside(directory).clone();
        let reference = match self.rows.find_unshown_bound(&drive, onedrive_id).and_then(|unshown| unshown.onedrive_item.clone()) {
            Some(reference) => reference,
            None => OneDriveItemReference { drive_id: drive, item_id: OneDriveItemId::new(onedrive_id) },
        };
        self.written.bind(item, &reference)
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
    /// all that is in it has left.
    fn take_from_folder(&mut self, item: LocalItemId, finding: DiskFinding) -> Result<(), UnresolvedReason> {
        self.leaving.insert(item);
        let emptied = self.written.places().list_inside(item).into_iter().try_for_each(|inside| self.make_leave(inside));
        self.leaving.remove(&item);
        emptied.map_err(|_| UnresolvedReason::Blocked)?;
        self.written.take_from_folder(item, finding)?;
        self.outcome.add(finding);
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
