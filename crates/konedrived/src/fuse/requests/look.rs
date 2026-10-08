//! The requests that look: a name, a node, a directory's content, attributes.

use std::ffi::OsStr;
use std::io;
use std::os::fd::AsRawFd;
use std::time::Duration;

use fuser::{Errno, FileType, Generation, INodeNo, ReplyAttr, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyStatfs, ReplyXattr, Request};
use konedrive_fs::placeholder::{XATTR_DRIVE, XATTR_ITEM_ID, XATTR_PIN, XATTR_ROOT, XATTR_STATE, XATTR_SYNC};
use konedrive_tree::{Kind, Placement, Row};

use super::Requests;
use crate::fuse::announce::Told;
use crate::fuse::attr::TTL;
use crate::fuse::local::Entry;
use crate::fuse::shared::errno;

impl Requests {
    /// The entry `name` of `parent`, as the folder has it now.
    pub(super) fn entry(&self, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let s = &self.0;
        let found = s.dir(parent.0).zip(name.to_str()).and_then(|(dir, name)| Some((s.entry(&dir, name)?, dir.rel.join(name))));
        match found {
            Some((entry, rel)) => {
                let ino = s.ino_of(&entry, parent.0);
                reply.entry(&TTL, &s.attr_of(&entry, ino, &rel), Generation(0))
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    pub(super) fn looked_up(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if self.announcing(req) {
            match &*crate::panic::lock(&self.0.told) {
                Some(Told::Came { parent: in_, name: came }) if *in_ == parent.0 && name == came.as_str() => return reply.error(Errno::ENOENT),
                Some(Told::Gone { parent: in_, name: gone, ino, kind }) if *in_ == parent.0 && name == gone.as_str() => {
                    let row = Row { id: String::new(), parent_id: None, name: gone.clone(), kind: *kind, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed };
                    return reply.entry(&Duration::ZERO, &self.0.attr(&row, *ino, None), Generation(0));
                }
                _ => {}
            }
        }
        self.entry(parent, name, reply)
    }

    pub(super) fn attributes(&self, ino: INodeNo, reply: ReplyAttr) {
        match self.0.node(ino.0) {
            Some((entry, rel)) => reply.attr(&TTL, &self.0.attr_of(&entry, ino.0, &rel)),
            None => reply.error(Errno::ENOENT),
        }
    }

    pub(super) fn listed(&self, ino: INodeNo, offset: u64, mut reply: ReplyDirectory) {
        let s = &self.0;
        let Some(dir) = s.dir(ino.0) else { return reply.ok() };
        for (i, entry) in s.entries(&dir).iter().enumerate().skip(offset as usize) {
            let child = s.ino_of(entry, ino.0);
            let kind = if entry.is_dir() { FileType::Directory } else { FileType::RegularFile };
            if reply.add(INodeNo(child), i as u64 + 1, kind, entry.name()) {
                break;
            }
        }
        reply.ok();
    }

    pub(super) fn listed_plus(&self, ino: INodeNo, offset: u64, mut reply: ReplyDirectoryPlus) {
        let s = &self.0;
        let Some(dir) = s.dir(ino.0) else { return reply.ok() };
        for (i, entry) in s.entries(&dir).iter().enumerate().skip(offset as usize) {
            let child = s.ino_of(entry, ino.0);
            let attr = s.attr_of(entry, child, &dir.rel.join(entry.name()));
            if reply.add(INodeNo(child), i as u64 + 1, entry.name(), &TTL, &attr, Generation(0)) {
                break;
            }
        }
        reply.ok();
    }

    pub(super) fn space(&self, reply: ReplyStatfs) {
        // SAFETY: a plain structure the call fills.
        let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: the folder's own descriptor.
        if unsafe { libc::fstatvfs(self.0.folder.as_raw_fd(), &mut v) } != 0 {
            return reply.error(errno(&io::Error::last_os_error()));
        }
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
            return xattr::FileExt::get_xattr(&s.folder, name).ok().flatten().map(|v| String::from_utf8_lossy(&v).into_owned());
        }
        let (entry, rel) = s.node(ino.0)?;
        if name == XATTR_SYNC {
            return xattr::get(s.under(&rel), name).ok().flatten().map(|v| String::from_utf8_lossy(&v).into_owned());
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
