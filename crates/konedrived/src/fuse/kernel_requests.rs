//! The kernel's requests, each answered from the store and the backing directory.
//!
//! [`KernelRequests`] is the one implementation of the kernel's interface, and only hands
//! each request on, to the sub-module that answers it:
//!
//! - [`lookup_requests`]: a name looked up, and the attributes of a node;
//! - [`directory_requests`]: a directory opened, read and closed;
//! - [`xattr_requests`]: the extended attributes the mount shows;
//! - [`read_requests`]: a file opened to be read, read and closed;
//! - [`write_requests`]: everything that changes the folder.
//!
//! It holds the parts it hands the requests to, each by `Arc`, and one piece of state of its
//! own, keyed by the kernel's request numbers: the reads that an `interrupt` gives up
//! ([`InterruptibleReads`]). It is cloned into the jobs that answer a request later: on the
//! recorder's thread, or on the runtime.
//!
//! The requests of the [`InotifyBridge`]'s thread are answered and not made: the store has
//! the change already. They are refused by the handlers and not by mounting read-only: on a
//! read-only mount the kernel would refuse the bridge too.

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

use super::access_mode_switch::{AccessMode, AccessModeSwitch};
use super::backing_directory::BackingDirectory;
use super::directory_contents::DirectoryContents;
use super::download_states::DownloadStates;
use super::errno::to_errno;
use super::file_downloader::FileDownloader;
use super::inotify_bridge::InotifyBridge;
use super::node_resolver::NodeResolver;
use super::node_table::NodeId;
use super::open_handles::{Fh, OpenHandles};
use super::outbox_recorder::OutboxRecorder;
use super::pins::Pins;
use super::range_readers::RangeReaders;
use super::write_operations::WriteOperations;

mod directory_requests;
pub(super) mod interruptible_reads;
mod lookup_requests;
mod read_requests;
mod write_requests;
mod xattr_requests;

pub(super) use interruptible_reads::InterruptibleReads;
use interruptible_reads::RequestId;

// The edge of the layer: the kernel's numbers come in fuser's types, and are the layer's own
// from here on.
impl From<INodeNo> for NodeId {
    fn from(ino: INodeNo) -> Self {
        NodeId(ino.0)
    }
}

impl From<NodeId> for INodeNo {
    fn from(node_id: NodeId) -> Self {
        INodeNo(node_id.0)
    }
}

impl From<FileHandle> for Fh {
    fn from(fh: FileHandle) -> Self {
        Fh(fh.0)
    }
}

impl From<Fh> for FileHandle {
    fn from(fh: Fh) -> Self {
        FileHandle(fh.0)
    }
}

impl From<fuser::RequestId> for RequestId {
    fn from(unique: fuser::RequestId) -> Self {
        RequestId(unique.0)
    }
}

/// The program behind a request, for the log: its id and the name the kernel has for it.
fn describe_caller(req: &Request) -> String {
    let name = fs::read_to_string(format!("/proc/{}/comm", req.pid())).unwrap_or_default();
    format!("{} ({})", name.trim(), req.pid())
}

/// What answers the kernel's requests.
#[derive(Clone)]
pub(super) struct KernelRequests {
    pub(super) access_mode_switch: Arc<AccessModeSwitch>,
    pub(super) backing_directory: Arc<BackingDirectory>,
    pub(super) directory_contents: Arc<DirectoryContents>,
    pub(super) node_resolver: Arc<NodeResolver>,
    pub(super) open_handles: Arc<OpenHandles>,
    pub(super) outbox_recorder: Arc<OutboxRecorder>,
    pub(super) write_operations: Arc<WriteOperations>,
    pub(super) download_states: Arc<DownloadStates>,
    pub(super) file_downloader: Arc<FileDownloader>,
    pub(super) range_readers: Arc<RangeReaders>,
    pub(super) pins: Arc<Pins>,
    pub(super) inotify_bridge: Arc<InotifyBridge>,
    /// The reads that wait for a range from OneDrive, by request: shared with the mount,
    /// which gives them all up when it is taken off.
    pub(super) interruptible_reads: Arc<InterruptibleReads>,
    /// Where a read waits for OneDrive, and an open for a download.
    pub(super) runtime: tokio::runtime::Handle,
}

impl KernelRequests {
    /// Whether the request is the inotify bridge's own: a change from OneDrive it announces,
    /// which is answered from the store and never made.
    fn is_from_inotify_bridge(&self, req: &Request) -> bool {
        self.inotify_bridge.is_own_thread(req.pid())
    }

    /// Whether the folder holds no `name` in the directory node `parent`: what the inotify
    /// bridge removes is gone from the store already.
    fn is_absent(&self, parent: INodeNo, name: &OsStr) -> bool {
        self.node_resolver.find_directory(NodeId::from(parent)).zip(name.to_str()).is_none_or(|(dir, name)| self.directory_contents.find_entry(&dir, name).is_none())
    }

    /// Answers a remove the inotify bridge announces: done, as the name is gone already.
    fn answer_announced_remove(&self, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.is_absent(parent, name) {
            reply.ok()
        } else {
            reply.error(Errno::EROFS)
        }
    }

