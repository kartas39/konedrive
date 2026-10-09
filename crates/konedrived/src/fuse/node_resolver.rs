//! The entry a node id stands for, and the node id of an entry.
//!
//! The kernel names what it asks about by node id. [`NodeTable`] is the table alone: it
//! knows a key and a place for each node id, and nothing of the store or the disk.
//! [`NodeResolver`] is the part over it, the [`DirectoryContents`] and, for the file handle
//! of what is at a path, the [`BackingDirectory`]: it gives an entry its
//! node id, finds the entry a node id stands for now — what is in OneDrive is looked up again
//! in the directory contents, a local-only entry by its file handle in the backing directory
//! — and keeps the node id of a local-only entry once it is in OneDrive.
//!
//! The node resolver owns the table: whoever needs a node id, or changes a node's key or
//! place, asks here. A node's place has one kind of writer — whoever found or put the entry
//! there: a lookup of its name ([`NodeResolver::assign_node_id`], which moves a node only to
//! where the mount shows it now), a make, a rename ([`NodeResolver::move_node`]), and a
//! cycle that moves a copy a program writes aside
//! ([`NodeResolver::move_node_onto_local_only_entry`]). A
//! reading of a directory gives a name its node id and never a node its place
//! ([`NodeResolver::assign_node_id_of_listed_name`]). What a cycle of the delta listing
//! moved is at its new place once its name is looked up there.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use konedrive_fs::handle::FileHandle;

use crate::fuse::entry_place::EntryPlace;
use crate::fuse::local_only_entry::LocalOnlyEntry;
use super::backing_directory::{BackingDirectory, DiskEntry, EntryKind};
use super::directory_reference::DirectoryReference;
use super::directory_contents::{DirectoryContents, DirectoryEntry, DirectorySyncState, ListedName, EntryState, FileSyncState};
use super::node_table::{NodeId, NodeKey, NodeTable};

/// What a node id stands for, found again in the directory contents or on disk.
enum NodeTarget {
    InOneDrive { entry: DirectoryEntry, path: PathBuf },
    /// A local-only entry, with its name in the directory node `parent`: its place is not
    /// worked out yet.
    LocalOnly { on_disk: DiskEntry, path: PathBuf, name: String, parent: NodeId },
}

/// The key the node of `entry` has in the table.
fn key_of(entry: &DirectoryEntry) -> NodeKey {
    match &entry.state {
        EntryState::File(FileSyncState::LocalOnly(local_only)) | EntryState::Directory(DirectorySyncState::LocalOnly(local_only)) => NodeKey::LocalOnly(local_only.on_disk.handle.clone()),
        EntryState::File(FileSyncState::RemoteOnly(row) | FileSyncState::Synced(row, _) | FileSyncState::ModifiedHere(row, _)) | EntryState::Directory(DirectorySyncState::InOneDrive(row)) => NodeKey::InOneDrive(row.id.clone()),
    }
}

pub(super) struct NodeResolver {
    directory_contents: Arc<DirectoryContents>,
    /// For the file handle of what is at a path: a local-only entry is that handle.
    backing_directory: Arc<BackingDirectory>,
    node_table: Mutex<NodeTable>,
}

impl NodeResolver {
    /// The node resolver of a folder the kernel was told nothing of yet.
    pub(super) fn new(directory_contents: Arc<DirectoryContents>, backing_directory: Arc<BackingDirectory>) -> Self {
        NodeResolver { directory_contents, backing_directory, node_table: Mutex::new(NodeTable::new()) }
    }

    /// The node id of `entry`, which whoever asks found in the directory node `parent` — a
    /// lookup of its name, a make, a rename: the one it has, or a new one. A local-only
    /// entry that got into OneDrive since the kernel was told of it keeps its node id.
    ///
    /// A node the table has at another place moves to this one only when the mount shows
    /// the entry here now: a file or directory of OneDrive where the directory contents have
    /// its id, a local-only entry where its file handle is. The question and the move are
    /// one step under the table's lock, and a rename moves its node after it is recorded:
    /// an entry that was read before a rename, or before a cycle moved it, and is given its
    /// node id after, does not put the node back.
    pub(super) fn assign_node_id(&self, entry: &DirectoryEntry, parent: NodeId) -> NodeId {
        let key = key_of(entry);
        let mut node_table = crate::panic::lock(&self.node_table);
        let node_id = self.assign_node_id_of_key(&mut node_table, key.clone(), &entry.name, parent);
        if !node_table.is_at(node_id, parent, &entry.name) && self.is_shown_at(&node_table, &key, parent, &entry.name) {
            node_table.move_node(node_id, parent, &entry.name);
        }
        node_id
    }

