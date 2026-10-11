//! The kernel's interface of a served mount: the one `fuser::Filesystem`, over
//! [`ItemKernelRequests`].
//!
//! **State owned:** the reads that wait for OneDrive on the runtime, each by the kernel's
//! number of its request, for the kernel's interrupt. Everything else is
//! `ItemKernelRequests`'.
//!
//! **Answers for:** one function of `ItemKernelRequests` per request, and its answer
//! turned into the kernel's reply; and the requests that file leaves here (init, statfs,
//! access, interrupt, symlink, setxattr, removexattr). The kernel's numbers come in
//! fuser's types and are the mount's own from the first line on: a node id is a local
//! item id, a file handle an [`Fh`], the caller's id the id of its thread (by which the
//! inotify bridge's own requests are told).
//!
//! A reply that goes to the write-operation thread or to the runtime is moved into a
//! closure ([`ReplyTo`]); dropped unanswered — the thread stopped, the task aborted — it
//! answers the kernel an error by itself.
//!
//! Made by `ItemMount::serve`, which gives it to the session.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use konedrive_tree::{Item, ItemKind};
use tokio::task::AbortHandle;

use super::access_mode_switch::AccessModeSwitch;
use super::backing_directory::BackingDirectory;
use super::errno::to_errno;
use super::item_attributes::{to_local_item_id, to_node_id};
use super::item_handles::Fh;
use super::item_kernel_requests::{ItemKernelRequests, ATTRIBUTE_LIFETIME, LookupAnswer, MadeEntry, OpenedFile, RangeToFetch, ReadAnswer, ReplyTo};
use super::item_write_operations::{FileToFill, MadeKind, RenameMode};

/// The longest name the folder takes, as statfs says it.
const LONGEST_NAME: u32 = 255;

/// Where a read is fetched on the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeFetch {
    /// The range itself, from OneDrive.
    Range,
    /// The range, once the whole download of its content was asked for and landed.
    AfterWholeDownload,
}

/// The value a getxattr or listxattr is answered with, by the size the caller's buffer
/// has: the length for a buffer of none, `ERANGE` for one too small. Pure.
fn decide_xattr_reply(value_length: usize, size: u32) -> Result<XattrReply, Errno> {
    match size {
        0 => Ok(XattrReply::Length(value_length as u32)),
        size if (size as usize) < value_length => Err(Errno::ERANGE),
        _ => Ok(XattrReply::Value),
    }
}

/// What a request for an extended attribute is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XattrReply {
    Length(u32),
    Value,
}

/// How a rename is to treat what is at the new name, from the kernel's flags. Pure.
fn to_rename_mode(flags: RenameFlags) -> RenameMode {
    if flags.contains(RenameFlags::RENAME_EXCHANGE) {
        RenameMode::Exchange
    } else if flags.contains(RenameFlags::RENAME_NOREPLACE) {
        RenameMode::NoReplace
    } else {
        RenameMode::Replace
    }
}

/// The time a setattr asks for, in whole Unix seconds and the rest in nanoseconds; "now"
/// read here. A time before 1970 is 0. Pure but for the clock.
fn to_unix_time(time: TimeOrNow) -> (i64, u32) {
    let time = match time {
        TimeOrNow::SpecificTime(time) => time,
        TimeOrNow::Now => SystemTime::now(),
    };
    time.duration_since(UNIX_EPOCH).map_or((0, 0), |since| (since.as_secs() as i64, since.subsec_nanos()))
}

/// The name a listing shows an item under; `None` for an item with no local version.
fn find_listed_name(item: &Item) -> Option<&OsStr> {
    item.local.as_ref()?.place.as_ref().map(|place| place.name.as_os_str())
}

fn to_file_handle(fh: Fh) -> FileHandle {
    FileHandle(fh.0)
}

fn to_fh(fh: FileHandle) -> Fh {
    Fh(fh.0)
}

fn to_fopen_flags(opened: &OpenedFile) -> FopenFlags {
    if opened.keeps_cache {
        FopenFlags::FOPEN_KEEP_CACHE
    } else {
        FopenFlags::empty()
    }
}

/// The reply of an open, as what an operation answers goes into.
fn reply_to_open(reply: ReplyOpen) -> ReplyTo<OpenedFile> {
    Box::new(move |opened| match opened {
        Ok(opened) => reply.opened(to_file_handle(opened.fh), to_fopen_flags(&opened)),
        Err(errno) => reply.error(errno),
    })
}

/// The reply of a setattr.
fn reply_to_attributes(reply: ReplyAttr) -> ReplyTo<FileAttr> {
    Box::new(move |attributes| match attributes {
        Ok(attributes) => reply.attr(&ATTRIBUTE_LIFETIME, &attributes),
        Err(errno) => reply.error(errno),
    })
}

