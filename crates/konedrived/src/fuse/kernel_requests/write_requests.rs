//! Everything that changes the folder (`open` for writing, `create`, `mkdir`, `mknod`,
//! `unlink`, `rmdir`, `rename`, `setattr`, and the `release` of a file's last writer).
//! Each is made by a write operation ([`crate::fuse::write_operations`]) on the
//! write-operation thread, which answers it. A file an operation opens is in the open
//! handles when it answers.
//!
//! Two requests are answered here, on the mount's own thread, and are no write operation:
//! `write` and `fsync` go into the file a write operation opened, through the handle it put
//! into the open handles. Nothing is recorded for them — the outbox holds the file since it
//! was opened — and no name of the folder changes.
//!
//! The access mode is asked twice, by design. A read-only folder's request is refused at
//! once, before it is put on the thread ([`WriteRequests::run_named_operation`],
//! [`WriteRequests::run_with_content`], [`WriteRequests::run_without_content`], and the
//! time a `setattr` sets): the thread waits for
//! the folder's tree lock, which a cycle of the delta listing holds for as long as it
//! takes, and a program does not wait for a cycle to be told. The operation then asks the
//! same [`AccessModeSwitch`] again on the thread, and that answer is the one that counts:
//! the switch to read-only runs on that thread too, so a folder that was let through there
//! stays read-write until the operation has recorded.
//!
//! [`WriteRequests`] is given the [`WriteOperations`] and the [`WriteOperationThread`]; the
//! [`AccessModeSwitch`]; the [`NodeResolver`], for the node id and the attributes it answers
//! a make with; the [`OpenHandles`], which have the file a write goes into; the
//! [`FileDownloader`], for a file that has to be downloaded before it is written; and the
//! runtime that download is waited for on.

use std::ffi::OsStr;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use fuser::{Errno, FileHandle, FopenFlags, Generation, RenameFlags, ReplyAttr, ReplyCreate, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, TimeOrNow};
use konedrive_graph::pool::Class;
use tokio::sync::oneshot;

use super::lookup_requests::answer_with_attributes;
use crate::fuse::access_mode_switch::{AccessMode, AccessModeSwitch};
use crate::fuse::attributes::{attributes, TTL};
use crate::fuse::backing_directory::EntryKind;
use crate::fuse::errno::to_errno;
use crate::fuse::file_downloader::FileDownloader;
use crate::fuse::node_resolver::NodeResolver;
use crate::fuse::node_table::NodeId;
use crate::fuse::open_handles::{Fh, OpenHandles};
use crate::fuse::write_operation_thread::WriteOperationThread;
use crate::fuse::write_operations::{EntryToMake, RenameMode, WriteOperations};

/// A reply of the kernel's that can be answered with an errno.
trait ErrnoReply: Send + 'static {
    fn refuse(self, errno: Errno);
}

macro_rules! errno_reply {
    ($($reply:ty),*) => {$(
        impl ErrnoReply for $reply {
            fn refuse(self, errno: Errno) {
                self.error(errno)
            }
        }
    )*};
}

errno_reply!(ReplyAttr, ReplyCreate, ReplyEmpty, ReplyEntry, ReplyOpen);

#[derive(Clone)]
pub(in crate::fuse) struct WriteRequests {
    write_operations: Arc<WriteOperations>,
    write_operation_thread: Arc<WriteOperationThread>,
    access_mode_switch: Arc<AccessModeSwitch>,
    node_resolver: Arc<NodeResolver>,
    open_handles: Arc<OpenHandles>,
    file_downloader: Arc<FileDownloader>,
    /// Where an open waits for a download.
    runtime: tokio::runtime::Handle,
}

impl WriteRequests {
    pub(in crate::fuse) fn new(
        write_operations: Arc<WriteOperations>,
        write_operation_thread: Arc<WriteOperationThread>,
        access_mode_switch: Arc<AccessModeSwitch>,
        node_resolver: Arc<NodeResolver>,
        open_handles: Arc<OpenHandles>,
        file_downloader: Arc<FileDownloader>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        WriteRequests { write_operations, write_operation_thread, access_mode_switch, node_resolver, open_handles, file_downloader, runtime }
    }

    /// The errno of a change the mount never takes: `refusal` in a read-write folder, and a
    /// read-only filesystem's in a read-only one.
    pub(super) fn refuse_unsupported(&self, refusal: Errno) -> Errno {
        match self.access_mode_switch.access_mode() {
            AccessMode::ReadWrite => refusal,
            AccessMode::ReadOnly => Errno::EROFS,
        }
    }

    /// Has `job` run on the write-operation thread.
    fn run_on_thread(&self, job: impl FnOnce(&WriteRequests) + Send + 'static) {
        let requests = self.clone();
        self.write_operation_thread.run(move || job(&requests));
    }

