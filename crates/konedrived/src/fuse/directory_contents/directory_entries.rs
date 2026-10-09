//! The entries of a directory of the mount: the files and directories the store has there,
//! as the unsent changes leave them, and the local-only entries of the backing directory.
//!
//! A local-only entry is a file or a directory of the backing directory that is marked with
//! no OneDrive id the store knows: made by a program through the mount, and uploaded by the
//! outbox. It is shown where it lies, and is identified by its file handle. A file or
//! directory of OneDrive is shown where the store has it, or where the user moved it; a name
//! both have is OneDrive's.

use std::collections::HashSet;
use std::path::Path;

use konedrive_tree::{Kind, Placement, Row, Table};

use super::{DirectoryContents, DirectoryEntry, DirectorySyncState, EntryState};
use crate::folder::disk::daemon_owned;
use crate::fuse::detections::LocalOnlyEntry;
use crate::fuse::directory::Directory;
use crate::fuse::local_only_entries::folder_place;
use crate::fuse::unsent_changes::UnsentChanges;

/// Whether a row the store has in a directory is still shown there, for the unsent changes.
fn stays(row: &Row, unsent_changes: &UnsentChanges) -> bool {
    row.placement == Placement::Placed && !unsent_changes.removed.contains(&row.id) && !unsent_changes.moved.contains_key(&row.id)
}

impl DirectoryContents {
    /// The local-only entry `name` of `directory`, in the backing directory.
    pub(in crate::fuse) fn find_local_only(&self, directory: &Directory, name: &str) -> Option<LocalOnlyEntry> {
        let rel = directory.rel.join(name);
        let name = rel.file_name()?;
        if daemon_owned(name) {
            return None;
        }
        let on_disk = self.backing_directory.find_disk_entry(&rel)?;
        // One that is marked with an id the store does not have is not in OneDrive yet: its
        // upload is being committed, or the store was made anew.
        let in_onedrive = self.backing_directory.read_onedrive_id(&rel).is_some_and(|id| self.row(&id).is_some());
        (!in_onedrive).then(|| LocalOnlyEntry { on_disk, place: folder_place(directory, &name.to_string_lossy()) })
    }

    /// The row of the OneDrive id `id` under the name `name` the mount shows it by, unless
    /// the store has it out of the folder. What the unsent changes say of it is the caller's
    /// to ask.
    fn find_placed_row(&self, id: &str, name: &str) -> Option<Row> {
        let row = self.row(id).filter(|r| r.placement == Placement::Placed)?;
        Some(Row { name: name.to_owned(), ..row })
    }

    /// The entry of `row`, a row of the store under the name the mount shows it by, at the
    /// path `rel`: a directory, or a file with its sync state.
    pub(in crate::fuse) fn describe(&self, row: Row, rel: &Path) -> DirectoryEntry {
        self.describe_for(row, rel, &self.unsent_changes())
    }

    fn describe_for(&self, row: Row, rel: &Path, unsent_changes: &UnsentChanges) -> DirectoryEntry {
        let name = row.name.clone();
        let state = match row.kind {
            Kind::Folder => EntryState::Directory(DirectorySyncState::Synced(row)),
            Kind::File => EntryState::File(self.file_sync_state(row, rel, unsent_changes)),
        };
        DirectoryEntry { name, state }
    }

    /// The entry of the OneDrive id `id`, which the mount shows as `name` at `rel`, unless the
    /// store has it out of the folder. What the unsent changes say of where it is, is the
    /// caller's to ask.
    pub(in crate::fuse) fn find_entry_by_id(&self, id: &str, name: &str, rel: &Path) -> Option<DirectoryEntry> {
        self.find_placed_row(id, name).map(|row| self.describe(row, rel))
    }

    /// The entry `directory` holds under `name`.
    pub(in crate::fuse) fn find_entry(&self, directory: &Directory, name: &str) -> Option<DirectoryEntry> {
        match self.find_row_at(directory, name) {
            Some(row) => Some(self.describe(row, &directory.rel.join(name))),
            None => self.find_local_only(directory, name).map(DirectoryEntry::from_local_only),
        }
    }

    /// The row of what OneDrive has, or the user moved, under `name` in `directory`.
    pub(super) fn find_row_at(&self, directory: &Directory, name: &str) -> Option<Row> {
        let unsent_changes = self.unsent_changes();
        let stored = directory.id.as_deref().and_then(|id| self.read(|s| s.child(Table::Items, id, name)).flatten());
        // What the user moved to this name is what the name is: what OneDrive has there too
        // is shown once the move is sent, or refused.
        let moved = unsent_changes.moved.iter().find(|(_, (to, as_))| directory.matches(to) && as_ == name);
        match moved {
            Some((id, _)) => self.find_placed_row(id, name),
            None => stored.filter(|row| stays(row, &unsent_changes)),
        }
    }

    /// Every entry `directory` holds.
    pub(in crate::fuse) fn list_entries(&self, directory: &Directory) -> Vec<DirectoryEntry> {
        let unsent_changes = self.unsent_changes();
        let stored = directory.id.as_deref().and_then(|id| self.read(|s| s.children(Table::Items, id))).unwrap_or_default();
        let mut rows: Vec<Row> = unsent_changes.moved.iter().filter(|(_, (to, _))| directory.matches(to)).filter_map(|(id, (_, name))| self.find_placed_row(id, name)).collect();
        let moved_here: HashSet<String> = rows.iter().map(|row| row.name.clone()).collect();
        rows.extend(stored.into_iter().filter(|row| stays(row, &unsent_changes) && !moved_here.contains(&row.name)));
        let taken: HashSet<String> = rows.iter().map(|row| row.name.clone()).collect();
        let mut all: Vec<DirectoryEntry> = rows.into_iter().map(|row| {
            let rel = directory.rel.join(&row.name);
            self.describe_for(row, &rel, &unsent_changes)
        }).collect();
        for name in self.backing_directory.list(&directory.rel).map(|e| e.file_name()) {
            if let Some(name) = name.to_str().filter(|name| !taken.contains(*name)) {
                all.extend(self.find_local_only(directory, name).map(DirectoryEntry::from_local_only));
            }
        }
        all
    }
}