/// The reply of a mkdir or a mknod.
fn reply_to_made_entry(reply: ReplyEntry) -> ReplyTo<MadeEntry> {
    Box::new(move |made| match made {
        Ok(made) => reply.entry(&ATTRIBUTE_LIFETIME, &made.entry.attributes, Generation(0)),
        Err(errno) => reply.error(errno),
    })
}

/// The reply of a create.
fn reply_to_created(reply: ReplyCreate) -> ReplyTo<MadeEntry> {
    Box::new(move |made| match made {
        Ok(made) => reply.created(&ATTRIBUTE_LIFETIME, &made.entry.attributes, Generation(0), to_file_handle(made.fh), FopenFlags::empty()),
        Err(errno) => reply.error(errno),
    })
}

/// The reply of an unlink, an rmdir or a rename.
fn reply_to_done(reply: ReplyEmpty) -> ReplyTo<()> {
    Box::new(move |done| match done {
        Ok(()) => reply.ok(),
        Err(errno) => reply.error(errno),
    })
}

/// The reads that wait for OneDrive, each by the number of its request.
#[derive(Default)]
struct InterruptibleReads(Mutex<HashMap<u64, AbortHandle>>);

impl InterruptibleReads {
    fn add(&self, request: u64, read: AbortHandle) {
        crate::panic::lock(&self.0).insert(request, read);
    }

    fn remove(&self, request: u64) {
        crate::panic::lock(&self.0).remove(&request);
    }

    /// The kernel gave the request up: its read is aborted, and its reply, dropped with
    /// the task, answers the kernel.
    fn interrupt(&self, request: u64) {
        let read = crate::panic::lock(&self.0).remove(&request);
        if let Some(read) = read {
            read.abort();
        }
    }
}

/// What answers the kernel's requests of one served mount.
pub(super) struct KernelRequests {
    requests: Arc<ItemKernelRequests>,
    /// For statfs: the space of the filesystem the folder is on.
    backing_directory: Arc<BackingDirectory>,
    /// For access and for what is refused whatever the folder holds.
    access_mode_switch: Arc<AccessModeSwitch>,
    interruptible_reads: Arc<InterruptibleReads>,
    runtime: tokio::runtime::Handle,
}

impl KernelRequests {
    /// Made by `ItemMount::serve` over what the mount's build answered.
    pub(super) fn new(requests: ItemKernelRequests, backing_directory: Arc<BackingDirectory>, access_mode_switch: Arc<AccessModeSwitch>, runtime: tokio::runtime::Handle) -> Self {
        KernelRequests { requests: Arc::new(requests), backing_directory, access_mode_switch, interruptible_reads: Arc::default(), runtime }
    }

    /// The errno of a request the folder never takes: `refusal` in a folder that takes
    /// changes, `EROFS` in one that takes none.
    fn refuse_unsupported(&self, refusal: Errno) -> Errno {
        match self.access_mode_switch.check_read_write() {
            Ok(()) => refusal,
            Err(read_only) => read_only,
        }
    }

    /// Fetches `range` on the runtime and answers `reply` there; the task is kept by the
    /// request's number until it ends, for the kernel's interrupt.
    fn fetch_on_runtime(&self, req: &Request, range: RangeToFetch, fetch: RangeFetch, reply: ReplyData) {
        let request = req.unique().0;
        let (requests, reads) = (Arc::clone(&self.requests), Arc::clone(&self.interruptible_reads));
        let read = self.runtime.spawn(async move {
            let fetched = match fetch {
                RangeFetch::Range => requests.fetch_read_range(range).await,
                RangeFetch::AfterWholeDownload => requests.fetch_range_after_download(range).await,
            };
            reads.remove(request);
            match fetched {
                Ok(bytes) => reply.data(&bytes),
                Err(errno) => reply.error(errno),
            }
        });
        self.interruptible_reads.add(request, read.abort_handle());
        // It may have ended before it was kept: then nothing is left to interrupt.
        if read.is_finished() {
            self.interruptible_reads.remove(request);
        }
    }
}

