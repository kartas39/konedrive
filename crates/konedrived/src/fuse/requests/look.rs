//! The requests that look: a name, a node, a directory's content, attributes.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fuser::{Errno, FileAttr, FileHandle, FileType, Generation, INodeNo, ReplyAttr, ReplyOpen, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyStatfs, ReplyXattr, Request};
use konedrive_fs::placeholder::{XATTR_DRIVE, XATTR_ITEM_ID, XATTR_PIN, XATTR_ROOT, XATTR_STATE, XATTR_SYNC};
use konedrive_tree::{Kind, Placement, Row};

use super::Requests;
use crate::fuse::announce::Told;
use crate::fuse::attributes::{attributes, TTL};
use crate::fuse::directory::Directory;
use crate::fuse::directory_contents::{DirectoryEntry, DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::download_states::to_hydration_state;
use crate::fuse::shared::errno;

/// The entry of a name that is gone from the store, for the one request that still has to be
/// told it is there: its kind, and nothing else.
fn vanished_entry(name: &str, kind: Kind) -> DirectoryEntry {
    let row = Row { id: String::new(), parent_id: None, name: name.to_owned(), kind, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed };
    let state = match kind {
        Kind::Folder => EntryState::Directory(DirectorySyncState::Synced(row)),
        Kind::File => EntryState::File(FileSyncState::RemoteOnly(row)),
    };
    DirectoryEntry { name: name.to_owned(), state }
}

impl Requests {
    /// The attributes of `entry`, which the node `ino` stands for, in the folder's access
    /// mode.
    fn attributes_of(&self, entry: &DirectoryEntry, ino: u64) -> FileAttr {
        attributes(ino, self.0.access_mode(), entry)
    }

    /// The entry `name` of `parent`, as the folder has it now.
    pub(super) fn entry(&self, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let s = &self.0;
        let found = s.node_lookup.find_directory(parent.0).zip(name.to_str()).and_then(|(dir, name)| s.directory_contents.find_entry(&dir, name));
        match found {
            Some(entry) => {
                let ino = s.node_lookup.assign_node_id(&entry, parent.0);
                reply.entry(&TTL, &self.attributes_of(&entry, ino), Generation(0))
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    pub(super) fn looked_up(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if self.announcing(req) {
            match &*crate::panic::lock(&self.0.told) {
                Some(Told::Came { parent: in_, name: came }) if *in_ == parent.0 && name == came.as_str() => return reply.error(Errno::ENOENT),
                Some(Told::Gone { parent: in_, name: gone, ino, kind }) if *in_ == parent.0 && name == gone.as_str() => {
                    return reply.entry(&Duration::ZERO, &self.attributes_of(&vanished_entry(gone, *kind), *ino), Generation(0));
                }
                _ => {}
            }
        }
        self.entry(parent, name, reply)
    }

    pub(super) fn attributes(&self, ino: INodeNo, reply: ReplyAttr) {
        match self.0.node_lookup.find_entry(ino.0) {
            Some((entry, _)) => reply.attr(&TTL, &self.attributes_of(&entry, ino.0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    /// What the open directory `fh` holds: listed at its first read, and the same for every
    /// part of the listing after it.
    fn listing(&self, ino: INodeNo, fh: FileHandle, offset: u64) -> Option<(Directory, Arc<Vec<DirectoryEntry>>)> {
        let s = &self.0;
        let dir = s.node_lookup.find_directory(ino.0)?;
        let kept = crate::panic::lock(&s.listings).get(&fh.0).cloned().filter(|_| offset > 0);
        let entries = kept.unwrap_or_else(|| {
            let entries = Arc::new(s.directory_contents.list_entries(&dir));
            crate::panic::lock(&s.listings).insert(fh.0, Arc::clone(&entries));
            entries
        });
        Some((dir, entries))
    }

    pub(super) fn listed(&self, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let Some((_, entries)) = self.listing(ino, fh, offset) else { return reply.ok() };
        for (i, entry) in entries.iter().enumerate().skip(offset as usize) {
            let child = self.0.node_lookup.assign_node_id(entry, ino.0);
            let kind = match entry.state {
                EntryState::Directory(_) => FileType::Directory,
                EntryState::File(_) => FileType::RegularFile,
            };
            if reply.add(INodeNo(child), i as u64 + 1, kind, &entry.name) {
                break;
            }
        }
        reply.ok();
    }

    pub(super) fn listed_plus(&self, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectoryPlus) {
        let s = &self.0;
        let Some((_, entries)) = self.listing(ino, fh, offset) else { return reply.ok() };
        for (i, entry) in entries.iter().enumerate().skip(offset as usize) {
            let child = s.node_lookup.assign_node_id(entry, ino.0);
            let attr = self.attributes_of(entry, child);
            if reply.add(INodeNo(child), i as u64 + 1, &entry.name, &TTL, &attr, Generation(0)) {
                break;
            }
        }
        reply.ok();
    }

    pub(super) fn directory_opened(&self, reply: ReplyOpen) {
        let fh = self.0.next_handle.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        reply.opened(FileHandle(fh), fuser::FopenFlags::empty());
    }

    pub(super) fn directory_closed(&self, fh: FileHandle, reply: ReplyEmpty) {
        crate::panic::lock(&self.0.listings).remove(&fh.0);
        reply.ok();
    }

    pub(super) fn space(&self, reply: ReplyStatfs) {
        let v = match self.0.backing_directory.space() {
            Ok(v) => v,
            Err(e) => return reply.error(errno(&e)),
        };
        reply.statfs(v.f_blocks, v.f_bfree, v.f_bavail, v.f_files, v.f_ffree, v.f_bsize as u32, 255, v.f_frsize as u32);
    }

    /// Whether the caller may do what it asks: anything, but write to a read-only folder.
    pub(super) fn allowed(&self, mask: fuser::AccessFlags, reply: ReplyEmpty) {
        if mask.contains(fuser::AccessFlags::W_OK) && !self.0.writable() {
            return reply.error(Errno::EROFS);
        }
        reply.ok();
    }

    /// The attributes the Dolphin plugins, `konedrivectl` and the daemon read: an item's from
    /// the store, and what the outbox's worker marked on a file it sends.
    fn xattr(&self, ino: INodeNo, name: &str) -> Option<String> {
        let s = &self.0;
        if ino.0 == 1 && (name == XATTR_ROOT || name == XATTR_DRIVE) {
            // The folder's own: what its registration and its first cycle wrote on it.
            return s.backing_directory.attribute(Path::new(""), name);
        }
        let (entry, rel) = s.node_lookup.find_entry(ino.0)?;
        if name == XATTR_SYNC {
            return s.backing_directory.attribute(&rel, name);
        }
        let row = entry.row()?;
        match (name, &entry.state) {
            (XATTR_ITEM_ID, _) => Some(row.id.clone()),
            (XATTR_STATE, EntryState::File(file)) => Some(to_hydration_state(file, s.download_states.is_downloading(&row.id))?.as_str().to_owned()),
            (XATTR_PIN, _) if crate::panic::lock(&s.pins).contains(&row.id) => Some("1".to_owned()),
            _ => None,
        }
    }

    pub(super) fn attribute(&self, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let Some(value) = name.to_str().and_then(|name| self.xattr(ino, name)) else { return reply.error(Errno::ENODATA) };
        match size {
            0 => reply.size(value.len() as u32),
            n if (n as usize) < value.len() => reply.error(Errno::ERANGE),
            _ => reply.data(value.as_bytes()),
        }
    }

    pub(super) fn attribute_names(&self, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let names = [XATTR_ROOT, XATTR_DRIVE, XATTR_ITEM_ID, XATTR_STATE, XATTR_PIN, XATTR_SYNC];
        let held: Vec<u8> = names.iter().filter(|n| self.xattr(ino, n).is_some()).flat_map(|n| n.bytes().chain(std::iter::once(0))).collect();
        match size {
            0 => reply.size(held.len() as u32),
            n if (n as usize) < held.len() => reply.error(Errno::ERANGE),
            _ => reply.data(&held),
        }
    }
}
