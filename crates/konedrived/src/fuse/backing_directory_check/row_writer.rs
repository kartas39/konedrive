//! What the check writes rows through ([`RowWriter`]): the handle of the store, the upload
//! rules, and in memory the place and the drive of each item, kept in step with every write
//! made through it.
//!
//! What a finding writes is here, each write one operation of [`ItemStore`] in a
//! transaction of its own: a new item, a binding, a content, a move to a free place, a
//! removal, a lost place, an exchange, a ring of moves, a row set aside. Nothing here asks
//! which write comes first: a write whose place is taken, or whose directory holds
//! something, is refused by the store, and the order is
//! [`WriteOrder`](super::write_order::WriteOrder)'s.
//!
//! Here is also the one place where the check derives a drive
//! ([`RowWriter::derive_drive_inside`]): an entry marked with an item id no row has is bound
//! in the drive of the nearest directory above it that has a binding. No row is looked for
//! by a drive anywhere in the check.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use konedrive_tree::{ContentPresence, ContentVersion, DriveId, ItemKind, ItemPlace, ItemStore, LocalItemId, LocalVersionChange, OneDriveItemId, OneDriveItemReference, TreeError};

use super::backing_directory_contents::BackingDirectoryEntry;
use super::disk_finding::{DiskContent, DiskFinding, UnresolvedReason};
use super::item_rows::ItemRows;
use super::local_places::LocalPlaces;
use crate::folder::disk::SET_ASIDE_PREFIX;
use crate::fuse::upload_rules::UploadRules;

/// The reason a path is left with when the store refused its write.
fn to_refused(error: TreeError) -> UnresolvedReason {
    UnresolvedReason::Refused(error.to_string())
}

/// The content a file is made with: of a marked file no row knows, the one its cTag mark
/// tells, with no hash, for the plan to compare with OneDrive's; of any other, one changed
/// here. A directory has none.
pub(super) fn tell_made_content(entry: &BackingDirectoryEntry, finding: DiskFinding) -> Option<ContentVersion> {
    let (size, mtime, mtime_nsec) = (entry.file.size, entry.file.mtime, entry.file.mtime_nsec);
    (entry.kind == ItemKind::File).then(|| match &entry.file.ctag {
        Some(ctag) if finding == DiskFinding::MarkedWithoutRow => ContentVersion::Hashed { size, mtime, mtime_nsec, hash: None, ctag: Some(ctag.clone()) },
        _ => ContentVersion::ChangedHere { size, mtime, mtime_nsec, change_count: 1 },
    })
}

pub(super) struct RowWriter<'a, 'store> {
    item_store: &'a mut ItemStore<'store>,
    /// What says of a made or moved item's name whether it is ignored.
    upload_rules: &'a UploadRules,
    /// The drive of the folder: of a mark no row has, in a directory with no directory of
    /// OneDrive above it.
    drive_id: &'a DriveId,
    /// Where each item is in the folder, as the rows say it after the writes so far.
    places: LocalPlaces,
    /// The drive of each item that has a binding, after the writes so far.
    drives: HashMap<LocalItemId, DriveId>,
}

impl<'a, 'store> RowWriter<'a, 'store> {
    /// Over the rows as they were read: nothing is written yet.
    pub(super) fn new(item_store: &'a mut ItemStore<'store>, upload_rules: &'a UploadRules, drive_id: &'a DriveId, rows: &ItemRows) -> Self {
        let places = LocalPlaces::new(rows.list());
        let drives = rows.list().filter_map(|item| Some((item.local_item_id, item.onedrive_item.as_ref()?.drive_id.clone()))).collect();
        RowWriter { item_store, upload_rules, drive_id, places, drives }
    }

    /// Which item has each place now.
    pub(super) fn places(&self) -> &LocalPlaces {
        &self.places
    }

    /// The path of `item` as the rows say it now; empty when the folder does not show it.
    pub(super) fn find_path(&self, item: LocalItemId) -> PathBuf {
        self.item_store.find_path(item).ok().flatten().unwrap_or_default()
    }

