//! The entry a node id stands for, and the node id of an entry.
//!
//! The kernel names what it asks about by node id. [`NodeTable`] is the table alone: it
//! knows a key and a place for each node id, and nothing of the store or the disk.
//! [`NodeLookup`] is the part over it and the [`DirectoryContents`]: it gives an entry its
//! node id, finds the entry a node id stands for now — what is in OneDrive is looked up again
//! in the directory contents, a local-only entry by its file handle in the backing directory
//! — and keeps the node id of a local-only entry once it is in OneDrive.
//!
//! The node lookup owns the table: whoever needs a node id, or changes a node's key or
//! place, asks here.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use konedrive_fs::handle::FileHandle;

use super::backing_directory::{DiskEntry, EntryKind};
use super::detections::LocalOnlyEntry;
use super::directory::Directory;
use super::directory_contents::{DirectoryContents, DirectoryEntry, DirectorySyncState, EntrySource, EntryState, FileSyncState};
use super::local_only_entries::folder_place;
use super::node_table::{NodeKey, NodeTable};

/// What a node id stands for, found again in the directory contents or on disk.
enum NodeTarget {
    InOneDrive { entry: DirectoryEntry, rel: PathBuf },
    /// A local-only entry, with its name in the directory node `parent`: its place is not
    /// worked out yet.
    LocalOnly { on_disk: DiskEntry, rel: PathBuf, name: String, parent: u64 },
}

/// The key the node of `entry` has in the table.
fn key_of(entry: &DirectoryEntry) -> NodeKey {
    match &entry.state {
        EntryState::File(FileSyncState::LocalOnly(local_only)) | EntryState::Directory(DirectorySyncState::LocalOnly(local_only)) => NodeKey::LocalOnly(local_only.on_disk.handle.clone()),
        EntryState::File(FileSyncState::RemoteOnly(row) | FileSyncState::Synced(row, _) | FileSyncState::ModifiedHere(row, _)) | EntryState::Directory(DirectorySyncState::Synced(row)) => NodeKey::InOneDrive(row.id.clone()),
    }
}

pub(super) struct NodeLookup {
    directory_contents: Arc<DirectoryContents>,
    node_table: Mutex<NodeTable>,
}

impl NodeLookup {
    /// The node lookup of a folder the kernel was told nothing of yet.
    pub(super) fn new(directory_contents: Arc<DirectoryContents>) -> Self {
        NodeLookup { directory_contents, node_table: Mutex::new(NodeTable::new()) }
    }

    /// The node id of `entry`, which is in the directory node `parent`: the one it has, or a
    /// new one. A local-only entry that got into OneDrive since the kernel was told of it
    /// keeps its node id.
    pub(super) fn assign_node_id(&self, entry: &DirectoryEntry, parent: u64) -> u64 {
        self.assign_node_id_of_key(key_of(entry), &entry.name, parent)
    }

    /// The node id of what `source` is the source of, which is in the directory node
    /// `parent`: the node id its entry has ([`assign_node_id`](Self::assign_node_id)), given
    /// without the entry being made.
    pub(super) fn assign_node_id_of_source(&self, source: &EntrySource, parent: u64) -> u64 {
        let key = match source {
            EntrySource::InOneDrive(row) => NodeKey::InOneDrive(row.id.clone()),
            EntrySource::LocalOnly(local_only) => NodeKey::LocalOnly(local_only.on_disk.handle.clone()),
        };
        self.assign_node_id_of_key(key, source.name(), parent)
    }

    /// The node id of `key`, which is `name` in the directory node `parent`. A OneDrive id
    /// the kernel was not told of yet is looked for on disk once, by its path: a local-only
    /// entry that got into OneDrive keeps the node id it had.
    fn assign_node_id_of_key(&self, key: NodeKey, name: &str, parent: u64) -> u64 {
        let mut node_table = crate::panic::lock(&self.node_table);
        if matches!(key, NodeKey::InOneDrive(_)) && !node_table.node_ids.contains_key(&key) {
            let on_disk = node_table.path_of(parent).and_then(|directory| self.directory_contents.backing_directory().find_file_handle(&directory.join(name)));
            if let Some(handle) = on_disk {
                node_table.rekey(&NodeKey::LocalOnly(handle), key.clone());
            }
        }
        node_table.assign(key, parent, name)
    }

