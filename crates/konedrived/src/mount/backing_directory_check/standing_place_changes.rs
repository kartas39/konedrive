//! The place changes that stand on record when the daemon starts
//! ([`StandingPlaceChanges`]): each a change of the backing directory's names that a death
//! cut short, read from the store with what the rows and the disk say now of what it
//! names, decided ([`decide_settlement`]), and finished or undone — before the check looks
//! at anything else.
//!
//! This is the thin step around the decision: it reads a record, the rows of the items it
//! names and the kind of what is at the places it names; and it does what was decided —
//! one write of the store that takes the record with it, or one or two renames that
//! replace nothing and then the record's going. It looks at no place a record does not
//! name, and at one mark: whether the file where a download was to land is marked as the
//! whole copy of the record's item, as the landing marks it, before that file is removed.
//!
//! **State owned:** what could not be settled: the items and the places of the records
//! that stand on, which the check leaves as they are and says. A record stands on only
//! where a place it names could not be looked at; an undo the disk refuses is not tried
//! again, its record goes, and the entries are the check's to decide as they are.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use konedrive_tree::{
    ContentPresence, ContentVersion, Item, ItemKind, ItemPlace, ItemStore, LocalItemId, LocalVersionChange, MadeEntryOwner, PlaceChangeRecord, PlaceChangeRecordId, PlaceChangeRowWrite, TreeError,
};

use super::disk_finding::UnresolvedReason;
use super::place_change_settlement::{decide_settlement, AsideEntry, AsideRow, ChangeUndo, DiskPlace, LandedRow, LinkedEntry, MovedRow, PlaceChangeSettlement, RecordFacts, ReplacedRow, RowFinish, WriterFileRow};
use super::UnresolvedPath;
use crate::mount::backing_directory::{BackingDirectory, DiskMarks};
use crate::mount::upload_rules::UploadRules;

/// What the settling of the records left: nothing, when every record is gone.
#[derive(Default)]
pub(super) struct StandingPlaceChanges {
    /// The items the records that stand on name.
    items: HashSet<LocalItemId>,
    /// The places they name.
    places: HashSet<(LocalItemId, OsString)>,
    unresolved: Vec<UnresolvedPath>,
}

/// The rows and the disk a record is settled over.
struct RowsAndDisk<'a, 'store> {
    backing_directory: &'a BackingDirectory,
    upload_rules: &'a UploadRules,
    item_store: &'a mut ItemStore<'store>,
}

/// Whether nothing is at a path: it is not there, or a directory above it is not one.
fn is_nothing_there(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ENOTDIR)
}

/// Whether the row of `item` says it has an entry on disk: a directory may, and a file
/// whose content is on disk does.
fn has_entry_on_disk(item: &Item) -> bool {
    item.kind == ItemKind::Directory || item.content_presence == ContentPresence::OnDisk
}

fn is_at(item: &Item, place: &ItemPlace) -> bool {
    item.local.as_ref().and_then(|local| local.place.as_ref()) == Some(place)
}

/// What tells the entry a record is about from another at its name: its kind, and for a
/// file whose row says what is on disk, that content's size and time.
struct OwnEntry<'a> {
    kind: ItemKind,
    content: Option<&'a ContentVersion>,
}

impl<'a> OwnEntry<'a> {
    /// An entry told by its kind alone.
    fn of_kind(kind: ItemKind) -> Self {
        OwnEntry { kind, content: None }
    }

    /// The entry of the item of `row`: a file whose content is on disk and that no writer
    /// holds is told by the size and the time its row says.
    fn of_row(row: &'a Item) -> Self {
        let is_content_on_disk = row.kind == ItemKind::File && row.content_presence == ContentPresence::OnDisk && row.content_before_write.is_none();
        OwnEntry { kind: row.kind, content: row.local.as_ref().and_then(|local| local.content.as_ref()).filter(|_| is_content_on_disk) }
    }

    fn is(&self, metadata: &std::fs::Metadata) -> bool {
        let is_of_kind = metadata.is_dir() == (self.kind == ItemKind::Directory);
        is_of_kind && self.content.is_none_or(|content| metadata.size() == content.size() && metadata.mtime() == content.mtime().0)
    }
}

