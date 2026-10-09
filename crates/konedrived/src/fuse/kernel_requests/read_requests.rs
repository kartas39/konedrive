//! A file opened to be read, and read (`open`, `read`, `interrupt`).
//!
//! [`ReadRequests`] is given the [`NodeResolver`], the [`DirectoryContents`] and the
//! [`BackingDirectory`], which say what a file is and open its file on disk; the
//! [`OpenHandles`], which keep what was opened; the [`DownloadStates`], the
//! [`RangeFetcher`] and the [`FileDownloader`], for a file that is not downloaded; the
//! [`InterruptibleReads`], which give up a read that waits for OneDrive; and the runtime
//! such a read waits on. Where the bytes of a read come from is found as a value
//! ([`ReadRequests::find_read_origin`], a [`ReadOrigin`] that carries what is read from),
//! and the read is then answered from it; what is decided of a file that is not downloaded
//! is decided in [`read_origin`](super::read_origin).

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::Arc;

use fuser::{Errno, FileHandle, FopenFlags, OpenFlags, ReplyData, ReplyOpen, Request};
use konedrive_graph::pool::Class;
use konedrive_tree::Row;

use super::describe_caller;
use super::interruptible_reads::{InterruptibleReads, RequestId};
use super::read_origin::{choose_read_origin, ReadOrigin};
use crate::fuse::backing_directory::BackingDirectory;
use crate::fuse::directory_contents::{DirectoryContents, DirectorySyncState, EntryState, FileSyncState};
use crate::fuse::download_states::DownloadStates;
use crate::fuse::errno::to_errno;
use crate::fuse::file_downloader::{FileDownloader, FileToDownload};
use crate::fuse::node_resolver::NodeResolver;
use crate::fuse::node_table::NodeId;
use crate::fuse::open_handles::{Fh, OpenFile, OpenHandles};
use crate::fuse::range_fetcher::RangeFetcher;

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

#[derive(Clone)]
pub(in crate::fuse) struct ReadRequests {
    node_resolver: Arc<NodeResolver>,
    directory_contents: Arc<DirectoryContents>,
    backing_directory: Arc<BackingDirectory>,
    open_handles: Arc<OpenHandles>,
    download_states: Arc<DownloadStates>,
    range_fetcher: Arc<RangeFetcher>,
    file_downloader: Arc<FileDownloader>,
    /// The reads that wait for a range from OneDrive, by request: shared with the mount,
    /// which gives them all up when it is taken off.
    interruptible_reads: Arc<InterruptibleReads>,
    /// Where a read waits for OneDrive.
    runtime: tokio::runtime::Handle,
}

