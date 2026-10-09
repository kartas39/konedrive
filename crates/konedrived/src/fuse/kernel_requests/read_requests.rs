//! A file opened to be read, read and closed (`open`, `read`, `release`).

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use fuser::{Errno, FileHandle, FopenFlags, INodeNo, OpenFlags, ReplyData, ReplyEmpty, ReplyOpen, Request};
use konedrive_graph::pool::Class;

use super::{describe_caller, KernelRequests};
use crate::fuse::directory_contents::{DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::errno::to_errno;
use crate::fuse::open_files::OpenFile;
use crate::fuse::open_writers::CloseOutcome;

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
        let Some((entry, rel)) = self.node_resolver.find_entry(ino.0) else { return reply.error(Errno::ENOENT) };
        // What is in OneDrive and not downloaded has no file in the backing directory.
        let on_disk = !matches!(entry.state, EntryState::File(FileSyncState::RemoteOnly(_)) | EntryState::Directory(DirectorySyncState::InOneDrive(_)));
        let file_on_disk = if on_disk {
            match self.backing_directory.read(&rel) {
                Ok(file) => Some(Arc::new(file)),
                Err(e) => return reply.error(to_errno(&e)),
            }
        } else {
            None
        };
        tracing::debug!("open of {} by {}: flags {:#x}, {}", rel.display(), describe_caller(req), flags.0, if file_on_disk.is_some() { "on disk" } else { "not downloaded" });
        let fh = self.open_files.add_file(OpenFile { node_id: ino.0, onedrive_id: entry.onedrive_id().map(str::to_owned), rel, file_on_disk, may_write: false });
        reply.opened(FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE);
    }

    /// The bytes asked for, from wherever they are: the file in the backing directory; what
    /// a download under way has written; or OneDrive, as a range. Enough read as ranges
    /// starts the download of the whole file, which no read waits for.
    pub(super) fn answer_read(&self, req: &Request, fh: FileHandle, offset: u64, size: u32, reply: ReplyData) {
        let Some(OpenFile { onedrive_id, rel, file_on_disk, .. }) = self.open_files.find_file(fh.0) else { return reply.error(Errno::EBADF) };
        if let Some(file) = file_on_disk {
            return answer_from_file(&file, offset, size, reply);
        }
        let Some(id) = onedrive_id else { return reply.error(Errno::EIO) };
        // Downloaded since it was opened: the handle reads the downloaded copy from now on.
        if self.backing_directory.has_downloaded_copy(&rel, &id) {
            return match self.backing_directory.read(&rel) {
                Ok(file) => {
                    let file = Arc::new(file);
                    self.open_files.set_file_on_disk(fh.0, Arc::clone(&file));
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
        let (requests, request) = (self.clone(), req.unique().0);
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
        let closed = self.open_files.remove_file(fh.0);
        if let Some(closed) = closed.filter(|closed| closed.may_write) {
            let content_changed = match self.open_writers.remove_writer(closed.node_id) {
                CloseOutcome::StillOpenElsewhere => None,
                CloseOutcome::ContentChanged => Some(true),
                CloseOutcome::ContentUntouched => Some(false),
            };
            if let Some(content_changed) = content_changed {
                let write_operations = Arc::clone(&self.write_operations);
                self.outbox_recorder.run(move || write_operations.close_last_writer(closed.node_id, content_changed));
            }
        }
        reply.ok();
    }
}