impl RowsAndDisk<'_, '_> {
    /// The path of `place` in the backing directory; `None` when the folder does not show
    /// its directory.
    fn find_path(&self, place: &ItemPlace) -> Result<Option<PathBuf>, TreeError> {
        Ok(self.item_store.find_path(place.parent_local_item_id)?.map(|directory| directory.join(&place.name)))
    }

    fn read_metadata(&self, path: &Path) -> io::Result<std::fs::Metadata> {
        std::fs::symlink_metadata(self.backing_directory.resolve_path(path))
    }

    /// What the disk has at `place`, where the entry a record is about is told by `own`.
    fn look_at(&self, place: &ItemPlace, own: &OwnEntry<'_>) -> Result<DiskPlace, TreeError> {
        let Some(path) = self.find_path(place)? else { return Ok(DiskPlace::NoDirectory) };
        Ok(match self.read_metadata(&path) {
            Ok(metadata) if !metadata.is_file() && !metadata.is_dir() => DiskPlace::NotLookedAt,
            Ok(metadata) if own.is(&metadata) => DiskPlace::OwnEntry,
            Ok(_) => DiskPlace::AnotherEntry,
            // Nothing under the name: in a directory that is there, or with none.
            Err(e) if is_nothing_there(&e) => match self.read_metadata(path.parent().unwrap_or(Path::new(""))) {
                Ok(directory) if directory.is_dir() => DiskPlace::Nothing,
                Ok(_) => DiskPlace::NoDirectory,
                Err(e) if is_nothing_there(&e) => DiskPlace::NoDirectory,
                Err(_) => DiskPlace::NotLookedAt,
            },
            Err(_) => DiskPlace::NotLookedAt,
        })
    }

    /// What the disk has at the name an entry was set aside to, in the root.
    fn look_at_aside(&self, aside_name: &OsStr) -> AsideEntry {
        match self.read_metadata(Path::new(aside_name)) {
            Ok(_) => AsideEntry::Entry,
            Err(e) if is_nothing_there(&e) => AsideEntry::Nothing,
            Err(_) => AsideEntry::NotLookedAt,
        }
    }

    /// What the rows say of the item a move is of, and what the disk has at the two places.
    fn read_move(&self, item: LocalItemId, from: &ItemPlace, to: &ItemPlace) -> Result<(MovedRow, DiskPlace, DiskPlace), TreeError> {
        let Some(row) = self.item_store.find_item(item)? else { return Ok((MovedRow::Elsewhere, DiskPlace::Nothing, DiskPlace::Nothing)) };
        let moved = if is_at(&row, to) {
            MovedRow::AtTo
        } else if !is_at(&row, from) {
            MovedRow::Elsewhere
        } else if has_entry_on_disk(&row) {
            MovedRow::AtFromWithEntry
        } else {
            MovedRow::AtFromWithoutEntry
        };
        // Whatever is where it came from is in the way; what is where it went is the moved
        // entry only when it is still what the row says of it.
        Ok((moved, self.look_at(from, &OwnEntry::of_kind(row.kind))?, self.look_at(to, &OwnEntry::of_row(&row))?))
    }

