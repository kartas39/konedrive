//! A directory opened, read and closed (`opendir`, `readdir`, `readdirplus`, `releasedir`).
//!
//! What an open directory holds is listed when it is read from its start, as what its
//! entries are made from ([`ListedName`]), and kept with its `fh`: every part of the
//! reading after that hands out the same names. `readdir` answers from the listed names alone;
//! `readdirplus` makes the entry of each name it answers with, for its attributes. Either
//! gives a name the node id its entry has, and neither changes the place of a node: a name
//! kept from the start of a reading may be of a file that was renamed since.
//!
//! [`DirectoryRequests`] is given the [`NodeResolver`], the [`DirectoryContents`], the
//! [`OpenHandles`], which keep the listed names, and the [`AccessModeSwitch`], for the modes
//! `readdirplus` shows.

use std::sync::Arc;

use fuser::{FileHandle, FileType, FopenFlags, Generation, INodeNo, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyOpen};

use crate::fuse::access_mode_switch::AccessModeSwitch;
use crate::fuse::attributes::{to_attributes, TTL};
use crate::fuse::backing_directory::EntryKind;
use crate::fuse::directory_contents::{DirectoryContents, ListedName};
use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::node_resolver::NodeResolver;
use crate::fuse::node_table::NodeId;
use crate::fuse::open_handles::{Fh, OpenHandles};

#[derive(Clone)]
pub(in crate::fuse) struct DirectoryRequests {
    node_resolver: Arc<NodeResolver>,
    directory_contents: Arc<DirectoryContents>,
    open_handles: Arc<OpenHandles>,
    access_mode_switch: Arc<AccessModeSwitch>,
}

impl DirectoryRequests {
    pub(in crate::fuse) fn new(node_resolver: Arc<NodeResolver>, directory_contents: Arc<DirectoryContents>, open_handles: Arc<OpenHandles>, access_mode_switch: Arc<AccessModeSwitch>) -> Self {
        DirectoryRequests { node_resolver, directory_contents, open_handles, access_mode_switch }
    }

    /// The directory of the node `node_id`, and what the entries of the open directory `fh`
    /// are made from: listed when it is read from its start, and the same for every part of
    /// the reading after it.
    fn find_listed_names(&self, node_id: NodeId, fh: Fh, offset: u64) -> Option<(DirectoryReference, Arc<Vec<ListedName>>)> {
        let directory = self.node_resolver.find_directory(node_id)?;
        let kept = self.open_handles.find_listed_names(fh).filter(|_| offset > 0);
        let listed_names = kept.unwrap_or_else(|| {
            let listed_names = Arc::new(self.directory_contents.list_names(&directory));
            self.open_handles.keep_listed_names(fh, Arc::clone(&listed_names));
            listed_names
        });
        Some((directory, listed_names))
    }

    pub(super) fn answer_opendir(&self, reply: ReplyOpen) {
        reply.opened(FileHandle::from(self.open_handles.add_directory()), FopenFlags::empty());
    }

    /// The names and the kinds: from the store alone for what is in OneDrive, with no sync
    /// state worked out.
    pub(super) fn answer_readdir(&self, node_id: NodeId, fh: Fh, offset: u64, mut reply: ReplyDirectory) {
        let Some((_, listed_names)) = self.find_listed_names(node_id, fh, offset) else { return reply.ok() };
        for (i, listed_name) in listed_names.iter().enumerate().skip(offset as usize) {
            let child = self.node_resolver.assign_node_id_of_listed_name(listed_name, node_id);
            let kind = match listed_name.kind() {
                EntryKind::Directory => FileType::Directory,
                EntryKind::File => FileType::RegularFile,
            };
            if reply.add(INodeNo::from(child), i as u64 + 1, kind, listed_name.name()) {
                break;
            }
        }
        reply.ok();
    }

    /// The names with their attributes: the entry of each name that goes into this reply is
    /// made from its listed name, and of no other.
    pub(super) fn answer_readdirplus(&self, node_id: NodeId, fh: Fh, offset: u64, mut reply: ReplyDirectoryPlus) {
        let Some((directory, listed_names)) = self.find_listed_names(node_id, fh, offset) else { return reply.ok() };
        let access_mode = self.access_mode_switch.access_mode();
        for (i, listed_name) in listed_names.iter().enumerate().skip(offset as usize) {
            let entry = self.directory_contents.describe_listed_name(listed_name, &directory);
            let child = self.node_resolver.assign_node_id_of_listed_name(listed_name, node_id);
            let attr = to_attributes(child, access_mode, &entry);
            if reply.add(INodeNo::from(child), i as u64 + 1, &entry.name, &TTL, &attr, Generation(0)) {
                break;
            }
        }
        reply.ok();
    }

    pub(super) fn answer_releasedir(&self, fh: Fh, reply: ReplyEmpty) {
        self.open_handles.close_directory(fh);
        reply.ok();
    }
}

#[cfg(test)]
mod tests;
