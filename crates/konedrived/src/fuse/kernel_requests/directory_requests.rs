//! A directory opened, read and closed (`opendir`, `readdir`, `readdirplus`, `releasedir`).
//!
//! What an open directory holds is listed when it is read from its start, as what its
//! entries are made from ([`EntrySource`]), and kept with its file handle: every part of the
//! reading after that hands out the same names. `readdir` answers from the sources alone;
//! `readdirplus` makes the entry of each name it answers with, for its attributes.

use std::sync::Arc;

use fuser::{FileHandle, FileType, FopenFlags, Generation, INodeNo, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyOpen};

use super::KernelRequests;
use crate::fuse::attributes::TTL;
use crate::fuse::backing_directory::EntryKind;
use crate::fuse::directory::Directory;
use crate::fuse::directory_contents::EntrySource;

impl KernelRequests {
    /// The directory of the node `ino`, and what the entries of the open directory `fh` are
    /// made from: listed when it is read from its start, and the same for every part of the
    /// reading after it.
    fn find_entry_sources(&self, ino: INodeNo, fh: FileHandle, offset: u64) -> Option<(Directory, Arc<Vec<EntrySource>>)> {
        let directory = self.node_lookup.find_directory(ino.0)?;
        let kept = self.open_files.find_entry_sources(fh.0).filter(|_| offset > 0);
        let entry_sources = kept.unwrap_or_else(|| {
            let entry_sources = Arc::new(self.directory_contents.list_entry_sources(&directory));
            self.open_files.keep_entry_sources(fh.0, Arc::clone(&entry_sources));
            entry_sources
        });
        Some((directory, entry_sources))
    }

    pub(super) fn answer_opendir(&self, reply: ReplyOpen) {
        reply.opened(FileHandle(self.open_files.add_directory()), FopenFlags::empty());
    }

    /// The names and the kinds: from the store alone for what is in OneDrive, with no sync
    /// state worked out.
    pub(super) fn answer_readdir(&self, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let Some((_, entry_sources)) = self.find_entry_sources(ino, fh, offset) else { return reply.ok() };
        for (i, source) in entry_sources.iter().enumerate().skip(offset as usize) {
            let child = self.node_lookup.assign_node_id_of_source(source, ino.0);
            let kind = match source.kind() {
                EntryKind::Directory => FileType::Directory,
                EntryKind::File => FileType::RegularFile,
            };
            if reply.add(INodeNo(child), i as u64 + 1, kind, source.name()) {
                break;
            }
        }
        reply.ok();
    }

    /// The names with their attributes: the entry of each name that goes into this reply is
    /// made from its source, and of no other.
    pub(super) fn answer_readdirplus(&self, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectoryPlus) {
        let Some((directory, entry_sources)) = self.find_entry_sources(ino, fh, offset) else { return reply.ok() };
        for (i, source) in entry_sources.iter().enumerate().skip(offset as usize) {
            let entry = self.directory_contents.describe_source(source, &directory);
            let child = self.node_lookup.assign_node_id(&entry, ino.0);
            let attr = self.attributes_of(&entry, child);
            if reply.add(INodeNo(child), i as u64 + 1, &entry.name, &TTL, &attr, Generation(0)) {
                break;
            }
        }
        reply.ok();
    }

    pub(super) fn answer_releasedir(&self, fh: FileHandle, reply: ReplyEmpty) {
        self.open_files.remove_directory(fh.0);
        reply.ok();
    }
}