impl ReadRequests {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::fuse) fn new(
        node_resolver: Arc<NodeResolver>,
        directory_contents: Arc<DirectoryContents>,
        backing_directory: Arc<BackingDirectory>,
        open_handles: Arc<OpenHandles>,
        download_states: Arc<DownloadStates>,
        range_fetcher: Arc<RangeFetcher>,
        file_downloader: Arc<FileDownloader>,
        interruptible_reads: Arc<InterruptibleReads>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        ReadRequests { node_resolver, directory_contents, backing_directory, open_handles, download_states, range_fetcher, file_downloader, interruptible_reads, runtime }
    }

    /// Opening to read downloads nothing: a program may open a file and never read it.
    pub(super) fn answer_open_to_read(&self, req: &Request, node_id: NodeId, flags: OpenFlags, reply: ReplyOpen) {
        let Some((entry, rel)) = self.node_resolver.find_entry(node_id) else { return reply.error(Errno::ENOENT) };
        let file = match &entry.state {
            // What is in OneDrive and not downloaded has no file in the backing directory.
            EntryState::File(FileSyncState::RemoteOnly(row)) | EntryState::Directory(DirectorySyncState::InOneDrive(row)) => {
                tracing::debug!("open of {} by {}: flags {:#x}, not downloaded", rel.display(), describe_caller(req), flags.0);
                OpenFile::ReadingRemoteOnly { onedrive_id: row.id.clone(), path: rel }
            }
            _ => match self.backing_directory.open_to_read(&rel) {
                Ok(backing_file) => {
                    tracing::debug!("open of {} by {}: flags {:#x}, on disk", rel.display(), describe_caller(req), flags.0);
                    OpenFile::ReadingBackingFile { backing_file: Arc::new(backing_file) }
                }
                Err(e) => return reply.error(to_errno(&e)),
            },
        };
        reply.opened(FileHandle::from(self.open_handles.add_file(file)), FopenFlags::FOPEN_KEEP_CACHE);
    }

    /// The bytes asked for, from wherever they are ([`find_read_origin`](Self::find_read_origin)).
    pub(super) fn answer_read(&self, req: &Request, fh: Fh, offset: u64, size: u32, reply: ReplyData) {
        match self.find_read_origin(fh, offset, size) {
            Ok(ReadOrigin::BackingFile(file) | ReadOrigin::RunningDownload(file)) => answer_from_file(&file, offset, size, reply),
            Ok(ReadOrigin::OneDriveRange { row, path, from, end }) => self.answer_from_onedrive(req, row, path, from, end, reply),
            Ok(ReadOrigin::Nothing) => reply.data(&[]),
            Err(e) => reply.error(e),
        }
    }

    /// Where the `size` bytes at `offset` of the file held open by `fh` come from, or the
    /// errno the read is refused with: `EBADF` for an `fh` that holds no file open, the
    /// disk's when a downloaded copy cannot be opened, and `ENOENT` for a file that is not
    /// downloaded and is gone from the store.
    ///
    /// - A file opened from the backing directory is read there.
    /// - A file opened while it was not downloaded, and downloaded since: its copy is
    ///   opened, and the handle reads it from now on. The handle has the path the file was
    ///   opened at, which is not an entry looked up now: the directory contents are asked
    ///   about that path.
    /// - A file that is still not downloaded: decided from the store's row of it and from a
    ///   download of it that runs now ([`choose_read_origin`]).
    pub(super) fn find_read_origin(&self, fh: Fh, offset: u64, size: u32) -> Result<ReadOrigin, Errno> {
        match self.open_handles.find_file(fh).ok_or(Errno::EBADF)? {
            OpenFile::ReadingBackingFile { backing_file } | OpenFile::Writing { backing_file, .. } => Ok(ReadOrigin::BackingFile(backing_file)),
            OpenFile::ReadingRemoteOnly { onedrive_id, path } if self.directory_contents.has_downloaded_copy_at(&path, &onedrive_id) => {
                let backing_file = Arc::new(self.backing_directory.open_to_read(&path).map_err(|e| to_errno(&e))?);
                self.open_handles.switch_to_backing_file(fh, Arc::clone(&backing_file));
                Ok(ReadOrigin::BackingFile(backing_file))
            }
            OpenFile::ReadingRemoteOnly { onedrive_id, path } => {
                let row = self.directory_contents.find_row(&onedrive_id).ok_or(Errno::ENOENT)?;
                Ok(choose_read_origin(row, path, self.download_states.find_running_download(&onedrive_id), offset, size))
            }
        }
    }

    /// The bytes from `from` up to `end` of the OneDrive file `row`, fetched as a range on
    /// the runtime; the read is given up when its program is interrupted. Enough read as
    /// ranges starts the download of the whole file, which no read waits for.
    fn answer_from_onedrive(&self, req: &Request, row: Row, path: PathBuf, from: u64, end: u64, reply: ReplyData) {
        tracing::debug!("read of {} by {}: {} bytes at {from}, from OneDrive", path.display(), describe_caller(req), end - from);
        let (requests, request) = (self.clone(), RequestId::from(req.unique()));
        let read = self.runtime.spawn(async move {
            let fetched = requests.range_fetcher.fetch_range(&row, from, end).await;
            requests.interruptible_reads.remove(request);
            match fetched {
                Ok(bytes) => {
                    let read_enough = requests.download_states.count_bytes_read(&row.id, bytes.len() as u64, row.size);
                    reply.data(&bytes);
                    if read_enough {
                        requests.file_downloader.download(FileToDownload { row, path }, Class::Open, None);
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

    /// The program behind the request `request_id` was interrupted or killed: its read is
    /// given up, and answered with an error as its reply is dropped.
    pub(super) fn interrupt(&self, request_id: RequestId) {
        self.interruptible_reads.interrupt(request_id);
    }
}

#[cfg(test)]
mod tests;