impl Filesystem for KernelRequests {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let _ = config.set_max_readahead(1 << 20);
        // The truncate to nothing comes with the open: a file that is not downloaded and is
        // overwritten whole is never downloaded first.
        let _ = config.add_capabilities(InitFlags::FUSE_DO_READDIRPLUS | InitFlags::FUSE_READDIRPLUS_AUTO | InitFlags::FUSE_PARALLEL_DIROPS | InitFlags::FUSE_ATOMIC_O_TRUNC);
        Ok(())
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.requests.answer_lookup(to_local_item_id(parent), name, req.pid()) {
            Ok(answer) => reply.entry(&LookupAnswer::lifetime(&answer), answer.attributes(), Generation(0)),
            Err(errno) => reply.error(errno),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.requests.answer_getattr(to_local_item_id(ino), fh.map(to_fh)) {
            Ok(attributes) => reply.attr(&ATTRIBUTE_LIFETIME, &attributes),
            Err(errno) => reply.error(errno),
        }
    }

    fn interrupt(&self, _req: &Request, unique: fuser::RequestId) {
        self.interruptible_reads.interrupt(unique.0);
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let node = to_local_item_id(ino);
        // Opening to write is a change of the folder.
        if flags.0 & libc::O_TRUNC != 0 {
            return self.requests.queue_open_to_overwrite(node, reply_to_open(reply));
        }
        if flags.0 & libc::O_ACCMODE == libc::O_RDONLY {
            return match self.requests.answer_open_to_read(node) {
                Ok(opened) => reply.opened(to_file_handle(opened.fh), to_fopen_flags(&opened)),
                Err(errno) => reply.error(errno),
            };
        }
        match self.requests.decide_open_to_write(node) {
            Ok(FileToFill::Nothing) => self.requests.queue_open_to_write(node, reply_to_open(reply)),
            Ok(FileToFill::Download(file)) => {
                let (requests, reply) = (Arc::clone(&self.requests), reply_to_open(reply));
                self.runtime.spawn(async move { requests.fill_and_open_to_write(node, file, reply).await });
            }
            Err(errno) => reply.error(errno),
        }
    }

    fn read(&self, req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, size: u32, _flags: OpenFlags, _lock_owner: Option<LockOwner>, reply: ReplyData) {
        match self.requests.decide_read(to_fh(fh), offset, size) {
            Ok(ReadAnswer::Bytes(bytes)) => reply.data(&bytes),
            Ok(ReadAnswer::Range(range)) => self.fetch_on_runtime(req, range, RangeFetch::Range, reply),
            Ok(ReadAnswer::WholeDownload(range)) => self.fetch_on_runtime(req, range, RangeFetch::AfterWholeDownload, reply),
            Err(errno) => reply.error(errno),
        }
    }

    fn write(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, data: &[u8], _write_flags: WriteFlags, _flags: OpenFlags, _lock_owner: Option<LockOwner>, reply: ReplyWrite) {
        match self.requests.answer_write(to_fh(fh), offset, data) {
            Ok(written) => reply.written(written),
            Err(errno) => reply.error(errno),
        }
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        match self.requests.answer_fsync(to_fh(fh)) {
            Ok(()) => reply.ok(),
            Err(errno) => reply.error(errno),
        }
    }