    /// Has `operation` run on the write-operation thread with `name`, and answer `reply`.
    /// Refused before that, at once: with `not_a_name` for a name that is not text, and as a
    /// read-only filesystem does in a read-only folder — the operation asks the mode again
    /// on the thread, but a program does not wait for a cycle to be told.
    fn run_named_operation<R: ErrnoReply>(&self, name: &OsStr, not_a_name: Errno, reply: R, operation: impl FnOnce(&WriteRequests, &str, R) + Send + 'static) {
        let Some(name) = name.to_str().map(str::to_owned) else { return reply.refuse(not_a_name) };
        if let Err(read_only) = self.access_mode_switch.check_read_write() {
            return reply.refuse(read_only);
        }
        self.run_on_thread(move |requests| operation(requests, &name, reply));
    }

    /// Has `job` run on the write-operation thread with `reply` once the content of the node
    /// `node_id` is in the backing directory, when writing it needs that; `reply` is refused
    /// instead when the folder is read-only, or the download fails.
    fn run_with_content<R: ErrnoReply>(&self, node_id: NodeId, reply: R, job: impl FnOnce(&WriteRequests, R) + Send + 'static) {
        let needed = match self.write_operations.find_needed_download(node_id) {
            Ok(Some(needed)) => needed,
            Ok(None) => return self.run_on_thread(move |requests| job(requests, reply)),
            Err(e) => return reply.refuse(e),
        };
        let requests = self.clone();
        self.runtime.spawn(async move {
            let (waiter, downloaded) = oneshot::channel();
            requests.file_downloader.download(needed.row, needed.path, Class::Open, Some(waiter));
            match downloaded.await {
                Ok(Ok(())) => requests.run_on_thread(move |requests| job(requests, reply)),
                Ok(Err(e)) => reply.refuse(Errno::from_i32(e)),
                Err(_) => reply.refuse(Errno::EIO),
            }
        });
    }

    /// Has `job` run on the write-operation thread with `reply`, for a write that cuts its
    /// file to nothing and needs nothing of its content; `reply` is refused instead, at
    /// once, when the folder is read-only.
    fn run_without_content<R: ErrnoReply>(&self, reply: R, job: impl FnOnce(&WriteRequests, R) + Send + 'static) {
        if let Err(read_only) = self.access_mode_switch.check_read_write() {
            return reply.refuse(read_only);
        }
        self.run_on_thread(move |requests| job(requests, reply));
    }

