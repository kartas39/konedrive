//! The entries of a directory of the mount: the files and directories the store has there,
//! as the unsent changes leave them, and the local-only entries of the backing directory.
//!
//! A local-only entry is a file or a directory of the backing directory that is marked with
//! no OneDrive id the store knows: made by a program through the mount, and uploaded by the
//! outbox. It is shown where it lies, and is identified by its file handle. A file or
//! directory of OneDrive is shown where the store has it, or where the user moved it; a name
//! both have is OneDrive's.
//!
//! A directory is listed as [`ListedName`]s: for what is in OneDrive the store's row alone,
//! which says the name and the kind without the backing directory being read. The entry with
//! its sync state is made from a listed name only when it is asked for
//! ([`DirectoryContents::describe_listed_name`]).

use std::collections::HashSet;
use std::path::Path;

use konedrive_tree::{Kind, Placement, Row, Table};

use crate::fuse::entry_place::EntryPlace;
use crate::fuse::local_only_entry::LocalOnlyEntry;
use super::{DirectoryContents, DirectoryEntry, DirectorySyncState, EntryState};
use crate::folder::disk::daemon_owned;
use crate::fuse::backing_directory::EntryKind;
use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::unsent_changes::UnsentChanges;

/// One name a directory of the mount lists, with what has it: the store's row of a file or directory of OneDrive,
/// under the name the mount shows it by, or a local-only entry of the backing directory. It
/// says the name and the kind; the sync state is worked out from it, when it is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fuse) enum ListedName {
    /// A file or directory of OneDrive: nothing of it was read from the backing directory.
    InOneDrive(Row),
    /// A file or directory made here, as the backing directory has it.
    LocalOnly(LocalOnlyEntry),
}

impl ListedName {
    /// The name the mount shows it by.
    pub(in crate::fuse) fn name(&self) -> &str {
        match self {
            ListedName::InOneDrive(row) => &row.name,
            ListedName::LocalOnly(entry) => &entry.place.name,
        }
    }

    /// Whether it is a file or a directory.
    pub(in crate::fuse) fn kind(&self) -> EntryKind {
        match self {
            ListedName::InOneDrive(Row { kind: Kind::Folder, .. }) => EntryKind::Directory,
            ListedName::InOneDrive(Row { kind: Kind::File, .. }) => EntryKind::File,
            ListedName::LocalOnly(entry) => entry.on_disk.metadata.kind,
        }
    }
}

/// Whether a row the store has in a directory is still shown there, for the unsent changes.
fn stays(row: &Row, unsent_changes: &UnsentChanges) -> bool {
    row.placement == Placement::Placed && !unsent_changes.removed_ids.contains(&row.id) && !unsent_changes.moved_ids.contains_key(&row.id)
}

impl DirectoryContents {
    /// The local-only entry `name` of `directory`, in the backing directory.
    pub(in crate::fuse) fn find_local_only(&self, directory: &DirectoryReference, name: &str) -> Option<LocalOnlyEntry> {
        let rel = directory.path.join(name);
        let name = rel.file_name()?;
        if daemon_owned(name) {
            return None;
        }
        let on_disk = self.backing_directory.find_disk_entry(&rel)?;
        // One that is marked with an id the store does not have is not in OneDrive yet: its
        // upload is being committed, or the store was made anew.
        let in_onedrive = self.backing_directory.read_onedrive_id(&rel).is_some_and(|id| self.find_row(&id).is_some());
        (!in_onedrive).then(|| LocalOnlyEntry { on_disk, place: EntryPlace::new(directory, &name.to_string_lossy()) })
    }

    /// The row of the OneDrive id `id` under the name `name` the mount shows it by, unless
    /// the store has it out of the folder. What the unsent changes say of it is the caller's
    /// to ask.
    fn find_placed_row(&self, id: &str, name: &str) -> Option<Row> {
        let row = self.find_row(id).filter(|r| r.placement == Placement::Placed)?;
        Some(Row { name: name.to_owned(), ..row })
    }

    /// The entry of `row`, a row of the store under the name the mount shows it by, at the
    /// path `rel`: a directory, or a file with its sync state.
    pub(in crate::fuse) fn describe(&self, row: Row, rel: &Path) -> DirectoryEntry {
        let name = row.name.clone();
        let state = match row.kind {
            Kind::Folder => EntryState::Directory(DirectorySyncState::InOneDrive(row)),
            Kind::File => EntryState::File(self.file_sync_state(row, rel, &self.read_unsent_changes())),
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
    pub(in crate::fuse) fn find_entry(&self, directory: &DirectoryReference, name: &str) -> Option<DirectoryEntry> {
        match self.find_row_at(directory, name) {
            Some(row) => Some(self.describe(row, &directory.path.join(name))),
            None => self.find_local_only(directory, name).map(DirectoryEntry::from_local_only),
        }
    }

    /// The row of what OneDrive has, or the user moved, under `name` in `directory`.
    pub(super) fn find_row_at(&self, directory: &DirectoryReference, name: &str) -> Option<Row> {
        let unsent_changes = self.read_unsent_changes();
        let stored = directory.onedrive_id.as_deref().and_then(|id| self.read(|s| s.child(Table::Items, id, name)).flatten());
        // What the user moved to this name is what the name is: what OneDrive has there too
        // is shown once the move is sent, or refused.
        let moved = unsent_changes.moved_ids.iter().find(|(_, (to, as_))| directory.matches(to) && as_ == name);
        match moved {
            Some((id, _)) => self.find_placed_row(id, name),
            None => stored.filter(|row| stays(row, &unsent_changes)),
        }
    }

    /// The entry made from `listed_name`, which `directory` holds: of what is in OneDrive, a
    /// directory, or a file with its sync state as the backing directory has its copy now.
    pub(in crate::fuse) fn describe_listed_name(&self, listed_name: &ListedName, directory: &DirectoryReference) -> DirectoryEntry {
        match listed_name {
            ListedName::InOneDrive(row) => self.describe(row.clone(), &directory.path.join(&row.name)),
            ListedName::LocalOnly(entry) => DirectoryEntry::from_local_only(entry.clone()),
        }
    }

    /// What every entry `directory` holds is made from: the names and the kinds. The backing
    /// directory is read only for what the store does not have, the local-only entries.
    pub(in crate::fuse) fn list_names(&self, directory: &DirectoryReference) -> Vec<ListedName> {
        let unsent_changes = self.read_unsent_changes();
        let stored = directory.onedrive_id.as_deref().and_then(|id| self.read(|s| s.children(Table::Items, id))).unwrap_or_default();
        let mut rows: Vec<Row> = unsent_changes.moved_ids.iter().filter(|(_, (to, _))| directory.matches(to)).filter_map(|(id, (_, name))| self.find_placed_row(id, name)).collect();
        let moved_here: HashSet<String> = rows.iter().map(|row| row.name.clone()).collect();
        rows.extend(stored.into_iter().filter(|row| stays(row, &unsent_changes) && !moved_here.contains(&row.name)));
        let taken: HashSet<String> = rows.iter().map(|row| row.name.clone()).collect();
        let mut all: Vec<ListedName> = rows.into_iter().map(ListedName::InOneDrive).collect();
        for name in self.backing_directory.list_directory(&directory.path).map(|e| e.file_name()) {
            if let Some(name) = name.to_str().filter(|name| !taken.contains(*name)) {
                all.extend(self.find_local_only(directory, name).map(ListedName::LocalOnly));
            }
        }
        all
    }
}