    fn release(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, _lock_owner: Option<LockOwner>, _flush: bool, reply: ReplyEmpty) {
        self.requests.answer_release(to_fh(fh));
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.requests.answer_opendir(to_local_item_id(ino)) {
            Ok(fh) => reply.opened(to_file_handle(fh), FopenFlags::empty()),
            Err(errno) => reply.error(errno),
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let listed = match self.requests.answer_readdir(to_local_item_id(ino), to_fh(fh), offset as usize) {
            Ok(listed) => listed,
            Err(errno) => return reply.error(errno),
        };
        // An offset is a place in the listing, also for an item that is left out.
        for (place, item) in listed.iter().enumerate().skip(offset as usize) {
            let Some(name) = find_listed_name(item) else { continue };
            let kind = match item.kind {
                ItemKind::Directory => FileType::Directory,
                ItemKind::File => FileType::RegularFile,
            };
            if reply.add(to_node_id(item.local_item_id), place as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn readdirplus(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectoryPlus) {
        let listed = match self.requests.answer_readdir(to_local_item_id(ino), to_fh(fh), offset as usize) {
            Ok(listed) => listed,
            Err(errno) => return reply.error(errno),
        };
        for (place, item) in listed.iter().enumerate().skip(offset as usize) {
            let Some(name) = find_listed_name(item) else { continue };
            let Some(entry) = self.requests.describe_listed_item(item.clone()) else { continue };
            if reply.add(to_node_id(item.local_item_id), place as u64 + 1, name, &ATTRIBUTE_LIFETIME, &entry.attributes, Generation(0)) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        self.requests.answer_releasedir(to_fh(fh));
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match self.backing_directory.measure_space() {
            Ok(space) => reply.statfs(space.f_blocks, space.f_bfree, space.f_bavail, space.f_files, space.f_ffree, space.f_bsize as u32, LONGEST_NAME, space.f_frsize as u32),
            Err(error) => reply.error(to_errno(&error)),
        }
    }

    fn access(&self, _req: &Request, _ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        let refused = if mask.contains(AccessFlags::W_OK) { self.access_mode_switch.check_read_write() } else { Ok(()) };
        match refused {
            Ok(()) => reply.ok(),
            Err(read_only) => reply.error(read_only),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let value = match self.requests.answer_getxattr(to_local_item_id(ino), name) {
            Ok(value) => value,
            Err(errno) => return reply.error(errno),
        };
        match decide_xattr_reply(value.len(), size) {
            Ok(XattrReply::Length(length)) => reply.size(length),
            Ok(XattrReply::Value) => reply.data(&value),
            Err(errno) => reply.error(errno),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let names = match self.requests.answer_listxattr(to_local_item_id(ino)) {
            Ok(names) => names,
            Err(errno) => return reply.error(errno),
        };
        // Each name with the zero that ends it.
        let value: Vec<u8> = names.iter().flat_map(|name| name.bytes().chain(std::iter::once(0))).collect();
        match decide_xattr_reply(value.len(), size) {
            Ok(XattrReply::Length(length)) => reply.size(length),
            Ok(XattrReply::Value) => reply.data(&value),
            Err(errno) => reply.error(errno),
        }
    }

    fn setattr(&self, req: &Request, ino: INodeNo, _mode: Option<u32>, _uid: Option<u32>, _gid: Option<u32>, size: Option<u64>, _atime: Option<TimeOrNow>, mtime: Option<TimeOrNow>, _ctime: Option<SystemTime>, fh: Option<FileHandle>, _crtime: Option<SystemTime>, _chgtime: Option<SystemTime>, _bkuptime: Option<SystemTime>, _flags: Option<BsdFileFlags>, reply: ReplyAttr) {
        let (node, fh) = (to_local_item_id(ino), fh.map(to_fh));
        match (size, mtime) {
            (Some(size), _) => match self.requests.decide_truncate(node, size) {
                Ok(FileToFill::Nothing) => self.requests.queue_truncate(node, fh, size, reply_to_attributes(reply)),
                Ok(fill @ FileToFill::Download(_)) => {
                    let (requests, reply) = (Arc::clone(&self.requests), reply_to_attributes(reply));
                    self.runtime.spawn(async move { requests.fill_and_truncate(node, fh, size, fill, reply).await });
                }
                Err(errno) => reply.error(errno),
            },
            (None, Some(mtime)) => {
                let (mtime, mtime_nsec) = to_unix_time(mtime);
                self.requests.queue_set_modified_time(node, fh, mtime, mtime_nsec, req.pid(), reply_to_attributes(reply))
            }
            // Mode, owner and the other times are not kept: answered as getattr.
            (None, None) => match self.requests.answer_getattr(node, fh) {
                Ok(attributes) => reply.attr(&ATTRIBUTE_LIFETIME, &attributes),
                Err(errno) => reply.error(errno),
            },
        }
    }

    fn mkdir(&self, req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        self.requests.queue_make(to_local_item_id(parent), name, MadeKind::Directory, req.pid(), reply_to_made_entry(reply))
    }

    /// Only a regular file is made: nothing else has a place in OneDrive.
    fn mknod(&self, req: &Request, parent: INodeNo, name: &OsStr, mode: u32, _umask: u32, _rdev: u32, reply: ReplyEntry) {
        if mode & libc::S_IFMT != libc::S_IFREG {
            return reply.error(self.refuse_unsupported(Errno::EPERM));
        }
        self.requests.queue_make(to_local_item_id(parent), name, MadeKind::File, req.pid(), reply_to_made_entry(reply))
    }

    fn create(&self, req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        self.requests.queue_make(to_local_item_id(parent), name, MadeKind::FileOpenedForWriting, req.pid(), reply_to_created(reply))
    }

    /// A symbolic link has nothing OneDrive could hold: none is made.
    fn symlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, _target: &Path, reply: ReplyEntry) {
        reply.error(self.refuse_unsupported(Errno::EPERM));
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.requests.queue_remove(to_local_item_id(parent), name, ItemKind::File, req.pid(), reply_to_done(reply))
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.requests.queue_remove(to_local_item_id(parent), name, ItemKind::Directory, req.pid(), reply_to_done(reply))
    }

    fn rename(&self, _req: &Request, parent: INodeNo, name: &OsStr, newparent: INodeNo, newname: &OsStr, flags: RenameFlags, reply: ReplyEmpty) {
        self.requests.queue_rename(to_local_item_id(parent), name, to_local_item_id(newparent), newname, to_rename_mode(flags), reply_to_done(reply))
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