    /// A new item for `entry`, at its name in `directory`, a file with `content`, its
    /// content on disk. `replaced` is the file removed here from that place, whose binding
    /// the new one takes over.
    pub(super) fn make(&mut self, directory: LocalItemId, entry: &BackingDirectoryEntry, content: Option<ContentVersion>, replaced: Option<LocalItemId>) -> Result<LocalItemId, UnresolvedReason> {
        let place = ItemPlace { parent_local_item_id: directory, name: entry.name.clone() };
        let is_name_ignored = self.upload_rules.is_name_ignored(&entry.name);
        let item = self.item_store.apply(&LocalVersionChange::Made { kind: entry.kind, place, content, is_name_ignored, replaced }).map_err(to_refused)?;
        self.places.set(item, directory, &entry.name);
        if let Some(drive) = replaced.and_then(|replaced| self.drives.get(&replaced).cloned()) {
            self.drives.insert(item, drive);
        }
        Ok(item)
    }

    /// The drive of an item of OneDrive that is found in `directory` and that no row has:
    /// that of the nearest directory, from `directory` up, that has a binding, or the
    /// folder's own. An item is of the drive of the directory OneDrive has it in; it is the
    /// one thing the check derives a drive for.
    fn derive_drive_inside(&self, directory: LocalItemId) -> &DriveId {
        let mut at = Some(directory);
        while let Some(here) = at {
            if let Some(drive) = self.drives.get(&here).filter(|_| here != LocalItemId::ROOT) {
                return drive;
            }
            at = self.places.find_place(here).map(|(above, _)| above);
        }
        self.drive_id
    }

    /// `item` is the item `onedrive_item` of OneDrive: the row the folder does not show that
    /// has that binding is merged into it.
    pub(super) fn bind(&mut self, item: LocalItemId, onedrive_item: &OneDriveItemReference) -> Result<(), UnresolvedReason> {
        self.item_store.bind_item(item, onedrive_item).map_err(to_refused)?;
        self.drives.insert(item, onedrive_item.drive_id.clone());
        Ok(())
    }

    /// `item`, which is found in `directory` marked with an item id no row has, is that item
    /// of OneDrive in the drive of the directory: the one binding whose drive the check
    /// derives ([`derive_drive_inside`](Self::derive_drive_inside)).
    pub(super) fn bind_in_drive_of_directory(&mut self, item: LocalItemId, directory: LocalItemId, onedrive_id: &str) -> Result<(), UnresolvedReason> {
        let reference = OneDriveItemReference { drive_id: self.derive_drive_inside(directory).clone(), item_id: OneDriveItemId::new(onedrive_id) };
        self.bind(item, &reference)
    }

    /// What a finding of a file's content writes to the row of `item`, whose content on
    /// disk is `file`. `was_open_for_writing` and `was_on_disk` are what the row said when
    /// it was read.
    pub(super) fn write_content(&mut self, item: LocalItemId, finding: DiskFinding, file: &DiskContent, was_open_for_writing: bool, was_on_disk: bool) -> Result<(), UnresolvedReason> {
        match finding {
            // A daemon died with a writer open that wrote nothing: as its close leaves it.
            DiskFinding::NothingChanged if was_open_for_writing => self.item_store.apply(&LocalVersionChange::ClosedUnchanged { item }).map(|_| ()).map_err(to_refused),
            DiskFinding::ContentChanged => {
                // Another content, its count raised: by the open that the dead daemon wrote,
                // or by one written here with the close.
                let closed = LocalVersionChange::ClosedChanged { item, size: file.size, mtime: file.mtime, mtime_nsec: file.mtime_nsec };
                let changes = if was_open_for_writing { vec![closed] } else { vec![LocalVersionChange::OpenedForWriting { item }, closed] };
                self.item_store.apply_all(&changes).map_err(to_refused)?;
                if !was_on_disk {
                    self.item_store.set_content_presence(item, ContentPresence::OnDisk).map_err(to_refused)?;
                }
                Ok(())
            }
            DiskFinding::CopyLandedBeforeRow => self.item_store.set_content_presence(item, ContentPresence::OnDisk).map_err(to_refused),
            DiskFinding::ContentNoLongerOnDisk => self.item_store.set_content_presence(item, ContentPresence::NotOnDisk).map_err(to_refused),
            _ => Ok(()),
        }
    }