    /// What the rows and the disk say now of what `record` names.
    fn read_facts(&self, record: &PlaceChangeRecord) -> Result<RecordFacts, TreeError> {
        Ok(match record {
            PlaceChangeRecord::MakeEntry { kind, place, owner: MadeEntryOwner::NewItem { .. } } => {
                let is_place_shown = self.item_store.find_item_at(place.parent_local_item_id, &place.name)?.is_some();
                RecordFacts::MadeForNewItem { is_place_shown, entry: self.look_at(place, &OwnEntry::of_kind(*kind))? }
            }
            PlaceChangeRecord::MakeEntry { place, owner: MadeEntryOwner::Item(item), .. } => {
                let row = match self.item_store.find_item(*item)? {
                    Some(row) if row.kind == ItemKind::File && is_at(&row, place) => match (row.content_presence, &row.content_before_write) {
                        (ContentPresence::NotOnDisk, _) => WriterFileRow::NotOnDisk,
                        (ContentPresence::OnDisk, Some(_)) => WriterFileRow::OnDiskWithContentBeforeWrite,
                        (ContentPresence::OnDisk, None) => WriterFileRow::OnDiskWithoutContentBeforeWrite,
                    },
                    _ => WriterFileRow::NotAtPlace,
                };
                RecordFacts::MadeForItem { row, entry: self.look_at(place, &OwnEntry::of_kind(ItemKind::File))? }
            }
            PlaceChangeRecord::MoveEntry { item, from, to } => {
                let (row, from, to) = self.read_move(*item, from, to)?;
                RecordFacts::Moved { row, from, to }
            }
            PlaceChangeRecord::MoveEntryOver { item, from, to, replaced, aside_name } => {
                let replaced = match self.item_store.find_item(*replaced)? {
                    Some(replaced) if is_at(&replaced, to) && has_entry_on_disk(&replaced) => ReplacedRow::AtToWithEntry,
                    Some(replaced) if is_at(&replaced, to) => ReplacedRow::AtToWithoutEntry,
                    _ => ReplacedRow::NotAtTo,
                };
                let (row, from, to) = self.read_move(*item, from, to)?;
                RecordFacts::MovedOver { row, replaced, from, to, aside: self.look_at_aside(aside_name) }
            }
            PlaceChangeRecord::SetAside { item, place, aside_name } => {
                let (row, kind) = match self.item_store.find_item(*item)? {
                    Some(row) if is_at(&row, place) && has_entry_on_disk(&row) => (AsideRow::AtPlaceWithEntry, row.kind),
                    _ => (AsideRow::Otherwise, ItemKind::File),
                };
                RecordFacts::SetAside { row, place: self.look_at(place, &OwnEntry::of_kind(kind))?, aside: self.look_at_aside(aside_name) }
            }
            PlaceChangeRecord::LinkFile { item, place, content } => {
                let item_row = self.item_store.find_item(*item)?.filter(|row| row.kind == ItemKind::File && is_at(row, place));
                let row = match &item_row {
                    Some(row) => match row.content_presence {
                        ContentPresence::OnDisk => LandedRow::OnDisk,
                        ContentPresence::NotOnDisk if row.local.as_ref().and_then(|local| local.content.as_ref()) == Some(content) => LandedRow::NotOnDiskWithRecordedContent,
                        ContentPresence::NotOnDisk => LandedRow::NotOnDiskWithOtherContent,
                    },
                    None => LandedRow::NotAtPlace,
                };
                // The file the landing linked: of the recorded size and time, and marked as
                // the whole copy of the item, as a landing marks its file before it links it.
                let onedrive_id = item_row.as_ref().and_then(|row| row.onedrive_item.as_ref()).map(|reference| reference.item_id.as_str());
                let entry = match self.find_path(place)?.map(|path| (self.read_metadata(&path), path)) {
                    None => LinkedEntry::Nothing,
                    Some((Ok(metadata), path)) => {
                        let is_of_recorded_content = metadata.is_file() && metadata.size() == content.size() && metadata.mtime() == content.mtime().0;
                        let is_marked_as_the_copy = onedrive_id.is_some_and(|onedrive_id| DiskMarks::read(&self.backing_directory.resolve_path(&path)).is_downloaded_copy_of(onedrive_id));
                        if is_of_recorded_content && is_marked_as_the_copy {
                            LinkedEntry::FileOfRecordedContent
                        } else {
                            LinkedEntry::AnotherEntry
                        }
                    }
                    Some((Err(e), _)) if is_nothing_there(&e) => LinkedEntry::Nothing,
                    Some((Err(_), _)) => LinkedEntry::NotLookedAt,
                };
                RecordFacts::Linked { row, entry }
            }
        })
    }

    /// Moves what is at `from` to `to`, where nothing is: nothing is replaced. A place the
    /// folder does not show any more has no path, and nothing is moved.
    fn rename_without_replacing(&self, from: Option<PathBuf>, to: Option<PathBuf>) -> io::Result<()> {
        let (Some(from), Some(to)) = (from, to) else { return Err(io::Error::from(io::ErrorKind::NotFound)) };
        let descriptor = self.backing_directory.clone_descriptor()?;
        nix::fcntl::renameat2(descriptor.as_fd(), &from, descriptor.as_fd(), &to, nix::fcntl::RenameFlags::RENAME_NOREPLACE).map_err(io::Error::from)
    }

