//! The kernel's requests, each answered from the store and the backing directory.
//!
//! [`KernelRequests`] is the one implementation of the kernel's interface, and only hands
//! each request on, to the group that answers it. A group is a type of its own, given
//! exactly the parts it uses:
//!
//! - [`LookupRequests`]: a name looked up, the attributes of a node and of the filesystem;
//! - [`DirectoryRequests`]: a directory opened, read and closed;
//! - [`XattrRequests`]: the extended attributes the mount shows;
//! - [`ReadRequests`]: a file opened to be read, and read;
//! - [`WriteRequests`]: everything that changes the folder, and a file closed.
//!
//! Besides the five it holds the [`InotifyBridge`], for the one question it asks itself:
//! whether a request is the bridge's own. Such a request is answered and not made: the store
//! has the change already. It is refused by the groups and not by mounting read-only: on a
//! read-only mount the kernel would refuse the bridge too.
//!
//! The kernel's numbers come in fuser's types, and are the layer's own — a [`NodeId`], an
//! [`Fh`], a [`RequestId`] — from here on. What answers a request later, on the
//! write-operation thread or on the runtime, is a clone of its group.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use fuser::{
    BsdFileFlags, Errno, FileHandle, Filesystem, INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr,
    Request, TimeOrNow, WriteFlags,
};

use super::backing_directory::EntryKind;
use super::inotify_bridge::InotifyBridge;
use super::node_table::NodeId;
use super::open_handles::Fh;
use super::write_operations::EntryToMake;

mod directory_requests;
pub(super) mod interruptible_reads;
mod kernel_numbers;
mod lookup_requests;
mod read_origin;
mod read_requests;
mod write_requests;
mod xattr_requests;
mod xattr_value;

pub(super) use directory_requests::DirectoryRequests;
pub(super) use interruptible_reads::InterruptibleReads;
use interruptible_reads::RequestId;
pub(super) use lookup_requests::LookupRequests;
pub(super) use read_requests::ReadRequests;
pub(super) use write_requests::WriteRequests;
pub(super) use xattr_requests::XattrRequests;

/// The program behind a request, for the log: its id and the name the kernel has for it.
fn describe_caller(req: &Request) -> String {
    let name = fs::read_to_string(format!("/proc/{}/comm", req.pid())).unwrap_or_default();
    format!("{} ({})", name.trim(), req.pid())
}

/// What answers the kernel's requests.
pub(super) struct KernelRequests {
    lookup_requests: LookupRequests,
    directory_requests: DirectoryRequests,
    xattr_requests: XattrRequests,
    read_requests: ReadRequests,
    write_requests: WriteRequests,
    /// Says whether a request is the bridge's own.
    inotify_bridge: Arc<InotifyBridge>,
}

impl KernelRequests {
    pub(super) fn new(lookup_requests: LookupRequests, directory_requests: DirectoryRequests, xattr_requests: XattrRequests, read_requests: ReadRequests, write_requests: WriteRequests, inotify_bridge: Arc<InotifyBridge>) -> Self {
        KernelRequests { lookup_requests, directory_requests, xattr_requests, read_requests, write_requests, inotify_bridge }
    }

    /// Whether the request is the inotify bridge's own: a change from OneDrive it announces,
    /// which is answered from the store and never made.
    fn is_from_inotify_bridge(&self, req: &Request) -> bool {
        self.inotify_bridge.is_own_thread(req.pid())
    }
}

