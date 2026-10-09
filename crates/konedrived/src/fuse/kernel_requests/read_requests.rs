//! A file opened to be read, read and closed (`open`, `read`, `release`).

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use fuser::{Errno, FileHandle, FopenFlags, INodeNo, OpenFlags, ReplyData, ReplyEmpty, ReplyOpen, Request};
use konedrive_graph::pool::Class;

use crate::fuse::kernel_requests::interruptible_reads::RequestId;
use crate::fuse::open_handles::Fh;
use crate::fuse::node_table::NodeId;
use super::{describe_caller, KernelRequests};
use crate::fuse::directory_contents::{DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::errno::to_errno;
use crate::fuse::open_handles::{CloseOutcome, OpenFile};

/// Answers with `size` bytes of `file` at `offset`, or as many as it has.
fn answer_from_file(file: &File, offset: u64, size: u32, reply: ReplyData) {
    let mut buf = vec![0u8; size as usize];
    let mut got = 0;
    while got < buf.len() {
        match file.read_at(&mut buf[got..], offset + got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) => return reply.error(to_errno(&e)),
        }
    }
    reply.data(&buf[..got])
}

impl KernelRequests {
    /// Opening to read downloads nothing: a program may open a file and never read it.
    pub(super) fn answer_open_to_read(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let Some((entry, rel)) = self.node_resolver.find_entry(NodeId::from(ino)) else { return reply.error(Errno::ENOENT) };
        let file = match &entry.state {
            // What is in OneDrive and not downloaded has no file in the backing directory.
            EntryState::File(FileSyncState::RemoteOnly(row)) | EntryState::Directory(DirectorySyncState::InOneDrive(row)) => {
                tracing::debug!("open of {} by {}: flags {:#x}, not downloaded", rel.display(), describe_caller(req), flags.0);
                OpenFile::ReadingRemoteOnly { onedrive_id: row.id.clone(), path: rel }
            }
            _ => match self.backing_directory.read(&rel) {
                Ok(backing_file) => {
                    tracing::debug!("open of {} by {}: flags {:#x}, on disk", rel.display(), describe_caller(req), flags.0);
                    OpenFile::ReadingBackingFile { backing_file: Arc::new(backing_file) }
                }
                Err(e) => return reply.error(to_errno(&e)),
            },
        };
        reply.opened(FileHandle::from(self.open_handles.add_file(file)), FopenFlags::FOPEN_KEEP_CACHE);
    }

    /// The bytes asked for, from wherever they are: the file in the backing directory; what
    /// a download under way has written; or OneDrive, as a range. Enough read as ranges
    /// starts the download of the whole file, which no read waits for.
    pub(super) fn answer_read(&self, req: &Request, fh: FileHandle, offset: u64, size: u32, reply: ReplyData) {
        let (id, rel) = match self.open_handles.find_file(Fh::from(fh)) {
            None => return reply.error(Errno::EBADF),
            Some(OpenFile::ReadingBackingFile { backing_file } | OpenFile::Writing { backing_file, .. }) => return answer_from_file(&backing_file, offset, size, reply),
            Some(OpenFile::ReadingRemoteOnly { onedrive_id, path }) => (onedrive_id, path),
        };
        // Downloaded since it was opened: the handle reads the downloaded copy from now on.
        // The handle has the path the file was opened at, which is not an entry looked up
        // now: the directory contents are asked about that path.
        if self.directory_contents.has_downloaded_copy_at(&rel, &id) {
            return match self.backing_directory.read(&rel) {
                Ok(file) => {
                    let file = Arc::new(file);
                    self.open_handles.switch_to_backing_file(Fh::from(fh), Arc::clone(&file));
                    answer_from_file(&file, offset, size, reply)
                }
                Err(e) => reply.error(to_errno(&e)),
            };
        }
        let Some(row) = self.directory_contents.find_row(&id) else { return reply.error(Errno::ENOENT) };
        let end = (offset + u64::from(size)).min(row.size);
        if offset >= end {
            return reply.data(&[]);
        }
        // What a download under way has written already.
        if let Some(written_file) = self.download_states.find_written_file(&id, end) {
            return answer_from_file(&written_file, offset, size, reply);
        }
        tracing::debug!("read of {} by {}: {} bytes at {offset}, from OneDrive", rel.display(), describe_caller(req), end - offset);
        let (requests, request) = (self.clone(), RequestId::from(req.unique()));
        let read = self.runtime.spawn(async move {
            let fetched = requests.range_readers.fetch_range(&row, offset, end).await;
            requests.interruptible_reads.remove(request);
            match fetched {
                Ok(bytes) => {
                    let read_enough = requests.download_states.count_bytes_read(&row.id, bytes.len() as u64, row.size);
                    reply.data(&bytes);
                    if read_enough {
                        requests.file_downloader.download(row, rel, Class::Open, None);
                    }
                }
                Err(e) => reply.error(Errno::from_i32(e)),
            }
        });
        self.interruptible_reads.add(request, read.abort_handle());
        if read.is_finished() {
            self.interruptible_reads.remove(request);
        }
    }

    /// A handle is closed: the last one that wrote a file leaves its content ready to send.
    pub(super) fn answer_release(&self, fh: FileHandle, reply: ReplyEmpty) {
        let (closed, outcome) = self.open_handles.close_file(Fh::from(fh));
        match (closed, outcome) {
            (Some(OpenFile::Writing { node_id, .. }), CloseOutcome::ContentChanged | CloseOutcome::ContentUntouched) => {
                let write_operations = Arc::clone(&self.write_operations);
                self.outbox_recorder.run(move || write_operations.close_last_writer(node_id, outcome));
            }
            (_, CloseOutcome::NotOpen | CloseOutcome::StillOpenElsewhere | CloseOutcome::ContentChanged | CloseOutcome::ContentUntouched) => {}
        }
        reply.ok();
    }
}