    /// The node id of what `listed_name` names, which a reading of the directory node
    /// `parent` listed: the node id its entry has, or a new one, given without the entry
    /// being made. A reading never changes the place of a node the table has: the names of a
    /// reading in parts are those of its start, and what was renamed or moved since is where
    /// the rename or the move put it.
    pub(super) fn assign_node_id_of_listed_name(&self, listed_name: &ListedName, parent: NodeId) -> NodeId {
        let key = match listed_name {
            ListedName::InOneDrive(row) => NodeKey::InOneDrive(row.id.clone()),
            ListedName::LocalOnly(local_only) => NodeKey::LocalOnly(local_only.on_disk.handle.clone()),
        };
        self.assign_node_id_of_key(&mut crate::panic::lock(&self.node_table), key, listed_name.name(), parent)
    }

    /// The node id of `key` in `node_table`: the one it has, or a new one, whose node is
    /// `name` in the directory node `parent`. A OneDrive id the kernel was not told of yet
    /// is looked for on disk once, by its path: a local-only entry that got into OneDrive
    /// keeps the node id it had.
    fn assign_node_id_of_key(&self, node_table: &mut NodeTable, key: NodeKey, name: &str, parent: NodeId) -> NodeId {
        if matches!(key, NodeKey::InOneDrive(_)) && !node_table.has_key(&key) {
            let on_disk = node_table.find_path(parent).and_then(|directory| self.backing_directory.find_file_handle(&directory.join(name)));
            if let Some(handle) = on_disk {
                node_table.rekey(&NodeKey::LocalOnly(handle), key.clone());
            }
        }
        node_table.assign(key, parent, name)
    }

    /// Whether the mount shows what `key` stands for as `name` in the directory node
    /// `parent` now: the directory contents have a OneDrive id at that path, and a
    /// local-only entry is the file handle the backing directory has there.
    fn is_shown_at(&self, node_table: &NodeTable, key: &NodeKey, parent: NodeId, name: &str) -> bool {
        let Some(path) = node_table.find_path(parent).map(|directory| directory.join(name)) else { return false };
        match key {
            NodeKey::InOneDrive(onedrive_id) => self.directory_contents.find_path(onedrive_id).as_deref() == Some(path.as_path()),
            NodeKey::LocalOnly(handle) => self.backing_directory.find_file_handle(&path).as_ref() == Some(handle),
        }
    }

    /// What the node `node_id`, which is not the root, stands for now.
    fn find_node(&self, node_id: NodeId) -> Option<NodeTarget> {
        let (key, path, name, parent) = {
            let node_table = crate::panic::lock(&self.node_table);
            let record = node_table.find_record(node_id)?;
            (record.key.clone(), node_table.find_path(node_id)?, record.name.clone(), record.parent_node_id)
        };
        match key {
            NodeKey::InOneDrive(id) => {
                let entry = self.directory_contents.find_entry_by_id(&id, &name, &path).filter(|_| !self.directory_contents.is_removed_here(&id))?;
                Some(NodeTarget::InOneDrive { entry, path })
            }
            NodeKey::LocalOnly(handle) => {
                // Whether it is local-only still is the directory contents' to say, by the
                // one rule a lookup of its name goes by.
                match self.directory_contents.find_local_only_at(&path) {
                    Some(on_disk) if on_disk.handle == handle => Some(NodeTarget::LocalOnly { on_disk, path, name, parent }),
                    // Another file or directory at its path is not this node.
                    Some(_) => None,
                    None => {
                        self.backing_directory.find_file_handle(&path).filter(|at_path| *at_path == handle)?;
                        // It got into OneDrive since: the node stands for what is in
                        // OneDrive from now on.
                        let entry = self.directory_contents.find_entry_by_mark(&path, &name)?;
                        crate::panic::lock(&self.node_table).rekey(&NodeKey::LocalOnly(handle), key_of(&entry));
                        Some(NodeTarget::InOneDrive { entry, path })
                    }
                }
            }
        }
    }

