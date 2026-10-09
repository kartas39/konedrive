//! The requests that change a file's content or its time: one opened for writing (`open`),
//! written (`write`), truncated or given a time (`setattr`), written out (`fsync`) and closed
//! (`release`). The open, the truncate, the time and the close of a writer are write operations,
//! on the write-operation thread; `write` and `fsync` go into the file an open put into the
//! open handles, on the mount's own thread.

use std::os::unix::fs::FileExt;

use fuser::{Errno, FileHandle, FopenFlags, ReplyAttr, ReplyEmpty, ReplyOpen, ReplyWrite, TimeOrNow};

use super::{ErrnoReply, WriteRequests};
use crate::fuse::errno::to_errno;
use crate::fuse::kernel_requests::lookup_requests::answer_with_attributes;
use crate::fuse::node_table::NodeId;
use crate::fuse::open_handles::Fh;

impl WriteRequests {
    /// A file opened for writing, with the content it has.
    pub(in crate::fuse::kernel_requests) fn answer_open_to_write(&self, node_id: NodeId, reply: ReplyOpen) {
        self.run_with_content(node_id, reply, move |requests, reply| match requests.write_operations.open_to_write(node_id) {
            Ok(fh) => reply.opened(FileHandle::from(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        });
    }

    /// A file opened for writing and truncated to nothing (`O_TRUNC`): nothing of it is
    /// downloaded first.
    pub(in crate::fuse::kernel_requests) fn answer_open_to_overwrite(&self, node_id: NodeId, reply: ReplyOpen) {
        self.run_in_read_write_folder(reply, move |requests, reply| match requests.write_operations.open_to_overwrite(node_id) {
            Ok(fh) => reply.opened(FileHandle::from(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        });
    }

    pub(in crate::fuse::kernel_requests) fn answer_write(&self, fh: Fh, offset: u64, data: &[u8], reply: ReplyWrite) {
        match self.write(fh, offset, data) {
            Ok(written) => reply.written(written),
            Err(e) => reply.error(e),
        }
    }

    /// Writes `data` at `offset` through `fh`, a handle that may write (`EBADF` for one that
    /// may not): into the file in the backing directory, whose node is marked written. How
    /// many bytes were written. The outbox holds the file since it was opened.
    fn write(&self, fh: Fh, offset: u64, data: &[u8]) -> Result<u32, Errno> {
        let (node_id, file) = self.open_handles.find_file_to_write(fh).ok_or(Errno::EBADF)?;
        self.open_handles.mark_written(node_id);
        file.write_all_at(data, offset).map_err(|e| to_errno(&e))?;
        Ok(data.len() as u32)
    }

    /// A size set truncates or extends the file; a time set alone is the file's own from then
    /// on. Either is a write operation, made on the write-operation thread. Modes and owners
    /// are the mount's to say and are not kept.
    pub(in crate::fuse::kernel_requests) fn answer_setattr(&self, node_id: NodeId, size: Option<u64>, mtime: Option<TimeOrNow>, reply: ReplyAttr) {
        let Some(size) = size else { return self.run_set_modified_time(node_id, mtime, reply, WriteRequests::answer_with_attributes) };
        let truncate = move |requests: &WriteRequests, reply: ReplyAttr| match requests.write_operations.truncate(node_id, size, mtime) {
            Ok(()) => requests.answer_with_attributes(node_id, reply),
            Err(e) => reply.error(e),
        };
        // Truncated to nothing, its content is not needed.
        match size {
            0 => self.run_in_read_write_folder(reply, truncate),
            _ => self.run_with_content(node_id, reply, truncate),
        }
    }

    /// Has the time `mtime` gives set on the node `node_id` by the write operation, on the
    /// write-operation thread, and `answer` called with `reply` once it is; `reply` is
    /// refused instead when the folder is read-only — at once, and again by the operation
    /// when the folder was switched meanwhile.
    fn run_set_modified_time<R: ErrnoReply>(&self, node_id: NodeId, mtime: Option<TimeOrNow>, reply: R, answer: impl FnOnce(&WriteRequests, NodeId, R) + Send + 'static) {
        self.run_in_read_write_folder(reply, move |requests, reply| match requests.write_operations.set_modified_time(node_id, mtime) {
            Ok(()) => answer(requests, node_id, reply),
            Err(e) => reply.refuse(e),
        });
    }

    /// The attributes of the node `node_id` as they are once a `setattr` was made.
    fn answer_with_attributes(&self, node_id: NodeId, reply: ReplyAttr) {
        answer_with_attributes(&self.node_resolver, self.access_mode_switch.access_mode(), node_id, reply)
    }

    pub(in crate::fuse::kernel_requests) fn answer_fsync(&self, fh: Fh, reply: ReplyEmpty) {
        match self.sync_file(fh) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    /// Writes out to the disk what was written through `fh`. Nothing for a handle that may
    /// not write: it wrote nothing.
    fn sync_file(&self, fh: Fh) -> Result<(), Errno> {
        match self.open_handles.find_file_to_write(fh) {
            Some((_, file)) => file.sync_all().map_err(|e| to_errno(&e)),
            None => Ok(()),
        }
    }

    /// A handle of a file is closed (`release`). Answered at once: the kernel does not wait
    /// for what the close of a writer records.
    pub(in crate::fuse::kernel_requests) fn answer_release(&self, fh: Fh, reply: ReplyEmpty) {
        self.close_file(fh);
        reply.ok();
    }

    /// Closes the file a program held open by `fh`. One opened to be read is closed here.
    /// One opened for writing is closed by a write operation, on the write-operation thread,
    /// and the node it writes is given: whether it was the node's last writer, and what that
    /// leaves to send, is asked of the open handles there and recorded in the same step —
    /// an answer taken here would be acted on only after every operation that waits for the
    /// thread, an open or a truncate of the same file among them. Until then the file counts as
    /// open for writing.
    fn close_file(&self, fh: Fh) -> Option<NodeId> {
        let Some((node_id, _)) = self.open_handles.find_file_to_write(fh) else {
            self.open_handles.close_file(fh);
            return None;
        };
        self.run_on_thread(move |requests| requests.write_operations.close_writer(fh));
        Some(node_id)
    }
}

#[cfg(test)]
mod tests;
