//! The mount's requests, each answered from the store and the folder under the mount.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fuser::{
    BsdFileFlags, Errno, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags, KernelConfig, LockOwner,
    OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use konedrive_fs::placeholder::{XATTR_DRIVE, XATTR_ITEM_ID, XATTR_PIN, XATTR_ROOT, XATTR_STATE};
use konedrive_graph::pool::Class;
use konedrive_tree::{Kind, Placement, Row};

use super::announce::Told;
use super::attr::TTL;
use super::fill::{Running, WRITTEN_BEHIND};
use super::read::Handle;
use super::shared::{errno, Shared};

/// The program behind a request, for the log: its id and the name the kernel has for it.
pub(super) fn caller(req: &Request) -> String {
    let name = fs::read_to_string(format!("/proc/{}/comm", req.pid())).unwrap_or_default();
    format!("{} ({})", name.trim(), req.pid())
}

/// The mount's requests.
pub(super) struct Requests(pub(super) Arc<Shared>);

impl Requests {
    fn announcing(&self, req: &Request) -> bool {
        req.pid() as i32 == self.0.announcer.load(Ordering::SeqCst)
    }

    /// The entry `name` of `parent`, as the store has it now.
    fn entry(&self, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.0.child(parent, name) {
            Some(row) => {
                let ino = crate::panic::lock(&self.0.nodes).ino(&row, parent.0);
                reply.entry(&TTL, &self.0.attr(&row, ino), Generation(0))
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn xattr(&self, ino: INodeNo, name: &str) -> Option<String> {
        let s = &self.0;
        if ino.0 == 1 && (name == XATTR_ROOT || name == XATTR_DRIVE) {
            // The folder's own: what its registration and its first cycle wrote on it.
            return xattr::FileExt::get_xattr(&s.folder, name).ok().flatten().map(|v| String::from_utf8_lossy(&v).into_owned());
        }
        let row = s.id(ino).and_then(|id| s.row(&id))?;
        match name {
            XATTR_ITEM_ID => Some(row.id),
            XATTR_STATE if row.kind == Kind::File => Some(s.state_of(&row, &s.rel_of_ino(ino.0)?).as_str().to_owned()),
            XATTR_PIN if crate::panic::lock(&s.pins).contains(&row.id) => Some("1".to_owned()),
            _ => None,
        }
    }
}

impl Filesystem for Requests {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let _ = config.set_max_readahead(1 << 20);
        let _ = config.add_capabilities(InitFlags::FUSE_DO_READDIRPLUS | InitFlags::FUSE_READDIRPLUS_AUTO | InitFlags::FUSE_PARALLEL_DIROPS);
        Ok(())
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if self.announcing(req) {
            match &*crate::panic::lock(&self.0.told) {
                Some(Told::Came { parent: in_, name: came }) if *in_ == parent.0 && name == came.as_str() => return reply.error(Errno::ENOENT),
                Some(Told::Gone { parent: in_, name: gone, ino, kind }) if *in_ == parent.0 && name == gone.as_str() => {
                    let row = Row { id: String::new(), parent_id: None, name: gone.clone(), kind: *kind, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed };
                    return reply.entry(&Duration::ZERO, &self.0.attr(&row, *ino), Generation(0));
                }
                _ => {}
            }
        }
        self.entry(parent, name, reply)
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let s = &self.0;
        if ino.0 == 1 {
            let root = Row { id: String::new(), parent_id: None, name: String::new(), kind: Kind::Folder, size: 0, mtime: 0, etag: None, ctag: None, quickxor: None, mime: None, placement: Placement::Placed };
            return reply.attr(&TTL, &s.attr(&root, 1));
        }
        match s.id(ino).and_then(|id| s.row(&id)) {
            Some(row) => reply.attr(&TTL, &s.attr(&row, ino.0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    /// The program behind a read that waits for OneDrive was interrupted or killed: the read is
    /// given up, and answered with an error as its reply is dropped.
    fn interrupt(&self, _req: &Request, unique: fuser::RequestId) {
        if let Some(read) = crate::panic::lock(&self.0.reads).remove(&unique.0) {
            read.abort();
        }
    }

    /// Opening downloads nothing: a program may open a file and never read it.
    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let s = &self.0;
        if flags.0 & libc::O_ACCMODE != libc::O_RDONLY || flags.0 & libc::O_TRUNC != 0 {
            return reply.error(Errno::EROFS);
        }
        let (Some(row), Some(rel)) = (s.id(ino).and_then(|id| s.row(&id)), s.rel_of_ino(ino.0)) else {
            return reply.error(Errno::ENOENT);
        };
        let copy = if s.has_copy(&rel, &row.id) {
            match File::open(s.under(&rel)) {
                Ok(file) => Some(Arc::new(file)),
                Err(e) => return reply.error(errno(&e)),
            }
        } else {
            None
        };
        tracing::info!("open of {} by {}: flags {:#x}, {}", rel.display(), caller(req), flags.0, if copy.is_some() { "downloaded" } else { "not downloaded" });
        let fh = s.next_handle.fetch_add(1, Ordering::SeqCst);
        crate::panic::lock(&s.handles).insert(fh, Handle { id: row.id, rel, copy });
        reply.opened(FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE);
    }

    /// The bytes asked for, from wherever they are: the downloaded copy; what a download under
    /// way has written; or OneDrive, as a range. Enough read as ranges starts the download of
    /// the whole file, which no read waits for.
    fn read(&self, req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, size: u32, _flags: OpenFlags, _lock_owner: Option<LockOwner>, reply: ReplyData) {
        let s = &self.0;
        let from_file = |file: &File, reply: ReplyData| {
            let mut buf = vec![0u8; size as usize];
            let mut got = 0;
            while got < buf.len() {
                match file.read_at(&mut buf[got..], offset + got as u64) {
                    Ok(0) => break,
                    Ok(n) => got += n,
                    Err(e) => return reply.error(errno(&e)),
                }
            }
            reply.data(&buf[..got])
        };
        let Some((id, rel, copy)) = crate::panic::lock(&s.handles).get(&fh.0).map(|h| (h.id.clone(), h.rel.clone(), h.copy.clone())) else {
            return reply.error(Errno::EBADF);
        };
        if let Some(copy) = copy {
            return from_file(&copy, reply);
        }
        // Downloaded since it was opened: the handle reads the copy from now on.
        if s.has_copy(&rel, &id) {
            return match File::open(s.under(&rel)) {
                Ok(file) => {
                    let file = Arc::new(file);
                    if let Some(handle) = crate::panic::lock(&s.handles).get_mut(&fh.0) {
                        handle.copy = Some(Arc::clone(&file));
                    }
                    from_file(&file, reply)
                }
                Err(e) => reply.error(errno(&e)),
            };
        }
        let Some(row) = s.row(&id) else { return reply.error(Errno::ENOENT) };
        let end = (offset + u64::from(size)).min(row.size);
        if offset >= end {
            return reply.data(&[]);
        }
        // What a download under way has written already.
        let running = crate::panic::lock(&s.running).get(&id).cloned();
        if let Some(Running { mark, staged }) = running {
            if end + WRITTEN_BEHIND <= mark.load(Ordering::SeqCst) {
                return from_file(&staged, reply);
            }
        }
        tracing::info!("read of {} by {}: {} bytes at {offset}, from OneDrive", rel.display(), caller(req), end - offset);
        let (shared, unique) = (Arc::clone(s), req.unique().0);
        let task = s.runtime.spawn(async move {
            let fetched = shared.range(&row, offset, end).await;
            crate::panic::lock(&shared.reads).remove(&unique);
            match fetched {
                Ok(bytes) => {
                    let asked = {
                        let mut asked = crate::panic::lock(&shared.asked);
                        let total = asked.entry(row.id.clone()).or_default();
                        *total += bytes.len() as u64;
                        *total
                    };
                    reply.data(&bytes);
                    // A file smaller than the threshold is downloaded once all of it was read:
                    // it would be fetched again at every read otherwise.
                    if asked >= shared.download_after.min(row.size) {
                        shared.ensure(row, rel, Class::Open, None);
                    }
                }
                Err(e) => reply.error(Errno::from_i32(e)),
            }
        });
        crate::panic::lock(&s.reads).insert(unique, task.abort_handle());
        if task.is_finished() {
            crate::panic::lock(&s.reads).remove(&unique);
        }
    }

    fn release(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, _lock_owner: Option<LockOwner>, _flush: bool, reply: ReplyEmpty) {
        crate::panic::lock(&self.0.handles).remove(&fh.0);
        reply.ok();
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        reply.ok();
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let s = &self.0;
        let Some(id) = s.id(ino) else { return reply.ok() };
        for (i, row) in s.children(&id).iter().enumerate().skip(offset as usize) {
            let child = crate::panic::lock(&s.nodes).ino(row, ino.0);
            let kind = if row.kind == Kind::Folder { FileType::Directory } else { FileType::RegularFile };
            if reply.add(INodeNo(child), i as u64 + 1, kind, &row.name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectoryPlus) {
        let s = &self.0;
        let Some(id) = s.id(ino) else { return reply.ok() };
        for (i, row) in s.children(&id).iter().enumerate().skip(offset as usize) {
            let child = crate::panic::lock(&s.nodes).ino(row, ino.0);
            if reply.add(INodeNo(child), i as u64 + 1, &row.name, &TTL, &s.attr(row, child), Generation(0)) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // SAFETY: a plain structure the call fills.
        let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: the folder's own descriptor.
        if unsafe { libc::fstatvfs(self.0.folder.as_raw_fd(), &mut v) } != 0 {
            return reply.error(errno(&io::Error::last_os_error()));
        }
        reply.statfs(v.f_blocks, v.f_bfree, v.f_bavail, v.f_files, v.f_ffree, v.f_bsize as u32, 255, v.f_frsize as u32);
    }

    /// Whether the caller may do what it asks: anything but write.
    fn access(&self, _req: &Request, _ino: INodeNo, mask: fuser::AccessFlags, reply: ReplyEmpty) {
        if mask.contains(fuser::AccessFlags::W_OK) {
            return reply.error(Errno::EROFS);
        }
        reply.ok();
    }

    /// The attributes the Dolphin plugins, `konedrivectl` and the daemon read, answered from
    /// the store.
    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let Some(value) = name.to_str().and_then(|name| self.xattr(ino, name)) else { return reply.error(Errno::ENODATA) };
        match size {
            0 => reply.size(value.len() as u32),
            n if (n as usize) < value.len() => reply.error(Errno::ERANGE),
            _ => reply.data(value.as_bytes()),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let names = [XATTR_ROOT, XATTR_DRIVE, XATTR_ITEM_ID, XATTR_STATE, XATTR_PIN];
        let held: Vec<u8> = names.iter().filter(|n| self.xattr(ino, n).is_some()).flat_map(|n| n.bytes().chain(std::iter::once(0))).collect();
        match size {
            0 => reply.size(held.len() as u32),
            n if (n as usize) < held.len() => reply.error(Errno::ERANGE),
            _ => reply.data(&held),
        }
    }

    // Nothing that changes the folder is accepted from a program. Refused here and not by a
    // read-only mount: on one the kernel would refuse the announcer too.

    fn setattr(&self, req: &Request, ino: INodeNo, _mode: Option<u32>, _uid: Option<u32>, _gid: Option<u32>, _size: Option<u64>, _atime: Option<TimeOrNow>, _mtime: Option<TimeOrNow>, _ctime: Option<SystemTime>, _fh: Option<FileHandle>, _crtime: Option<SystemTime>, _chgtime: Option<SystemTime>, _bkuptime: Option<SystemTime>, _flags: Option<BsdFileFlags>, reply: ReplyAttr) {
        if self.announcing(req) {
            return self.getattr(req, ino, None, reply);
        }
        reply.error(Errno::EROFS);
    }

    fn mkdir(&self, req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        if self.announcing(req) {
            return self.entry(parent, name, reply);
        }
        reply.error(Errno::EROFS);
    }

    fn mknod(&self, req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, _rdev: u32, reply: ReplyEntry) {
        if self.announcing(req) {
            return self.entry(parent, name, reply);
        }
        reply.error(Errno::EROFS);
    }

    fn symlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, _target: &Path, reply: ReplyEntry) {
        reply.error(Errno::EROFS);
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.announcing(req) && self.0.child(parent, name).is_none() {
            return reply.ok();
        }
        reply.error(Errno::EROFS);
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.announcing(req) && self.0.child(parent, name).is_none() {
            return reply.ok();
        }
        reply.error(Errno::EROFS);
    }

    fn rename(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, _newparent: INodeNo, _newname: &OsStr, _flags: RenameFlags, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn create(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        reply.error(Errno::EROFS);
    }

    fn write(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _offset: u64, _data: &[u8], _write_flags: WriteFlags, _flags: OpenFlags, _lock_owner: Option<LockOwner>, reply: ReplyWrite) {
        reply.error(Errno::EROFS);
    }

    fn setxattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, _value: &[u8], _flags: i32, _position: u32, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }
}