impl Filesystem for KernelRequests {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let _ = config.set_max_readahead(1 << 20);
        // The cut to nothing comes with the open: a file that is not downloaded and is
        // overwritten whole is never downloaded first.
        let _ = config.add_capabilities(InitFlags::FUSE_DO_READDIRPLUS | InitFlags::FUSE_READDIRPLUS_AUTO | InitFlags::FUSE_PARALLEL_DIROPS | InitFlags::FUSE_ATOMIC_O_TRUNC);
        Ok(())
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.lookup_requests.answer_lookup(self.is_from_inotify_bridge(req), NodeId::from(parent), name, reply)
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.lookup_requests.answer_getattr(NodeId::from(ino), reply)
    }

    fn interrupt(&self, _req: &Request, unique: fuser::RequestId) {
        self.read_requests.interrupt(RequestId::from(unique));
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        // Opening to write is a change of the folder.
        if flags.0 & libc::O_ACCMODE != libc::O_RDONLY || flags.0 & libc::O_TRUNC != 0 {
            return self.write_requests.answer_open_to_write(NodeId::from(ino), flags.0 & libc::O_TRUNC != 0, reply);
        }
        self.read_requests.answer_open_to_read(req, NodeId::from(ino), flags, reply)
    }

    fn read(&self, req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, size: u32, _flags: OpenFlags, _lock_owner: Option<LockOwner>, reply: ReplyData) {
        self.read_requests.answer_read(req, Fh::from(fh), offset, size, reply)
    }

    fn release(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, _lock_owner: Option<LockOwner>, _flush: bool, reply: ReplyEmpty) {
        self.write_requests.answer_release(Fh::from(fh), reply)
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        self.write_requests.answer_fsync(Fh::from(fh), reply)
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.directory_requests.answer_opendir(reply)
    }

    fn releasedir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        self.directory_requests.answer_releasedir(Fh::from(fh), reply)
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, reply: ReplyDirectory) {
        self.directory_requests.answer_readdir(NodeId::from(ino), Fh::from(fh), offset, reply)
    }

    fn readdirplus(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, reply: ReplyDirectoryPlus) {
        self.directory_requests.answer_readdirplus(NodeId::from(ino), Fh::from(fh), offset, reply)
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        self.lookup_requests.answer_statfs(reply)
    }

    fn access(&self, _req: &Request, _ino: INodeNo, mask: fuser::AccessFlags, reply: ReplyEmpty) {
        self.lookup_requests.answer_access(mask, reply)
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        self.xattr_requests.answer_getxattr(NodeId::from(ino), name, size, reply)
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        self.xattr_requests.answer_listxattr(NodeId::from(ino), size, reply)
    }

    fn setattr(&self, req: &Request, ino: INodeNo, _mode: Option<u32>, _uid: Option<u32>, _gid: Option<u32>, size: Option<u64>, _atime: Option<TimeOrNow>, mtime: Option<TimeOrNow>, _ctime: Option<SystemTime>, _fh: Option<FileHandle>, _crtime: Option<SystemTime>, _chgtime: Option<SystemTime>, _bkuptime: Option<SystemTime>, _flags: Option<BsdFileFlags>, reply: ReplyAttr) {
        if self.is_from_inotify_bridge(req) {
            return self.lookup_requests.answer_getattr(NodeId::from(ino), reply);
        }
        self.write_requests.answer_setattr(NodeId::from(ino), size, mtime, reply)
    }

    fn mkdir(&self, req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        if self.is_from_inotify_bridge(req) {
            return self.lookup_requests.answer_with_entry(NodeId::from(parent), name, reply);
        }
        self.write_requests.answer_make(NodeId::from(parent), name, EntryToMake::Directory, reply)
    }

    fn mknod(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32, _umask: u32, _rdev: u32, reply: ReplyEntry) {
        if self.is_from_inotify_bridge(req) {
            return self.lookup_requests.answer_with_entry(NodeId::from(parent), name, reply);
        }
        self.write_requests.answer_mknod(NodeId::from(parent), name, mode, reply)
    }

    /// A symbolic link has nothing OneDrive could hold: none is made.
    fn symlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, _target: &Path, reply: ReplyEntry) {
        reply.error(self.write_requests.refuse_unsupported(Errno::EPERM));
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.is_from_inotify_bridge(req) {
            return self.lookup_requests.answer_announced_remove(NodeId::from(parent), name, reply);
        }
        self.write_requests.answer_remove(NodeId::from(parent), name, EntryKind::File, reply)
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.is_from_inotify_bridge(req) {
            return self.lookup_requests.answer_announced_remove(NodeId::from(parent), name, reply);
        }
        self.write_requests.answer_remove(NodeId::from(parent), name, EntryKind::Directory, reply)
    }

    fn rename(&self, _req: &Request, parent: INodeNo, name: &OsStr, newparent: INodeNo, newname: &OsStr, flags: RenameFlags, reply: ReplyEmpty) {
        self.write_requests.answer_rename(NodeId::from(parent), name, NodeId::from(newparent), newname, flags, reply)
    }

    fn create(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        self.write_requests.answer_create(NodeId::from(parent), name, reply)
    }

    fn write(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, data: &[u8], _write_flags: WriteFlags, _flags: OpenFlags, _lock_owner: Option<LockOwner>, reply: ReplyWrite) {
        self.write_requests.answer_write(Fh::from(fh), offset, data, reply)
    }

    /// The attributes are konedrive's own to write; a program that copies a file's along is
    /// told the folder keeps none.
    fn setxattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, _value: &[u8], _flags: i32, _position: u32, reply: ReplyEmpty) {
        reply.error(self.write_requests.refuse_unsupported(Errno::ENOTSUP));
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(self.write_requests.refuse_unsupported(Errno::ENOTSUP));
    }
}
