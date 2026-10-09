//! The requests that open and read a file.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use fuser::{Errno, FileHandle, FopenFlags, INodeNo, OpenFlags, ReplyData, ReplyEmpty, ReplyOpen, Request};
use konedrive_graph::pool::Class;

use super::{caller, Requests};
use crate::fuse::fill::{Running, WRITTEN_BEHIND};
use crate::fuse::directory_contents::Entry;
use crate::fuse::read::Handle;
use crate::fuse::shared::{errno, Shared};
use crate::fuse::writers::Closed;

/// `size` bytes of `file` at `offset`, or as many as it has.
fn from_file(file: &File, offset: u64, size: u32, reply: ReplyData) {
    let mut buf = vec![0u8; size as usize];
    let mut got = 0;
    while got < buf.len() {
        match file.read_at(&mut buf[got..], offset + got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) => return reply.error(errno(&e)),
        }
    }
    reply.data(&buf[..got])
}

impl Requests {
    /// Opening to read downloads nothing: a program may open a file and never read it.
    /// Opening to write is a change of the folder ([`Self::opened_to_write`]).
    pub(super) fn opened(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let s = &self.0;
        if flags.0 & libc::O_ACCMODE != libc::O_RDONLY || flags.0 & libc::O_TRUNC != 0 {
            return self.opened_to_write(ino, flags.0 & libc::O_TRUNC != 0, reply);
        }
        let Some((entry, rel)) = s.resolve.node(ino.0) else { return reply.error(Errno::ENOENT) };
        let on_disk = match &entry {
            Entry::Item(row) => s.under.has_copy(&rel, &row.id),
            Entry::Local(_) => true,
        };
        let copy = if on_disk {
            match s.under.read(&rel) {
                Ok(file) => Some(Arc::new(file)),
                Err(e) => return reply.error(errno(&e)),
            }
        } else {
            None
        };
        tracing::debug!("open of {} by {}: flags {:#x}, {}", rel.display(), caller(req), flags.0, if copy.is_some() { "on disk" } else { "not downloaded" });
        let fh = s.next_handle.fetch_add(1, Ordering::SeqCst);
        crate::panic::lock(&s.handles).insert(fh, Handle { ino: ino.0, id: entry.id().map(str::to_owned), rel, copy, writes: false });
        reply.opened(FileHandle(fh), FopenFlags::FOPEN_KEEP_CACHE);
    }

    /// The bytes asked for, from wherever they are: the file in the folder under the mount;
    /// what a download under way has written; or OneDrive, as a range. Enough read as ranges
    /// starts the download of the whole file, which no read waits for.
    pub(super) fn read(&self, req: &Request, fh: FileHandle, offset: u64, size: u32, reply: ReplyData) {
        let s = &self.0;
        let Some((id, rel, copy)) = crate::panic::lock(&s.handles).get(&fh.0).map(|h| (h.id.clone(), h.rel.clone(), h.copy.clone())) else {
            return reply.error(Errno::EBADF);
        };
        if let Some(copy) = copy {
            return from_file(&copy, offset, size, reply);
        }
        let Some(id) = id else { return reply.error(Errno::EIO) };
        // Downloaded since it was opened: the handle reads the copy from now on.
        if s.under.has_copy(&rel, &id) {
            return match s.under.read(&rel) {
                Ok(file) => {
                    let file = Arc::new(file);
                    if let Some(handle) = crate::panic::lock(&s.handles).get_mut(&fh.0) {
                        handle.copy = Some(Arc::clone(&file));
                    }
                    from_file(&file, offset, size, reply)
                }
                Err(e) => reply.error(errno(&e)),
            };
        }
        let Some(row) = s.view.row(&id) else { return reply.error(Errno::ENOENT) };
        let end = (offset + u64::from(size)).min(row.size);
        if offset >= end {
            return reply.data(&[]);
        }
        // What a download under way has written already.
        let running = crate::panic::lock(&s.running).get(&id).cloned();
        if let Some(Running { mark, staged }) = running {
            if end + WRITTEN_BEHIND <= mark.load(Ordering::SeqCst) {
                return from_file(&staged, offset, size, reply);
            }
        }
        tracing::debug!("read of {} by {}: {} bytes at {offset}, from OneDrive", rel.display(), caller(req), end - offset);
        let (shared, unique): (Arc<Shared>, u64) = (Arc::clone(s), req.unique().0);
        let task = s.runtime.spawn(async move {
            let fetched = shared.range(&row, offset, end).await;
            crate::panic::lock(&shared.reads).remove(&unique);
            match fetched {
                Ok(bytes) => {
                    let asked = {
                        let mut asked = crate::panic::lock(&shared.asked);
                        let total = asked.entry(row.id.clone()).or_default();
                        *total += bytes.len() as u64;
                        *total
                    };
                    reply.data(&bytes);
                    // A file smaller than the threshold is downloaded once all of it was read:
                    // it would be fetched again at every read otherwise.
                    if asked >= shared.download_after.min(row.size) {
                        shared.ensure(row, rel, Class::Open, None);
                    }
                }
                Err(e) => reply.error(Errno::from_i32(e)),
            }
        });
        crate::panic::lock(&s.reads).insert(unique, task.abort_handle());
        if task.is_finished() {
            crate::panic::lock(&s.reads).remove(&unique);
        }
    }

    /// A handle is closed: the last one that wrote a file leaves its content ready to send.
    pub(super) fn released(&self, fh: FileHandle, reply: ReplyEmpty) {
        let s = &self.0;
        let closed = crate::panic::lock(&s.handles).remove(&fh.0);
        if let Some(handle) = closed.filter(|handle| handle.writes) {
            let changed = match s.writers.closed(handle.ino) {
                Closed::StillOpen => None,
                Closed::Changed => Some(true),
                Closed::Untouched => Some(false),
            };
            if let Some(changed) = changed {
                super::change::on_recorder(s, move |s| s.write_operations.closed(handle.ino, changed));
            }
        }
        reply.ok();
    }
}
