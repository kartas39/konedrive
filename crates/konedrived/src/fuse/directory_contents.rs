//! The folder as the mount shows it: every item where the store has it, with what the
//! outbox's rows say laid over that, and the files and directories of the folder under the
//! mount that OneDrive has no item for yet.
//!
//! [`DirectoryContents`] owns the mount's reading connection to the store, opened for reading alone — what
//! it is asked never waits for the cycle that writes the store — and the [`UnsentChanges`] as last
//! read. It uses [`BackingDirectory`] for what only the disk knows: the objects without an item. It
//! knows nothing of the kernel's nodes, of what is open or of what is being downloaded, and
//! changes nothing but, on request, the marks of the directories ([`DirectoryContents::backing`]).
//!
//! What it answers:
//!
//! - here: an item's row ([`DirectoryContents::row`]), the row above it ([`DirectoryContents::parent`]), the folder's
//!   own item ([`DirectoryContents::root`]), the files at and below an item ([`DirectoryContents::files_below`]), the
//!   item whose file the user just removed from a place ([`DirectoryContents::find_file_removed_at`]);
//! - [`entries`]: what a directory holds under a name and in whole ([`Entry`], [`Directory`]);
//! - [`places`]: where the folder shows an item, and what stands at a path.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use konedrive_tree::outbox::{OutboxKind, OutboxState};
use konedrive_tree::{Kind, Placement, Row, Table, TreeError, TreeStore};

use super::unsent_changes::UnsentChanges;
use super::backing_directory::BackingDirectory;

mod entries;
mod places;

pub(super) use entries::{Directory, Entry, Local};

pub(super) struct DirectoryContents {
    /// The store, for reading alone.
    reader: Mutex<TreeStore>,
    /// What the outbox's rows say, and the store's version that was read at; `None` before
    /// the first reading.
    unsent_changes: Mutex<(Option<i64>, Arc<UnsentChanges>)>,
    backing_directory: Arc<BackingDirectory>,
}

impl DirectoryContents {
    /// The view of the folder `under` through `reader`, a connection of its own to the
    /// folder's store.
    pub(super) fn new(reader: TreeStore, backing_directory: Arc<BackingDirectory>) -> Self {
        DirectoryContents { reader: Mutex::new(reader), unsent_changes: Mutex::default(), backing_directory }
    }

    /// The folder under the mount this is a view of.
    pub(super) fn backing_directory(&self) -> &BackingDirectory {
        &self.backing_directory
    }

    /// One question to the store, for what has no named question here yet (`asked.rs`).
    /// `None` when it cannot be answered.
    pub(super) fn read<T>(&self, ask: impl FnOnce(&TreeStore) -> Result<T, TreeError>) -> Option<T> {
        ask(&crate::panic::lock(&self.reader)).ok()
    }

    /// The item whose file the user removed from `rel` and whose removal is not sent yet, as
    /// the store has it: a file made there now is that item's new content.
    pub(super) fn find_file_removed_at(&self, rel: &Path) -> Option<Row> {
        let rows = self.read(|s| s.outbox_at_or_under(rel))?;
        let removed = rows.into_iter().find(|row| row.rel == rel && row.kind == OutboxKind::Delete && row.state != OutboxState::Running)?;
        self.row(&removed.item_id?).filter(|base| base.kind == Kind::File)
    }

    /// The item `id`, as the store has it.
    pub(super) fn row(&self, id: &str) -> Option<Row> {
        self.read(|s| s.get(Table::Items, id)).flatten()
    }

    /// The item `row` lies in, as the store has it.
    pub(super) fn parent(&self, row: &Row) -> Option<Row> {
        row.parent_id.as_deref().and_then(|above| self.row(above))
    }

    /// The id of the folder's own item: the root of the drive.
    pub(super) fn root_id(&self) -> Option<String> {
        self.read(|s| s.root_item_id()).flatten()
    }

    /// The folder itself, as an item: without a name, and without an id before the store has
    /// the drive's root.
    pub(super) fn root(&self) -> Row {
        let id = self.root_id().unwrap_or_default();
        Row { id, parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed }
    }

    /// What the outbox says now. Read again only when the store changed since.
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

    /// The files at and below `row`, each with its place in the folder.
    pub(super) fn files_below(&self, row: &Row) -> Vec<(Row, PathBuf)> {
        // One question at a time: the mount's requests read through the same connection, and
        // none waits for a whole walk.
        let mut ids = self.read(|s| s.descendants(Table::Items, &row.id)).unwrap_or_default();
        ids.push(row.id.clone());
        let files: Vec<Row> = ids.iter().filter_map(|id| self.row(id).filter(|r| r.kind == Kind::File)).collect();
        files.into_iter().filter_map(|file| self.find_path(&file.id).map(|at| (file, at))).collect()
    }
}

#[cfg(test)]
pub(super) mod tests;