    /// Writes `write` with the going of `record`; a write the rows refuse leaves nothing
    /// to finish, and the record goes by itself: the check decides the place.
    fn settle_or_forget(&mut self, record: PlaceChangeRecordId, write: &PlaceChangeRowWrite) -> Result<(), TreeError> {
        match self.item_store.settle_place_change_record(record, write) {
            Ok(_) => Ok(()),
            Err(TreeError::Refused(_)) => self.item_store.forget_place_change_record(record).map(|_| ()),
            Err(e) => Err(e),
        }
    }

    /// Settles one record. `Ok(true)`: it is gone. `Ok(false)`: it stands, a place it names
    /// could not be looked at.
    fn settle(&mut self, id: PlaceChangeRecordId, record: &PlaceChangeRecord) -> Result<bool, TreeError> {
        let settlement = decide_settlement(self.read_facts(record)?);
        // What is done on disk for an undo there; a settlement that is not one of the
        // record's case is done as nothing left.
        let undone: io::Result<()> = match (settlement, record) {
            (PlaceChangeSettlement::LeftStanding, _) => return Ok(false),
            (PlaceChangeSettlement::Finish(RowFinish::MadeItem), _) => return self.finish_made_item(id, record).map(|()| true),
            (PlaceChangeSettlement::Finish(RowFinish::ContentOnDisk), PlaceChangeRecord::LinkFile { item, content, .. }) => {
                return self.settle_or_forget(id, &PlaceChangeRowWrite::ContentOnDisk { item: *item, content: content.clone() }).map(|()| true);
            }
            (PlaceChangeSettlement::Undo(ChangeUndo::ContentBeforeWrite), PlaceChangeRecord::MakeEntry { owner: MadeEntryOwner::Item(item), .. }) => {
                return self.settle_or_forget(id, &PlaceChangeRowWrite::ContentBeforeWrite { item: *item }).map(|()| true);
            }
            (PlaceChangeSettlement::Undo(ChangeUndo::MoveBack), PlaceChangeRecord::MoveEntry { from, to, .. } | PlaceChangeRecord::MoveEntryOver { from, to, .. }) => {
                self.rename_without_replacing(self.find_path(to)?, self.find_path(from)?)
            }
            (PlaceChangeSettlement::Undo(ChangeUndo::Restore), PlaceChangeRecord::MoveEntryOver { to: place, aside_name, .. } | PlaceChangeRecord::SetAside { place, aside_name, .. }) => {
                self.rename_without_replacing(Some(PathBuf::from(aside_name)), self.find_path(place)?)
            }
            (PlaceChangeSettlement::Undo(ChangeUndo::MoveBackAndRestore), PlaceChangeRecord::MoveEntryOver { from, to, aside_name, .. }) => {
                let (from, to) = (self.find_path(from)?, self.find_path(to)?);
                self.rename_without_replacing(to.clone(), from).and_then(|()| self.rename_without_replacing(Some(PathBuf::from(aside_name)), to))
            }
            (PlaceChangeSettlement::Undo(ChangeUndo::RemoveLinkedFile), PlaceChangeRecord::LinkFile { place, .. }) => match self.find_path(place)? {
                Some(path) => self.backing_directory.remove_file(&path),
                None => Ok(()),
            },
            (PlaceChangeSettlement::NothingLeft | PlaceChangeSettlement::Finish(_) | PlaceChangeSettlement::Undo(_), _) => Ok(()),
        };
        // An undo the disk refused is not tried again at every start: the entries stay as
        // they are, and the check decides their places like any other.
        if let Err(error) = undone {
            tracing::warn!("a change of the backing directory that a stop cut short could not be undone: {error}; what it left is taken as it is");
        }
        self.item_store.forget_place_change_record(id).map(|_| true)
    }

