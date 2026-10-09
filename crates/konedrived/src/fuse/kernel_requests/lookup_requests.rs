//! A name looked up, and the attributes of a node and of the filesystem (`lookup`,
//! `getattr`, `access`, `statfs`), and the answers the inotify bridge's own requests get.
//!
//! [`LookupRequests`] is given the [`NodeResolver`] and the [`DirectoryContents`], which say
//! what a node and a name stand for, the [`AccessModeSwitch`], which says the modes shown,
//! the [`InotifyBridge`], which says what a name it announces was before the change, and the
//! [`BackingDirectory`], whose filesystem's space the mount shows.

use std::ffi::OsStr;
use std::sync::Arc;
use std::time::Duration;

use fuser::{AccessFlags, Errno, FileAttr, Generation, ReplyAttr, ReplyEmpty, ReplyEntry, ReplyStatfs};
use konedrive_tree::Kind;

use crate::fuse::access_mode_switch::{AccessMode, AccessModeSwitch};
use crate::fuse::attributes::{to_attributes, TTL};
use crate::fuse::backing_directory::BackingDirectory;
use crate::fuse::directory_contents::{make_bare_row, DirectoryContents, DirectoryEntry, DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::errno::to_errno;
use crate::fuse::inotify_bridge::{InotifyBridge, NameBeforeChange};
use crate::fuse::node_resolver::NodeResolver;
use crate::fuse::node_table::NodeId;

/// The entry of a name that is gone from the store, for the one request that still has to be
/// told it is there: its kind, and nothing else.
fn make_vanished_entry(name: &str, kind: Kind) -> DirectoryEntry {
    let row = make_bare_row(String::new(), name.to_owned(), kind);
    let state = match kind {
        Kind::Folder => EntryState::Directory(DirectorySyncState::InOneDrive(row)),
        Kind::File => EntryState::File(FileSyncState::RemoteOnly(row)),
    };
    DirectoryEntry { name: name.to_owned(), state }
}

/// Answers with the attributes of the node `node_id`, as the folder has it now in
/// `access_mode`: the answer of `getattr`, and of a `setattr` that was made.
pub(super) fn answer_with_attributes(node_resolver: &NodeResolver, access_mode: AccessMode, node_id: NodeId, reply: ReplyAttr) {
    match node_resolver.find_entry(node_id) {
        Some((entry, _)) => reply.attr(&TTL, &to_attributes(node_id, access_mode, &entry)),
        None => reply.error(Errno::ENOENT),
    }
}

#[derive(Clone)]
pub(in crate::fuse) struct LookupRequests {
    node_resolver: Arc<NodeResolver>,
    directory_contents: Arc<DirectoryContents>,
    access_mode_switch: Arc<AccessModeSwitch>,
    inotify_bridge: Arc<InotifyBridge>,
    backing_directory: Arc<BackingDirectory>,
}

impl LookupRequests {
    pub(in crate::fuse) fn new(node_resolver: Arc<NodeResolver>, directory_contents: Arc<DirectoryContents>, access_mode_switch: Arc<AccessModeSwitch>, inotify_bridge: Arc<InotifyBridge>, backing_directory: Arc<BackingDirectory>) -> Self {
        LookupRequests { node_resolver, directory_contents, access_mode_switch, inotify_bridge, backing_directory }
    }

    /// The attributes of `entry`, which the node `node_id` stands for, in the folder's access
    /// mode.
    fn attributes_of(&self, entry: &DirectoryEntry, node_id: NodeId) -> FileAttr {
        to_attributes(node_id, self.access_mode_switch.access_mode(), entry)
    }

    /// The entry the directory node `parent` holds under `name`, as the folder has it now.
    fn find_entry(&self, parent: NodeId, name: &OsStr) -> Option<DirectoryEntry> {
        let (directory, name) = self.node_resolver.find_directory(parent).zip(name.to_str())?;
        self.directory_contents.find_entry(&directory, name)
    }

    /// The attributes of the entry `name` of `parent`, as the folder has it now, which is
    /// given its node id; `ENOENT` when the directory holds no such name.
    pub(super) fn find_entry_attributes(&self, parent: NodeId, name: &OsStr) -> Result<FileAttr, Errno> {
        let entry = self.find_entry(parent, name).ok_or(Errno::ENOENT)?;
        let node_id = self.node_resolver.assign_node_id(&entry, parent);
        Ok(self.attributes_of(&entry, node_id))
    }

    /// What a lookup of `name` in `parent` is answered: the attributes, and for how long
    /// the kernel may keep them. The inotify bridge's thread (`is_from_inotify_bridge`) is
    /// answered what the name it announces was before the change — absent (`ENOENT`), or
    /// still there with its node and kind, not to be kept; everyone else, and the bridge
    /// for every other name, what the folder has now.
    pub(super) fn find_lookup_answer(&self, is_from_inotify_bridge: bool, parent: NodeId, name: &OsStr) -> Result<(FileAttr, Duration), Errno> {
        let before_change = if is_from_inotify_bridge { self.inotify_bridge.find_name_before_change(parent, name) } else { None };
        match before_change {
            Some(NameBeforeChange::Absent) => Err(Errno::ENOENT),
            Some(NameBeforeChange::Present { node_id, kind }) => Ok((self.attributes_of(&make_vanished_entry(&name.to_string_lossy(), kind), node_id), Duration::ZERO)),
            None => Ok((self.find_entry_attributes(parent, name)?, TTL)),
        }
    }

    /// Whether a remove the inotify bridge announces is done: the name is gone from the
    /// store already. One the folder still has is not removed for the bridge (`EROFS`).
    pub(super) fn check_announced_remove(&self, parent: NodeId, name: &OsStr) -> Result<(), Errno> {
        match self.find_entry(parent, name) {
            None => Ok(()),
            Some(_) => Err(Errno::EROFS),
        }
    }

    /// Whether the caller may do what `mask` asks: anything, but write to a read-only
    /// folder.
    pub(super) fn check_access(&self, mask: AccessFlags) -> Result<(), Errno> {
        match mask.contains(AccessFlags::W_OK) && self.access_mode_switch.access_mode() == AccessMode::ReadOnly {
            true => Err(Errno::EROFS),
            false => Ok(()),
        }
    }

    /// Answers with the entry `name` of `parent`, as the folder has it now.
    pub(super) fn answer_with_entry(&self, parent: NodeId, name: &OsStr, reply: ReplyEntry) {
        match self.find_entry_attributes(parent, name) {
            Ok(attributes) => reply.entry(&TTL, &attributes, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    pub(super) fn answer_lookup(&self, is_from_inotify_bridge: bool, parent: NodeId, name: &OsStr, reply: ReplyEntry) {
        match self.find_lookup_answer(is_from_inotify_bridge, parent, name) {
            Ok((attributes, ttl)) => reply.entry(&ttl, &attributes, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    pub(super) fn answer_getattr(&self, node_id: NodeId, reply: ReplyAttr) {
        answer_with_attributes(&self.node_resolver, self.access_mode_switch.access_mode(), node_id, reply)
    }

    /// Answers a remove the inotify bridge announces ([`check_announced_remove`](Self::check_announced_remove)).
    pub(super) fn answer_announced_remove(&self, parent: NodeId, name: &OsStr, reply: ReplyEmpty) {
        match self.check_announced_remove(parent, name) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    pub(super) fn answer_access(&self, mask: AccessFlags, reply: ReplyEmpty) {
        match self.check_access(mask) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    /// The space of the filesystem the backing directory is on.
    pub(super) fn answer_statfs(&self, reply: ReplyStatfs) {
        match self.backing_directory.measure_space() {
            Ok(v) => reply.statfs(v.f_blocks, v.f_bfree, v.f_bavail, v.f_files, v.f_ffree, v.f_bsize as u32, 255, v.f_frsize as u32),
            Err(e) => reply.error(to_errno(&e)),
        }
    }
}

#[cfg(test)]
mod tests;
