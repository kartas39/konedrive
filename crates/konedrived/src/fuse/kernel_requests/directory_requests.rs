//! A directory opened, read and closed (`opendir`, `readdir`, `readdirplus`, `releasedir`).
//!
//! What an open directory holds is listed when it is read from its start, as what its
//! entries are made from ([`ListedName`]), and kept with its `fh`: every part of the
//! reading after that hands out the same names. `readdir` answers from the listed names alone;
//! `readdirplus` makes the entry of each name it answers with, for its attributes.

use std::sync::Arc;

use fuser::{FileHandle, FileType, FopenFlags, Generation, INodeNo, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyOpen};

use crate::fuse::open_files::Fh;
use crate::fuse::node_table::NodeId;
use super::KernelRequests;
use crate::fuse::attributes::TTL;
use crate::fuse::backing_directory::EntryKind;
use crate::fuse::directory_reference::DirectoryReference;
use crate::fuse::directory_contents::ListedName;

impl KernelRequests {
    /// The directory of the node `ino`, and what the entries of the open directory `fh` are
    /// made from: listed when it is read from its start, and the same for every part of the
    /// reading after it.
    fn find_listed_names(&self, ino: INodeNo, fh: FileHandle, offset: u64) -> Option<(DirectoryReference, Arc<Vec<ListedName>>)> {
        let directory = self.node_resolver.find_directory(NodeId::from(ino))?;
        let kept = self.open_files.find_listed_names(Fh::from(fh)).filter(|_| offset > 0);
        let listed_names = kept.unwrap_or_else(|| {
            let listed_names = Arc::new(self.directory_contents.list_names(&directory));
            self.open_files.keep_listed_names(Fh::from(fh), Arc::clone(&listed_names));
            listed_names
        });
        Some((directory, listed_names))
    }

    pub(super) fn answer_opendir(&self, reply: ReplyOpen) {
        reply.opened(FileHandle::from(self.open_files.add_directory()), FopenFlags::empty());
    }

    /// The names and the kinds: from the store alone for what is in OneDrive, with no sync
    /// state worked out.
    pub(super) fn answer_readdir(&self, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let Some((_, listed_names)) = self.find_listed_names(ino, fh, offset) else { return reply.ok() };
        for (i, listed_name) in listed_names.iter().enumerate().skip(offset as usize) {
            let child = self.node_resolver.assign_node_id_of_listed_name(listed_name, NodeId::from(ino));
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
    pub(super) fn answer_readdirplus(&self, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectoryPlus) {
        let Some((directory, listed_names)) = self.find_listed_names(ino, fh, offset) else { return reply.ok() };
        for (i, listed_name) in listed_names.iter().enumerate().skip(offset as usize) {
            let entry = self.directory_contents.describe_listed_name(listed_name, &directory);
            let child = self.node_resolver.assign_node_id(&entry, NodeId::from(ino));
            let attr = self.attributes_of(&entry, child);
            if reply.add(INodeNo::from(child), i as u64 + 1, &entry.name, &TTL, &attr, Generation(0)) {
                break;
            }
        }
        reply.ok();
    }

    pub(super) fn answer_releasedir(&self, fh: FileHandle, reply: ReplyEmpty) {
        self.open_files.remove_directory(Fh::from(fh));
        reply.ok();
    }
}