    /// `item` is at `name` in `directory`, a place no item has.
    pub(super) fn move_to_free_place(&mut self, item: LocalItemId, directory: LocalItemId, name: &OsStr) -> Result<(), UnresolvedReason> {
        let place = ItemPlace { parent_local_item_id: directory, name: name.to_owned() };
        let is_name_ignored = self.upload_rules.is_name_ignored(name);
        self.item_store.apply(&LocalVersionChange::Moved { item, place, is_name_ignored }).map_err(to_refused)?;
        self.places.set(item, directory, name);
        Ok(())
    }

    /// `item`, which holds nothing any more, leaves the folder as `finding` says: removed
    /// here, or its place lost.
    pub(super) fn take_from_folder(&mut self, item: LocalItemId, finding: DiskFinding) -> Result<(), UnresolvedReason> {
        match finding {
            DiskFinding::Removed => self.item_store.apply(&LocalVersionChange::Removed { item }).map(|_| ()),
            DiskFinding::PlaceLost => self.item_store.clear_local_and_synced_version(item),
            other => Err(TreeError::Refused(format!("{other:?} takes no item from the folder"))),
        }
        .map_err(to_refused)?;
        self.places.remove(item);
        Ok(())
    }

    /// The two items have each the other's place.
    pub(super) fn exchange(&mut self, first: LocalItemId, second: LocalItemId) -> Result<(), UnresolvedReason> {
        let (Some((first_directory, first_name)), Some((second_directory, second_name))) = (self.places.find_place(first), self.places.find_place(second)) else {
            return Err(UnresolvedReason::Blocked);
        };
        let (first_name, second_name) = (first_name.to_owned(), second_name.to_owned());
        self.item_store.exchange_local_places(first, second).map_err(to_refused)?;
        self.places.set(first, second_directory, &second_name);
        self.places.set(second, first_directory, &first_name);
        Ok(())
    }

    /// Three or more items, each moved to its place of `ring`, which the next one has and
    /// the last one's the first has: in one transaction, the first set aside for as long
    /// as it lasts.
    pub(super) fn turn_round(&mut self, ring: &[(LocalItemId, (LocalItemId, &OsStr))]) -> Result<(), UnresolvedReason> {
        let Some(&(first, _)) = ring.first() else { return Ok(()) };
        // The first steps aside, the last takes its place, and so on back to the first.
        let mut changes = vec![self.move_aside(first)];
        for &(item, (directory, name)) in ring.iter().rev() {
            let place = ItemPlace { parent_local_item_id: directory, name: name.to_owned() };
            changes.push(LocalVersionChange::Moved { item, place, is_name_ignored: self.upload_rules.is_name_ignored(name) });
        }
        self.item_store.apply_all(&changes).map_err(to_refused)?;
        for &(item, (directory, name)) in ring {
            self.places.set(item, directory, name);
        }
        Ok(())
    }

    /// The change that sets `item` aside: to a name of the daemon's own in the root of the
    /// folder, which no entry of the backing directory has, and that no row has.
    fn move_aside(&self, item: LocalItemId) -> LocalVersionChange {
        let mut name = OsString::from(format!("{SET_ASIDE_PREFIX}{item}"));
        while self.places.find_at(LocalItemId::ROOT, &name).is_some() {
            name.push("-");
        }
        LocalVersionChange::Moved { item, place: ItemPlace { parent_local_item_id: LocalItemId::ROOT, name }, is_name_ignored: false }
    }

    /// Sets `item` aside by a write of its own, and answers the name its row has now.
    pub(super) fn set_aside(&mut self, item: LocalItemId) -> Result<OsString, UnresolvedReason> {
        let aside = self.move_aside(item);
        self.item_store.apply(&aside).map_err(to_refused)?;
        let LocalVersionChange::Moved { place, .. } = aside else { return Err(UnresolvedReason::Blocked) };
        self.places.set(item, LocalItemId::ROOT, &place.name);
        Ok(place.name)
    }
}

#[cfg(test)]
mod tests;
