//! What each directory of the mount holds: every file and directory of OneDrive where the
//! store has it, with the unsent changes applied, and the files and directories of the
//! backing directory that were made here and are not in OneDrive yet.
//!
//! [`DirectoryContents`] owns the mount's reading connection to the store, opened for reading
//! alone — what it is asked never waits for the cycle that writes the store — and the
//! [`UnsentChanges`] as last read. It uses [`BackingDirectory`] for what only the disk knows:
//! the downloaded copies and the local-only entries. It is the one place that works out a
//! sync state ([`sync_states`]): from the store, the backing directory and the unsent
//! changes, once, as it finds an entry. It knows nothing of the kernel's node ids, of what
//! is open or of what is being downloaded, and changes nothing but, on request, the marks of
//! the directories ([`DirectoryContents::make_marked_directories`]).
//!
//! What it answers:
//!
//! - here: a row of the store ([`DirectoryContents::find_row`]), the row of the directory it is
//!   in ([`DirectoryContents::find_parent_row`]), the folder's own row and entry, the files at and
//!   below a row with their sync states ([`DirectoryContents::files_below`]), the OneDrive
//!   file the user just removed from a path ([`DirectoryContents::find_file_removed_at`]);
//! - [`sync_states`]: the sync state of a file and of a directory, and a directory entry;
//! - [`directory_entries`]: the entry a directory holds under a name, and what all its
//!   entries are made from;
//! - [`entry_paths`]: the path the mount shows a OneDrive id at, and the entry at a path.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use konedrive_tree::outbox::{OutboxKind, OutboxState};
use konedrive_tree::{Kind, Placement, Row, Table, TreeError, TreeStore};

use super::backing_directory::BackingDirectory;
use super::unsent_changes::UnsentChanges;

mod directory_entries;
mod entry_paths;
mod sync_states;

pub(super) use directory_entries::EntrySource;
pub(super) use sync_states::{DirectoryEntry, DirectorySyncState, EntryState, FileSyncState};

pub(super) struct DirectoryContents {
    /// The store, for reading alone.
    reader: Mutex<TreeStore>,
    /// The unsent changes, and the store's version they were read at; `None` before the
    /// first reading.
    unsent_changes: Mutex<(Option<i64>, Arc<UnsentChanges>)>,
    backing_directory: Arc<BackingDirectory>,
}

impl DirectoryContents {
    /// The contents of the folder whose disk is `backing_directory`, read through `reader`, a
    /// connection of its own to the folder's store.
    pub(super) fn new(reader: TreeStore, backing_directory: Arc<BackingDirectory>) -> Self {
        DirectoryContents { reader: Mutex::new(reader), unsent_changes: Mutex::default(), backing_directory }
    }

    /// The backing directory these are the contents of.
    pub(super) fn backing_directory(&self) -> &BackingDirectory {
        &self.backing_directory
    }

    /// One question to the store, for what has no named question here yet (`asked.rs`).
    /// `None` when it cannot be answered.
    pub(super) fn read<T>(&self, ask: impl FnOnce(&TreeStore) -> Result<T, TreeError>) -> Option<T> {
        ask(&crate::panic::lock(&self.reader)).ok()
    }

    /// The row of the OneDrive file the user removed from `rel` and whose removal is not sent
    /// yet, as the store has it: a file made there now is that file's new content.
    pub(super) fn find_file_removed_at(&self, rel: &Path) -> Option<Row> {
        let rows = self.read(|s| s.outbox_at_or_under(rel))?;
        let removed = rows.into_iter().find(|row| row.rel == rel && row.kind == OutboxKind::Delete && row.state != OutboxState::Running)?;
        self.find_row(&removed.item_id?).filter(|base| base.kind == Kind::File)
    }

    /// The row of the OneDrive id `id`, as the store has it.
    pub(super) fn find_row(&self, id: &str) -> Option<Row> {
        self.read(|s| s.get(Table::Items, id)).flatten()
    }

    /// The row of the directory `row` is in, as the store has it.
    pub(super) fn find_parent_row(&self, row: &Row) -> Option<Row> {
        row.parent_id.as_deref().and_then(|parent| self.find_row(parent))
    }

    /// The OneDrive id of the folder itself: the root of the drive.
    pub(super) fn find_root_id(&self) -> Option<String> {
        self.read(|s| s.root_item_id()).flatten()
    }

    /// The folder itself, as a row: without a name, and without an id before the store has
    /// the drive's root.
    pub(super) fn root_row(&self) -> Row {
        let id = self.find_root_id().unwrap_or_default();
        Row { id, parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed }
    }

    /// The folder itself, as an entry: a directory of OneDrive, without a name.
    pub(super) fn root_entry(&self) -> DirectoryEntry {
        DirectoryEntry { name: String::new(), state: EntryState::Directory(DirectorySyncState::Synced(self.root_row())) }
    }

    /// The unsent changes now. Read again only when the store changed since.
    pub(super) fn unsent_changes(&self) -> Arc<UnsentChanges> {
        let store = crate::panic::lock(&self.reader);
        let mut kept = crate::panic::lock(&self.unsent_changes);
        // A version that cannot be read is never the one kept.
        let version = store.data_version().ok();
        if version.is_none() || version != kept.0 {
            *kept = (version, Arc::new(UnsentChanges::of(&store)));
        }
        Arc::clone(&kept.1)
    }

    /// The files of OneDrive at and below `row`, each with its sync state and its path in the
    /// folder.
    pub(super) fn files_below(&self, row: &Row) -> Vec<(FileSyncState, PathBuf)> {
        // One question at a time: the mount's requests read through the same connection, and
        // none waits for a whole walk.
        let mut ids = self.read(|s| s.descendants(Table::Items, &row.id)).unwrap_or_default();
        ids.push(row.id.clone());
        let files: Vec<Row> = ids.iter().filter_map(|id| self.find_row(id).filter(|r| r.kind == Kind::File)).collect();
        let placed: Vec<(Row, PathBuf)> = files.into_iter().filter_map(|file| self.find_path(&file.id).map(|at| (file, at))).collect();
        let unsent_changes = self.unsent_changes();
        placed.into_iter().map(|(file, at)| (self.file_sync_state(file, &at, &unsent_changes), at)).collect()
    }

    /// The sync state of the OneDrive file `row`, which the mount shows at `rel`: from its
    /// downloaded copy, when the backing directory holds one, and from the unsent changes.
    fn file_sync_state(&self, row: Row, rel: &Path, unsent_changes: &UnsentChanges) -> FileSyncState {
        let downloaded_copy = self.backing_directory.find_downloaded_copy(rel, &row.id);
        let changed_here = unsent_changes.changed.contains(&row.id);
        FileSyncState::of_onedrive_file(row, downloaded_copy, changed_here)
    }
}

#[cfg(test)]
pub(super) mod tests;