    /// The errno of a change the mount never takes: `refusal` in a read-write folder, and a
    /// read-only filesystem's in a read-only one.
    fn refuse_unsupported(&self, refusal: Errno) -> Errno {
        match self.access_mode_switch.access_mode() {
            AccessMode::ReadWrite => refusal,
            AccessMode::ReadOnly => Errno::EROFS,
        }
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
        self.answer_lookup(req, parent, name, reply)
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.answer_getattr(ino, reply)
    }

    /// The program behind a read that waits for OneDrive was interrupted or killed: the read is
    /// given up, and answered with an error as its reply is dropped.
    fn interrupt(&self, _req: &Request, unique: fuser::RequestId) {
        self.interruptible_reads.interrupt(unique.into());
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        // Opening to write is a change of the folder.
        if flags.0 & libc::O_ACCMODE != libc::O_RDONLY || flags.0 & libc::O_TRUNC != 0 {
            return self.answer_open_to_write(ino, flags.0 & libc::O_TRUNC != 0, reply);
        }
        self.answer_open_to_read(req, ino, flags, reply)
    }

    fn read(&self, req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, size: u32, _flags: OpenFlags, _lock_owner: Option<LockOwner>, reply: ReplyData) {
        self.answer_read(req, fh, offset, size, reply)
    }

    fn release(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, _lock_owner: Option<LockOwner>, _flush: bool, reply: ReplyEmpty) {
        self.answer_release(fh, reply)
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        self.answer_fsync(fh, reply)
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.answer_opendir(reply)
    }

    fn releasedir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        self.answer_releasedir(fh, reply)
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, reply: ReplyDirectory) {
        self.answer_readdir(ino, fh, offset, reply)
    }

    fn readdirplus(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, reply: ReplyDirectoryPlus) {
        self.answer_readdirplus(ino, fh, offset, reply)
    }

    /// The space of the filesystem the backing directory is on.
    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match self.backing_directory.measure_space() {
            Ok(v) => reply.statfs(v.f_blocks, v.f_bfree, v.f_bavail, v.f_files, v.f_ffree, v.f_bsize as u32, 255, v.f_frsize as u32),
            Err(e) => reply.error(to_errno(&e)),
        }
    }

    /// Whether the caller may do what it asks: anything, but write to a read-only folder.
    fn access(&self, _req: &Request, _ino: INodeNo, mask: fuser::AccessFlags, reply: ReplyEmpty) {
        if mask.contains(fuser::AccessFlags::W_OK) && self.access_mode_switch.access_mode() == AccessMode::ReadOnly {
            return reply.error(Errno::EROFS);
        }
        reply.ok();
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        self.answer_getxattr(ino, name, size, reply)
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        self.answer_listxattr(ino, size, reply)
    }

    fn setattr(&self, req: &Request, ino: INodeNo, _mode: Option<u32>, _uid: Option<u32>, _gid: Option<u32>, size: Option<u64>, _atime: Option<TimeOrNow>, mtime: Option<TimeOrNow>, _ctime: Option<SystemTime>, _fh: Option<FileHandle>, _crtime: Option<SystemTime>, _chgtime: Option<SystemTime>, _bkuptime: Option<SystemTime>, _flags: Option<BsdFileFlags>, reply: ReplyAttr) {
        if self.is_from_inotify_bridge(req) {
            return self.answer_getattr(ino, reply);
        }
        self.answer_setattr(ino, size, mtime, reply)
    }

    fn mkdir(&self, req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        if self.is_from_inotify_bridge(req) {
            return self.answer_with_entry(parent, name, reply);
        }
        self.answer_make(parent, name, true, reply)
    }

    fn mknod(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32, _umask: u32, _rdev: u32, reply: ReplyEntry) {
        if self.is_from_inotify_bridge(req) {
            return self.answer_with_entry(parent, name, reply);
        }
        if mode & libc::S_IFMT != libc::S_IFREG {
            return reply.error(Errno::EPERM);
        }
        self.answer_make(parent, name, false, reply)
    }

    /// A symbolic link has nothing OneDrive could hold: none is made.
    fn symlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, _target: &Path, reply: ReplyEntry) {
        reply.error(self.refuse_unsupported(Errno::EPERM));
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.is_from_inotify_bridge(req) {
            return self.answer_announced_remove(parent, name, reply);
        }
        self.answer_remove(parent, name, false, reply)
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        if self.is_from_inotify_bridge(req) {
            return self.answer_announced_remove(parent, name, reply);
        }
        self.answer_remove(parent, name, true, reply)
    }

    fn rename(&self, _req: &Request, parent: INodeNo, name: &OsStr, newparent: INodeNo, newname: &OsStr, flags: RenameFlags, reply: ReplyEmpty) {
        self.answer_rename(parent, name, newparent, newname, flags, reply)
    }

    fn create(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        self.answer_create(parent, name, reply)
    }

    fn write(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, data: &[u8], _write_flags: WriteFlags, _flags: OpenFlags, _lock_owner: Option<LockOwner>, reply: ReplyWrite) {
        self.answer_write(fh, offset, data, reply)
    }

    /// The attributes are konedrive's own to write; a program that copies a file's along is
    /// told the folder keeps none.
    fn setxattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, _value: &[u8], _flags: i32, _position: u32, reply: ReplyEmpty) {
        reply.error(self.refuse_unsupported(Errno::ENOTSUP));
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(self.refuse_unsupported(Errno::ENOTSUP));
    }
}