    /// The row of a made entry, as the make would have written it: a file with the size
    /// and the time the disk has, changed here; taking over the binding of the removed
    /// file the record names, where the rows still let it.
    fn finish_made_item(&mut self, id: PlaceChangeRecordId, record: &PlaceChangeRecord) -> Result<(), TreeError> {
        let PlaceChangeRecord::MakeEntry { kind, place, owner: MadeEntryOwner::NewItem { replaced } } = record else { return Ok(()) };
        let metadata = self.find_path(place)?.and_then(|path| self.read_metadata(&path).ok());
        let content = match (kind, metadata) {
            (ItemKind::File, Some(metadata)) => Some(ContentVersion::ChangedHere { size: metadata.size(), mtime: metadata.mtime(), mtime_nsec: metadata.mtime_nsec() as u32, change_count: 1 }),
            // Gone since it was looked at: the check decides the place.
            (ItemKind::File, None) => return self.item_store.forget_place_change_record(id).map(|_| ()),
            (ItemKind::Directory, _) => None,
        };
        let is_name_ignored = self.upload_rules.is_name_ignored(&place.name);
        let made = |replaced| PlaceChangeRowWrite::LocalVersion(LocalVersionChange::Made { kind: *kind, place: place.clone(), content: content.clone(), is_name_ignored, replaced });
        match (self.item_store.settle_place_change_record(id, &made(*replaced)), replaced) {
            (Ok(_), _) => Ok(()),
            // The file it was to replace cannot be replaced any more: a new item.
            (Err(TreeError::Refused(_)), Some(_)) => self.settle_or_forget(id, &made(None)),
            (Err(TreeError::Refused(_)), None) => self.item_store.forget_place_change_record(id).map(|_| ()),
            (Err(e), _) => Err(e),
        }
    }
}

/// The items a record names, and the places.
fn list_named(record: &PlaceChangeRecord) -> (Vec<LocalItemId>, Vec<&ItemPlace>) {
    match record {
        PlaceChangeRecord::MakeEntry { place, owner: MadeEntryOwner::NewItem { .. }, .. } => (vec![], vec![place]),
        PlaceChangeRecord::MakeEntry { place, owner: MadeEntryOwner::Item(item), .. } => (vec![*item], vec![place]),
        PlaceChangeRecord::MoveEntry { item, from, to } => (vec![*item], vec![from, to]),
        PlaceChangeRecord::MoveEntryOver { item, from, to, replaced, .. } => (vec![*item, *replaced], vec![from, to]),
        PlaceChangeRecord::SetAside { item, place, .. } | PlaceChangeRecord::LinkFile { item, place, .. } => (vec![*item], vec![place]),
    }
}

impl StandingPlaceChanges {
    /// Settles every record that stands, in the order they were written, and answers what
    /// could not be settled. An error is the store's.
    pub(super) fn settle(backing_directory: &BackingDirectory, upload_rules: &UploadRules, item_store: &mut ItemStore<'_>) -> Result<Self, TreeError> {
        let mut standing = StandingPlaceChanges::default();
        for (id, record) in item_store.list_place_change_records()? {
            let mut settling = RowsAndDisk { backing_directory, upload_rules, item_store };
            if settling.settle(id, &record)? {
                continue;
            }
            let (items, places) = list_named(&record);
            for place in places {
                let path = settling.find_path(place)?.unwrap_or_else(|| PathBuf::from(&place.name));
                standing.unresolved.push(UnresolvedPath { path, reason: UnresolvedReason::PlaceChangeUnsettled });
                standing.places.insert((place.parent_local_item_id, place.name.clone()));
            }
            standing.items.extend(items);
        }
        Ok(standing)
    }

    /// Whether every record is gone.
    pub(super) fn is_settled(&self) -> bool {
        self.places.is_empty()
    }

    /// Whether a record that stands on names `item`: its row is left as it is.
    pub(super) fn names_item(&self, item: LocalItemId) -> bool {
        self.items.contains(&item)
    }

    /// Whether a record that stands on names the place `name` in `directory`.
    pub(super) fn names_place(&self, directory: LocalItemId, name: &OsStr) -> bool {
        !self.places.is_empty() && self.places.contains(&(directory, name.to_owned()))
    }

    /// The paths of the places the records that stand on name, each with why.
    pub(super) fn list_unresolved(&self) -> &[UnresolvedPath] {
        &self.unresolved
    }
}