    /// The entry the node `node_id` stands for now, and its path in the folder. `None` for a
    /// node whose file or directory is gone.
    pub(super) fn find_entry(&self, node_id: NodeId) -> Option<(DirectoryEntry, PathBuf)> {
        if node_id == NodeId::ROOT {
            return Some((self.directory_contents.find_root_entry(), PathBuf::new()));
        }
        match self.find_node(node_id)? {
            NodeTarget::InOneDrive { entry, path } => Some((entry, path)),
            NodeTarget::LocalOnly { on_disk, path, name, parent } => {
                // Its place is in the directory its node is in. A directory that is not
                // found any more is referred to by its path alone.
                let directory = self.find_directory(parent).unwrap_or_else(|| DirectoryReference { onedrive_id: None, path: path.parent().map(Path::to_owned).unwrap_or_default() });
                Some((DirectoryEntry::from_local_only(LocalOnlyEntry { on_disk, place: EntryPlace::new(&directory, &name) }), path))
            }
        }
    }

    /// The directory the node `node_id` stands for.
    pub(super) fn find_directory(&self, node_id: NodeId) -> Option<DirectoryReference> {
        if node_id == NodeId::ROOT {
            return Some(self.directory_contents.find_root_directory());
        }
        match self.find_node(node_id)? {
            NodeTarget::InOneDrive { entry, path } => entry.to_directory_reference(path),
            NodeTarget::LocalOnly { on_disk, path, .. } => (on_disk.metadata.kind == EntryKind::Directory).then_some(DirectoryReference { onedrive_id: None, path: path }),
        }
    }

    /// The directory the node `node_id` is in, and that directory's node id.
    pub(super) fn find_parent(&self, node_id: NodeId) -> Option<(NodeId, DirectoryReference)> {
        let parent = crate::panic::lock(&self.node_table).find_record(node_id)?.parent_node_id;
        Some((parent, self.find_directory(parent)?))
    }

    /// The node id that stands for the OneDrive id `id`, when the kernel was told of one.
    pub(super) fn find_node_id(&self, id: &str) -> Option<NodeId> {
        crate::panic::lock(&self.node_table).find_node_id(&NodeKey::InOneDrive(id.to_owned()))
    }

    /// The node `node_id` is `name` in the directory node `parent` now.
    pub(super) fn move_node(&self, node_id: NodeId, parent: NodeId, name: &str) {
        crate::panic::lock(&self.node_table).move_node(node_id, parent, name);
    }

    /// The node `node_id`, of the local-only file `handle` identifies, is `name` in the
    /// directory node `parent` now, where it took over the name of the OneDrive file
    /// `onedrive_id`: it stands for that file from here on — whoever looks the name up gets
    /// the node the new content is read and written through.
    pub(super) fn move_node_onto_onedrive_file(&self, node_id: NodeId, parent: NodeId, name: &str, handle: &FileHandle, onedrive_id: &str) {
        let mut node_table = crate::panic::lock(&self.node_table);
        node_table.move_node(node_id, parent, name);
        node_table.rekey(&NodeKey::LocalOnly(handle.clone()), NodeKey::InOneDrive(onedrive_id.to_owned()));
    }

    /// The node that stands for the OneDrive file `onedrive_id` stands, from here on, for
    /// the local-only entry `handle` identifies, which is `name` in the directory the node is
    /// in: the file's downloaded copy, which a program writes, was moved aside as the user's
    /// own, and the node goes with the file the program holds open. Whoever looks the
    /// OneDrive file's name up gets a node of its own. Nothing when the kernel was told of no
    /// node for the id.
    pub(super) fn move_node_onto_local_only_entry(&self, onedrive_id: &str, handle: &FileHandle, name: &str) {
        let mut node_table = crate::panic::lock(&self.node_table);
        let key = NodeKey::InOneDrive(onedrive_id.to_owned());
        let Some((node_id, parent)) = node_table.find_node_id(&key).and_then(|node_id| Some((node_id, node_table.find_record(node_id)?.parent_node_id))) else { return };
        node_table.move_node(node_id, parent, name);
        node_table.rekey(&key, NodeKey::LocalOnly(handle.clone()));
    }

    /// The local-only entry `handle` identifies is gone, and nothing else is ever that entry:
    /// its node is dropped, and nothing is found by it again. A program that still holds the
    /// file open goes on through its own descriptor, which needs no node; the kernel's later
    /// questions about the node id are answered as of something gone, as they were once the
    /// entry left the disk.
    pub(super) fn drop_node(&self, handle: &FileHandle) {
        crate::panic::lock(&self.node_table).drop_node(&NodeKey::LocalOnly(handle.clone()));
    }
}

#[cfg(test)]
mod tests;
