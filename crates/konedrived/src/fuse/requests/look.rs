//! The requests that look: a name, a node, a directory's content, attributes.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fuser::{Errno, FileAttr, FileHandle, FileType, Generation, INodeNo, ReplyAttr, ReplyOpen, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyStatfs, ReplyXattr, Request};
use konedrive_fs::placeholder::{XATTR_DRIVE, XATTR_ITEM_ID, XATTR_PIN, XATTR_ROOT, XATTR_STATE, XATTR_SYNC};
use konedrive_tree::Kind;

use super::Requests;
use crate::fuse::announce::Told;
use crate::fuse::attributes::{self, Shown, TTL};
use crate::fuse::directory_contents::{Directory, Entry};
use crate::fuse::shared::errno;

impl Requests {
    /// What the mount shows of `entry`, which is node `ino` at `rel`.
    fn shown(&self, entry: &Entry, ino: u64, rel: &Path) -> FileAttr {
        let s = &self.0;
        attributes::of_entry(entry, ino, rel, s.writable(), &s.view, &s.writers)
    }

    /// The entry `name` of `parent`, as the folder has it now.
    pub(super) fn entry(&self, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let s = &self.0;
        let found = s.resolve.dir(parent.0).zip(name.to_str()).and_then(|(dir, name)| Some((s.view.entry(&dir, name)?, dir.rel.join(name))));
        match found {
            Some((entry, rel)) => {
                let ino = s.resolve.ino_of(&entry, parent.0);
                reply.entry(&TTL, &self.shown(&entry, ino, &rel), Generation(0))
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    pub(super) fn looked_up(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if self.announcing(req) {
            match &*crate::panic::lock(&self.0.told) {
                Some(Told::Came { parent: in_, name: came }) if *in_ == parent.0 && name == came.as_str() => return reply.error(Errno::ENOENT),
                Some(Told::Gone { parent: in_, name: gone, ino, kind }) if *in_ == parent.0 && name == gone.as_str() => {
                    return reply.entry(&Duration::ZERO, &attributes::attr(*ino, self.0.writable(), &Shown::gone(*kind)), Generation(0));
                }
                _ => {}
            }
        }
        self.entry(parent, name, reply)
    }

    pub(super) fn attributes(&self, ino: INodeNo, reply: ReplyAttr) {
        match self.0.resolve.node(ino.0) {
            Some((entry, rel)) => reply.attr(&TTL, &self.shown(&entry, ino.0, &rel)),
            None => reply.error(Errno::ENOENT),
        }
    }

    /// What the open directory `fh` holds: listed at its first read, and the same for every
    /// part of the listing after it.
    fn listing(&self, ino: INodeNo, fh: FileHandle, offset: u64) -> Option<(Directory, Arc<Vec<Entry>>)> {
        let s = &self.0;
        let dir = s.resolve.dir(ino.0)?;
        let kept = crate::panic::lock(&s.listings).get(&fh.0).cloned().filter(|_| offset > 0);
        let entries = kept.unwrap_or_else(|| {
            let entries = Arc::new(s.view.entries(&dir));
            crate::panic::lock(&s.listings).insert(fh.0, Arc::clone(&entries));
            entries
        });
        Some((dir, entries))
    }

    pub(super) fn listed(&self, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let Some((_, entries)) = self.listing(ino, fh, offset) else { return reply.ok() };
        for (i, entry) in entries.iter().enumerate().skip(offset as usize) {
            let child = self.0.resolve.ino_of(entry, ino.0);
            let kind = if entry.is_dir() { FileType::Directory } else { FileType::RegularFile };
            if reply.add(INodeNo(child), i as u64 + 1, kind, entry.name()) {
                break;
            }
        }
        reply.ok();
    }

    pub(super) fn listed_plus(&self, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectoryPlus) {
        let s = &self.0;
        let Some((dir, entries)) = self.listing(ino, fh, offset) else { return reply.ok() };
        for (i, entry) in entries.iter().enumerate().skip(offset as usize) {
            let child = s.resolve.ino_of(entry, ino.0);
            let attr = self.shown(entry, child, &dir.rel.join(entry.name()));
            if reply.add(INodeNo(child), i as u64 + 1, entry.name(), &TTL, &attr, Generation(0)) {
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
        let v = match self.0.under.space() {
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
            return s.under.attribute(Path::new(""), name);
        }
        let (entry, rel) = s.resolve.node(ino.0)?;
        if name == XATTR_SYNC {
            return s.under.attribute(&rel, name);
        }
        let Entry::Item(row) = entry else { return None };
        match name {
            XATTR_ITEM_ID => Some(row.id),
            XATTR_STATE if row.kind == Kind::File => Some(s.state_of(&row, &rel).as_str().to_owned()),
            XATTR_PIN if crate::panic::lock(&s.pins).contains(&row.id) => Some("1".to_owned()),
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
