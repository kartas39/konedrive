//! A name looked up, and the attributes of a node (`lookup`, `getattr`).

use std::ffi::OsStr;
use std::time::Duration;

use fuser::{Errno, FileAttr, Generation, INodeNo, ReplyAttr, ReplyEntry, Request};
use konedrive_tree::{Kind, Placement, Row};

use super::KernelRequests;
use crate::fuse::attributes::{attributes, TTL};
use crate::fuse::directory_contents::{DirectoryEntry, DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::inotify_bridge::NameBeforeChange;

/// The entry of a name that is gone from the store, for the one request that still has to be
/// told it is there: its kind, and nothing else.
fn make_vanished_entry(name: &str, kind: Kind) -> DirectoryEntry {
    let row = Row { id: String::new(), parent_id: None, name: name.to_owned(), kind, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed };
    let state = match kind {
        Kind::Folder => EntryState::Directory(DirectorySyncState::Synced(row)),
        Kind::File => EntryState::File(FileSyncState::RemoteOnly(row)),
    };
    DirectoryEntry { name: name.to_owned(), state }
}

impl KernelRequests {
    /// The attributes of `entry`, which the node `node_id` stands for, in the folder's access
    /// mode.
    pub(super) fn attributes_of(&self, entry: &DirectoryEntry, node_id: u64) -> FileAttr {
        attributes(node_id, &self.access_mode, entry)
    }

    /// Answers with the entry `name` of `parent`, as the folder has it now.
    pub(super) fn answer_with_entry(&self, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let found = self.node_resolver.find_directory(parent.0).zip(name.to_str()).and_then(|(dir, name)| self.directory_contents.find_entry(&dir, name));
        match found {
            Some(entry) => {
                let node_id = self.node_resolver.assign_node_id(&entry, parent.0);
                reply.entry(&TTL, &self.attributes_of(&entry, node_id), Generation(0))
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    /// The inotify bridge's thread is answered what the name it announces was before the
    /// change; everyone else, and the bridge for every other name, what the folder has now.
    pub(super) fn answer_lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if self.is_from_inotify_bridge(req) {
            match self.inotify_bridge.find_name_before_change(parent.0, name) {
                Some(NameBeforeChange::Absent) => return reply.error(Errno::ENOENT),
                Some(NameBeforeChange::Present { node_id, kind }) => {
                    return reply.entry(&Duration::ZERO, &self.attributes_of(&make_vanished_entry(&name.to_string_lossy(), kind), node_id), Generation(0));
                }
                None => {}
            }
        }
        self.answer_with_entry(parent, name, reply)
    }

    pub(super) fn answer_getattr(&self, ino: INodeNo, reply: ReplyAttr) {
        match self.node_resolver.find_entry(ino.0) {
            Some((entry, _)) => reply.attr(&TTL, &self.attributes_of(&entry, ino.0)),
            None => reply.error(Errno::ENOENT),
        }
    }
}