    /// What the node `node_id`, which is not the root, stands for now.
    fn find_node(&self, node_id: u64) -> Option<NodeTarget> {
        let (key, rel, name, parent) = {
            let node_table = crate::panic::lock(&self.node_table);
            let node = node_table.nodes.get(&node_id)?;
            (node.key.clone(), node_table.path_of(node_id)?, node.place.1.clone(), node.place.0)
        };
        match key {
            NodeKey::InOneDrive(id) => {
                let entry = self.directory_contents.find_entry_by_id(&id, &name, &rel).filter(|_| !self.directory_contents.unsent_changes().removed.contains(&id))?;
                Some(NodeTarget::InOneDrive { entry, rel })
            }
            NodeKey::LocalOnly(handle) => {
                // Another file or directory at its path is not this node.
                let on_disk = self.directory_contents.backing_directory().find_disk_entry(&rel).filter(|on_disk| on_disk.handle == handle)?;
                // It got into OneDrive since: the node stands for what is in OneDrive from
                // now on.
                let in_onedrive = self.directory_contents.backing_directory().read_onedrive_id(&rel).and_then(|id| self.directory_contents.find_entry_by_id(&id, &name, &rel));
                if let Some(entry) = in_onedrive {
                    crate::panic::lock(&self.node_table).rekey(&NodeKey::LocalOnly(handle), key_of(&entry));
                    return Some(NodeTarget::InOneDrive { entry, rel });
                }
                Some(NodeTarget::LocalOnly { on_disk, rel, name, parent })
            }
        }
    }

    /// The entry the node `node_id` stands for now, and its path in the folder. `None` for a
    /// node whose file or directory is gone.
    pub(super) fn find_entry(&self, node_id: u64) -> Option<(DirectoryEntry, PathBuf)> {
        if node_id == 1 {
            return Some((self.directory_contents.root_entry(), PathBuf::new()));
        }
        match self.find_node(node_id)? {
            NodeTarget::InOneDrive { entry, rel } => Some((entry, rel)),
            NodeTarget::LocalOnly { on_disk, rel, name, parent } => {
                // Its place is in the directory its node is in. A directory that is not
                // found any more is referred to by its path alone.
                let directory = self.find_directory(parent).unwrap_or_else(|| Directory { id: None, rel: rel.parent().map(Path::to_owned).unwrap_or_default() });
                Some((DirectoryEntry::from_local_only(LocalOnlyEntry { on_disk, place: folder_place(&directory, &name) }), rel))
            }
        }
    }

    /// The directory the node `node_id` stands for.
    pub(super) fn find_directory(&self, node_id: u64) -> Option<Directory> {
        if node_id == 1 {
            return Some(Directory { id: self.directory_contents.root_id().filter(|id| !id.is_empty()), rel: PathBuf::new() });
        }
        match self.find_node(node_id)? {
            NodeTarget::InOneDrive { entry, rel } => matches!(entry.state, EntryState::Directory(_)).then(|| Directory { id: entry.onedrive_id().filter(|id| !id.is_empty()).map(str::to_owned), rel }),
            NodeTarget::LocalOnly { on_disk, rel, .. } => (on_disk.metadata.kind == EntryKind::Directory).then_some(Directory { id: None, rel }),
        }
    }

    /// The directory the node `node_id` is in, and that directory's node id.
    pub(super) fn find_parent(&self, node_id: u64) -> Option<(u64, Directory)> {
        let parent = crate::panic::lock(&self.node_table).nodes.get(&node_id)?.place.0;
        Some((parent, self.find_directory(parent)?))
    }

    /// The node id that stands for the OneDrive id `id`, when the kernel was told of one.
    pub(super) fn find_node_id(&self, id: &str) -> Option<u64> {
        crate::panic::lock(&self.node_table).node_ids.get(&NodeKey::InOneDrive(id.to_owned())).copied()
    }

    /// The node `node_id` is `name` in the directory node `parent` now. `taken`: it is the
    /// local-only file, by its handle, that took over the name of this OneDrive id, and
    /// stands for that file of OneDrive from here on — whoever looks the name up gets the
    /// node the new content is read and written through.
    pub(super) fn move_node(&self, node_id: u64, parent: u64, name: &str, taken: Option<(&FileHandle, &str)>) {
        let mut node_table = crate::panic::lock(&self.node_table);
        node_table.move_node(node_id, parent, name);
        if let Some((handle, id)) = taken {
            node_table.rekey(&NodeKey::LocalOnly(handle.clone()), NodeKey::InOneDrive(id.to_owned()));
        }
    }

    /// The local-only entry `handle` identifies is gone, and nothing else is ever that entry:
    /// its node is dropped, and nothing is found by it again. A program that still holds the
    /// file open goes on through its own descriptor, which needs no node; the kernel's later
    /// questions about the node id are answered as of something gone, as they were once the
    /// entry left the disk.
    pub(super) fn forget(&self, handle: &FileHandle) {
        crate::panic::lock(&self.node_table).forget(&NodeKey::LocalOnly(handle.clone()));
    }
}

#[cfg(test)]
mod tests;
