//! Everything that changes the folder (`open` for writing, `create`, `mkdir`, `mknod`,
//! `unlink`, `rmdir`, `rename`, `setattr`, and the `release` of a file opened for writing).
//! Each is made by a write operation ([`crate::fuse::write_operations`]) on the
//! write-operation thread, which answers it. A file an operation opens is in the open
//! handles when it answers.
//!
//! The `release` alone is answered before its operation ran: the kernel does not wait for
//! it, and neither does the program. The file it closes stays in the open handles, open for
//! writing, until the operation closes it there on the thread and records in the same step
//! what the close left ([`WriteRequests::close_file`](content_requests)).
//!
//! Two requests are answered here, on the mount's own thread, and are no write operation:
//! `write` and `fsync` go into the file a write operation opened, through the handle it put
//! into the open handles. Nothing is recorded for them — the outbox holds the file since it
//! was opened — and no name of the folder changes.
//!
//! The access mode is asked twice, by design. A read-only folder's request is refused at
//! once, before it is put on the thread ([`WriteRequests::run_in_read_write_folder`], and
//! [`WriteRequests::run_with_content`] before it downloads anything): the thread waits for
//! the folder's tree lock, which a cycle of the delta listing holds for as long as it
//! takes, and a program does not wait for a cycle to be told. The operation then asks the
//! same [`AccessModeSwitch`] again on the thread, and that answer is the one that counts:
//! the switch to read-only runs on that thread too, so a folder that was let through there
//! stays read-write until the operation has recorded.
//!
//! Here are the type, and how a request gets onto the thread. What is answered is in two
//! modules of functions, by what the request changes:
//!
//! - [`name_requests`]: a name of the folder — made, removed, renamed;
//! - [`content_requests`]: a file's content and its time — opened for writing, written,
//!   cut, synced, closed.
//!
//! [`WriteRequests`] is given the [`WriteOperations`] and the [`WriteOperationThread`]; the
//! [`AccessModeSwitch`]; the [`NodeResolver`], for the node id and the attributes it answers
//! a make with; the [`OpenHandles`], which have the file a write goes into; the
//! [`FileDownloader`], for a file that has to be downloaded before it is written; and the
//! runtime that download is waited for on.

use std::sync::Arc;

use fuser::{Errno, ReplyAttr, ReplyCreate, ReplyEmpty, ReplyEntry, ReplyOpen};
use konedrive_graph::pool::Class;
use tokio::sync::oneshot;

use crate::fuse::access_mode_switch::{AccessMode, AccessModeSwitch};
use crate::fuse::file_downloader::FileDownloader;
use crate::fuse::node_resolver::NodeResolver;
use crate::fuse::node_table::NodeId;
use crate::fuse::open_handles::OpenHandles;
use crate::fuse::write_operation_thread::WriteOperationThread;
use crate::fuse::write_operations::WriteOperations;

mod content_requests;
mod name_requests;

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

    #[cfg(test)]
    pub(super) fn write_operations(&self) -> &Arc<WriteOperations> {
        &self.write_operations
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

    /// Has `job` run on the write-operation thread with `reply`, in a folder that takes
    /// changes. A read-only folder's `reply` is refused instead, at once, as a read-only
    /// filesystem does: the operation asks the mode again on the thread, but a program does
    /// not wait for a cycle to be told.
    fn run_in_read_write_folder<R: ErrnoReply>(&self, reply: R, job: impl FnOnce(&WriteRequests, R) + Send + 'static) {
        if let Err(read_only) = self.access_mode_switch.check_read_write() {
            return reply.refuse(read_only);
        }
        self.run_on_thread(move |requests| job(requests, reply));
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
}

#[cfg(test)]
mod tests;
