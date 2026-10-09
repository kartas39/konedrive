//! The mount's requests, each answered from the store and the folder under the mount. The
//! one implementation of the kernel's interface is here and only hands each request on: to
//! what looks ([`look`]), what reads ([`read`]) and what changes the folder ([`change`]).

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::SystemTime;

use fuser::{
    BsdFileFlags, Errno, FileHandle, Filesystem, INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr,
    Request, TimeOrNow, WriteFlags,
};

use super::shared::Shared;

mod change;
mod look;
mod read;

/// The program behind a request, for the log: its id and the name the kernel has for it.
pub(super) fn caller(req: &Request) -> String {
    let name = fs::read_to_string(format!("/proc/{}/comm", req.pid())).unwrap_or_default();
    format!("{} ({})", name.trim(), req.pid())
}

/// The mount's requests.
pub(super) struct Requests(pub(super) Arc<Shared>);

impl Requests {
    /// Whether the request is the announcer's own: a change from OneDrive it tells the
    /// kernel of, which the mount answers from the store and never makes.
    fn announcing(&self, req: &Request) -> bool {
        req.pid() as i32 == self.0.announcer.load(Ordering::SeqCst)
    }

    /// Whether `name` of `parent` is a name the folder does not hold: what the announcer
    /// removes is gone from the store already.
    fn absent(&self, parent: INodeNo, name: &OsStr) -> bool {
        let s = &self.0;
        s.node_lookup.find_directory(parent.0).zip(name.to_str()).is_none_or(|(dir, name)| s.directory_contents.find_entry(&dir, name).is_none())
    }
}

impl Filesystem for Requests {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let _ = config.set_max_readahead(1 << 20);
        // The cut to nothing comes with the open: a file that is not downloaded and is
        // overwritten whole is never downloaded first.
        let _ = config.add_capabilities(InitFlags::FUSE_DO_READDIRPLUS | InitFlags::FUSE_READDIRPLUS_AUTO | InitFlags::FUSE_PARALLEL_DIROPS | InitFlags::FUSE_ATOMIC_O_TRUNC);
        Ok(())
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.looked_up(req, parent, name, reply)
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.attributes(ino, reply)
    }

    /// The program behind a read that waits for OneDrive was interrupted or killed: the read is
    /// given up, and answered with an error as its reply is dropped.
    fn interrupt(&self, _req: &Request, unique: fuser::RequestId) {
        if let Some(read) = crate::panic::lock(&self.0.reads).remove(&unique.0) {
            read.abort();
        }
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        self.opened(req, ino, flags, reply)
    }

    fn read(&self, req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, size: u32, _flags: OpenFlags, _lock_owner: Option<LockOwner>, reply: ReplyData) {
        Requests::read(self, req, fh, offset, size, reply)
    }

    fn release(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, _lock_owner: Option<LockOwner>, _flush: bool, reply: ReplyEmpty) {
        self.released(fh, reply)
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        self.synced(fh, reply)
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.directory_opened(reply)
    }

    fn releasedir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        self.directory_closed(fh, reply)
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, reply: ReplyDirectory) {
        self.listed(ino, fh, offset, reply)
    }

    fn readdirplus(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, reply: ReplyDirectoryPlus) {
        self.listed_plus(ino, fh, offset, reply)
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        self.space(reply)
    }

    fn access(&self, _req: &Request, _ino: INodeNo, mask: fuser::AccessFlags, reply: ReplyEmpty) {
        self.allowed(mask, reply)
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        self.attribute(ino, name, size, reply)
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        self.attribute_names(ino, size, reply)
    }

    // What the announcer asks is answered and not made: the store has the change already.
    // Refused by the handlers and not by a read-only mount: on one the kernel would refuse
    // the announcer too.

    fn setattr(&self, req: &Request, ino: INodeNo, _mode: Option<u32>, _uid: Option<u32>, _gid: Option<u32>, size: Option<u64>, _atime: Option<TimeOrNow>, mtime: Option<TimeOrNow>, _ctime: Option<SystemTime>, _fh: Option<FileHandle>, _crtime: Option<SystemTime>, _chgtime: Option<SystemTime>, _bkuptime: Option<SystemTime>, _flags: Option<BsdFileFlags>, reply: ReplyAttr) {
        if self.announcing(req) {
            return self.attributes(ino, reply);
        }
        self.set(ino, size, mtime, reply)
    }

    fn mkdir(&self, req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        if self.announcing(req) {
            return self.entry(parent, name, reply);
        }
        self.made(parent, name, true, reply)
    }

    fn mknod(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32, _umask: u32, _rdev: u32, reply: ReplyEntry) {
        if self.announcing(req) {
            return self.entry(parent, name, reply);
        }
        if mode & libc::S_IFMT != libc::S_IFREG {
            return reply.error(Errno::EPERM);
        }
        self.made(parent, name, false, reply)
    }

    /// A symbolic link has nothing OneDrive could hold: none is made.
    fn symlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, _target: &Path, reply: ReplyEntry) {
        reply.error(if self.0.writable() { Errno::EPERM } else { Errno::EROFS });
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.announcing(req) {
            return if self.absent(parent, name) { reply.ok() } else { reply.error(Errno::EROFS) };
        }
        self.removed(parent, name, false, reply)
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.announcing(req) {
            return if self.absent(parent, name) { reply.ok() } else { reply.error(Errno::EROFS) };
        }
        self.removed(parent, name, true, reply)
    }

    fn rename(&self, _req: &Request, parent: INodeNo, name: &OsStr, newparent: INodeNo, newname: &OsStr, flags: RenameFlags, reply: ReplyEmpty) {
        self.renamed(parent, name, newparent, newname, flags, reply)
    }

    fn create(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        self.created(parent, name, reply)
    }

    fn write(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, data: &[u8], _write_flags: WriteFlags, _flags: OpenFlags, _lock_owner: Option<LockOwner>, reply: ReplyWrite) {
        self.written(fh, offset, data, reply)
    }

    /// The attributes are konedrive's own to write; a program that copies a file's along is
    /// told the folder keeps none.
    fn setxattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, _value: &[u8], _flags: i32, _position: u32, reply: ReplyEmpty) {
        reply.error(if self.0.writable() { Errno::ENOTSUP } else { Errno::EROFS });
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(if self.0.writable() { Errno::ENOTSUP } else { Errno::EROFS });
    }
}