    /// A file opened for writing, with the content it has.
    pub(super) fn answer_open_to_write(&self, node_id: NodeId, reply: ReplyOpen) {
        self.run_with_content(node_id, reply, move |requests, reply| match requests.write_operations.open_to_write(node_id) {
            Ok(fh) => reply.opened(FileHandle::from(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        });
    }

    /// A file opened for writing and cut to nothing (`O_TRUNC`): nothing of it is
    /// downloaded first.
    pub(super) fn answer_open_to_overwrite(&self, node_id: NodeId, reply: ReplyOpen) {
        self.run_without_content(reply, move |requests, reply| match requests.write_operations.open_to_overwrite(node_id) {
            Ok(fh) => reply.opened(FileHandle::from(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        });
    }

    /// A directory (`mkdir`) or a file that is not opened (`mknod`) made.
    pub(super) fn answer_make(&self, parent: NodeId, name: &OsStr, entry_to_make: EntryToMake, reply: ReplyEntry) {
        self.run_named_operation(name, Errno::EINVAL, reply, move |requests, name, reply| match requests.write_operations.make(parent, name, entry_to_make) {
            Ok((node_id, entry, _)) => reply.entry(&TTL, &attributes(node_id, requests.access_mode_switch.access_mode(), &entry), Generation(0)),
            Err(e) => reply.error(e),
        });
    }

    /// `mknod`: a regular file is made, and nothing else is.
    pub(super) fn answer_mknod(&self, parent: NodeId, name: &OsStr, mode: u32, reply: ReplyEntry) {
        if mode & libc::S_IFMT != libc::S_IFREG {
            return reply.error(Errno::EPERM);
        }
        self.answer_make(parent, name, EntryToMake::File, reply)
    }

    pub(super) fn answer_create(&self, parent: NodeId, name: &OsStr, reply: ReplyCreate) {
        self.run_named_operation(name, Errno::EINVAL, reply, move |requests, name, reply| match requests.write_operations.make(parent, name, EntryToMake::FileOpenedForWriting) {
            Ok((node_id, entry, opened)) => {
                let attr = attributes(node_id, requests.access_mode_switch.access_mode(), &entry);
                reply.created(&TTL, &attr, Generation(0), FileHandle::from(opened.unwrap_or(Fh::NOT_OPENED)), FopenFlags::empty())
            }
            Err(e) => reply.error(e),
        });
    }

    /// A file (`unlink`) or a directory (`rmdir`) removed.
    pub(super) fn answer_remove(&self, parent: NodeId, name: &OsStr, kind: EntryKind, reply: ReplyEmpty) {
        self.run_named_operation(name, Errno::ENOENT, reply, move |requests, name, reply| match requests.write_operations.remove(parent, name, kind) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        });
    }

    pub(super) fn answer_rename(&self, parent: NodeId, name: &OsStr, new_parent: NodeId, new_name: &OsStr, flags: RenameFlags, reply: ReplyEmpty) {
        let (Some(_), Some(new_name)) = (name.to_str(), new_name.to_str().map(str::to_owned)) else { return reply.error(Errno::EINVAL) };
        // An exchange of two names is a change the mount never takes.
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            return reply.error(self.refuse_unsupported(Errno::EINVAL));
        }
        let mode = if flags.contains(RenameFlags::RENAME_NOREPLACE) { RenameMode::NoReplace } else { RenameMode::Replace };
        self.run_named_operation(name, Errno::EINVAL, reply, move |requests, name, reply| match requests.write_operations.rename(parent, name, new_parent, &new_name, mode) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        });
    }

    /// Bytes written through a handle that may write: into the file in the backing
    /// directory. The outbox holds the file since it was opened.
    pub(super) fn answer_write(&self, fh: Fh, offset: u64, data: &[u8], reply: ReplyWrite) {
        let Some((node_id, file)) = self.open_handles.find_file_to_write(fh) else { return reply.error(Errno::EBADF) };
        self.open_handles.mark_written(node_id);
        match file.write_all_at(data, offset) {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(to_errno(&e)),
        }
    }

    /// A size set cuts or extends the file; a time set alone is the file's own from then
    /// on. Either is a write operation, made on the write-operation thread. Modes and owners
    /// are the mount's to say and are not kept.
    pub(super) fn answer_setattr(&self, node_id: NodeId, size: Option<u64>, mtime: Option<TimeOrNow>, reply: ReplyAttr) {
        let Some(size) = size else { return self.run_set_modified_time(node_id, mtime, reply, WriteRequests::answer_with_attributes) };
        let truncate = move |requests: &WriteRequests, reply: ReplyAttr| match requests.write_operations.truncate(node_id, size, mtime) {
            Ok(()) => requests.answer_with_attributes(node_id, reply),
            Err(e) => reply.error(e),
        };
        // Cut to nothing, its content is not needed.
        match size {
            0 => self.run_without_content(reply, truncate),
            _ => self.run_with_content(node_id, reply, truncate),
        }
    }

    /// Has the time `mtime` gives set on the node `node_id` by the write operation, on the
    /// write-operation thread, and `answer` called with `reply` once it is; `reply` is
    /// refused instead when the folder is read-only — at once, and again by the operation
    /// when the folder was switched meanwhile.
    fn run_set_modified_time<R: ErrnoReply>(&self, node_id: NodeId, mtime: Option<TimeOrNow>, reply: R, answer: impl FnOnce(&WriteRequests, NodeId, R) + Send + 'static) {
        if let Err(read_only) = self.access_mode_switch.check_read_write() {
            return reply.refuse(read_only);
        }
        self.run_on_thread(move |requests| match requests.write_operations.set_modified_time(node_id, mtime) {
            Ok(()) => answer(requests, node_id, reply),
            Err(e) => reply.refuse(e),
        });
    }

    /// The attributes of the node `node_id` as they are once a `setattr` was made.
    fn answer_with_attributes(&self, node_id: NodeId, reply: ReplyAttr) {
        answer_with_attributes(&self.node_resolver, self.access_mode_switch.access_mode(), node_id, reply)
    }

    pub(super) fn answer_fsync(&self, fh: Fh, reply: ReplyEmpty) {
        match self.open_handles.find_file_to_write(fh).map(|(_, file)| file.sync_all()) {
            Some(Err(e)) => reply.error(to_errno(&e)),
            _ => reply.ok(),
        }
    }

    /// A handle of a file is closed (`release`): the last one that wrote a file leaves its
    /// content ready to send, which a write operation records.
    pub(super) fn answer_release(&self, fh: Fh, reply: ReplyEmpty) {
        let closed = self.open_handles.close_file(fh);
        if let Some(node_id) = closed.find_last_writer_node_id() {
            self.run_on_thread(move |requests| requests.write_operations.close_last_writer(node_id, closed.outcome));
        }
        reply.ok();
    }
}

#[cfg(test)]
mod tests;
