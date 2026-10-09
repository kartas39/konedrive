//! The requests that change the folder. A read-only folder refuses them all; a read-write
//! one has each made on the recorder's thread ([`crate::fuse::write_operations`]), which
//! answers it. A file an operation opens is kept here, in the table of open handles.

use std::ffi::OsStr;
use std::os::unix::fs::FileExt;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::SystemTime;

use fuser::{Errno, FileHandle, FopenFlags, Generation, INodeNo, RenameFlags, ReplyAttr, ReplyCreate, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, TimeOrNow};
use konedrive_graph::pool::Class;
use tokio::sync::oneshot;

use super::Requests;
use crate::fuse::attributes::{self, TTL};
use crate::fuse::read::Handle;
use crate::fuse::shared::{errno, Shared};
use crate::fuse::write_operations::OpenedFile;

/// Has `job` run on the recorder's thread.
pub(super) fn on_recorder(s: &Arc<Shared>, job: impl FnOnce(&Arc<Shared>) + Send + 'static) {
    let shared = Arc::clone(s);
    s.outbox_recorder.run(move || job(&shared));
}

/// Keeps `opened`, a file opened to be written, in the table of open handles: the handle the
/// program is given.
fn keep_handle(s: &Shared, opened: OpenedFile) -> u64 {
    let OpenedFile { node_id: ino, id, rel, file } = opened;
    let fh = s.next_handle.fetch_add(1, Ordering::SeqCst);
    crate::panic::lock(&s.handles).insert(fh, Handle { ino, id, rel, copy: Some(Arc::new(file)), writes: true });
    fh
}

/// Has `job` run on the recorder's thread once the content of node `ino` is in the folder,
/// when writing it needs that; `refused` is called instead when the download fails.
fn with_content(s: &Arc<Shared>, ino: u64, empty: bool, job: impl FnOnce(&Arc<Shared>) + Send + 'static, refused: impl FnOnce(Errno) + Send + 'static) {
    let Some((row, rel)) = s.write_operations.needs_download(ino, empty) else { return on_recorder(s, job) };
    let shared = Arc::clone(s);
    s.runtime.spawn(async move {
        let (tell, told) = oneshot::channel();
        shared.ensure(row, rel, Class::Open, Some(tell));
        match told.await {
            Ok(Ok(())) => on_recorder(&shared, job),
            Ok(Err(e)) => refused(Errno::from_i32(e)),
            Err(_) => refused(Errno::EIO),
        }
    });
}

impl Requests {
    pub(super) fn opened_to_write(&self, ino: INodeNo, truncate: bool, reply: ReplyOpen) {
        let s = &self.0;
        if let Err(e) = s.may_change() {
            return reply.error(e);
        }
        // The two ends of one reply: whichever is called answers.
        let reply = Arc::new(std::sync::Mutex::new(Some(reply)));
        let failed = Arc::clone(&reply);
        with_content(
            s,
            ino.0,
            truncate,
            move |s| {
                let Some(reply) = crate::panic::lock(&reply).take() else { return };
                match s.write_operations.open_to_write(ino.0, truncate) {
                    Ok(opened) => reply.opened(FileHandle(keep_handle(s, opened)), FopenFlags::empty()),
                    Err(e) => reply.error(e),
                }
            },
            move |e| {
                if let Some(reply) = crate::panic::lock(&failed).take() {
                    reply.error(e);
                }
            },
        );
    }

    pub(super) fn made(&self, parent: INodeNo, name: &OsStr, dir: bool, reply: ReplyEntry) {
        let Some(name) = name.to_str().map(str::to_owned) else { return reply.error(Errno::EINVAL) };
        if let Err(e) = self.0.may_change() {
            return reply.error(e);
        }
        on_recorder(&self.0, move |s| match s.write_operations.make(parent.0, &name, dir, false) {
            Ok(made) => reply.entry(&TTL, &made.attr, Generation(0)),
            Err(e) => reply.error(e),
        });
    }

    pub(super) fn created(&self, parent: INodeNo, name: &OsStr, reply: ReplyCreate) {
        let Some(name) = name.to_str().map(str::to_owned) else { return reply.error(Errno::EINVAL) };
        if let Err(e) = self.0.may_change() {
            return reply.error(e);
        }
        on_recorder(&self.0, move |s| match s.write_operations.make(parent.0, &name, false, true) {
            Ok(made) => {
                let fh = made.opened.map_or(0, |opened| keep_handle(s, opened));
                reply.created(&TTL, &made.attr, Generation(0), FileHandle(fh), FopenFlags::empty())
            }
            Err(e) => reply.error(e),
        });
    }

    pub(super) fn removed(&self, parent: INodeNo, name: &OsStr, dir: bool, reply: ReplyEmpty) {
        let Some(name) = name.to_str().map(str::to_owned) else { return reply.error(Errno::ENOENT) };
        if let Err(e) = self.0.may_change() {
            return reply.error(e);
        }
        on_recorder(&self.0, move |s| match s.write_operations.remove(parent.0, &name, dir) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        });
    }

    pub(super) fn renamed(&self, parent: INodeNo, name: &OsStr, new_parent: INodeNo, new_name: &OsStr, flags: RenameFlags, reply: ReplyEmpty) {
        let (Some(name), Some(new_name)) = (name.to_str().map(str::to_owned), new_name.to_str().map(str::to_owned)) else { return reply.error(Errno::EINVAL) };
        if let Err(e) = self.0.may_change() {
            return reply.error(e);
        }
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            return reply.error(Errno::EINVAL);
        }
        let keep = flags.contains(RenameFlags::RENAME_NOREPLACE);
        on_recorder(&self.0, move |s| match s.write_operations.rename(parent.0, &name, new_parent.0, &new_name, keep) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        });
    }

    /// Bytes written through a handle that may write: into the file in the folder under the
    /// mount. The outbox holds the file since it was opened.
    pub(super) fn written(&self, fh: FileHandle, offset: u64, data: &[u8], reply: ReplyWrite) {
        let s = &self.0;
        let Some((ino, file)) = crate::panic::lock(&s.handles).get(&fh.0).filter(|h| h.writes).and_then(|h| Some((h.ino, h.copy.clone()?))) else {
            return reply.error(Errno::EBADF);
        };
        s.open_writers.mark_written(ino);
        match file.write_all_at(data, offset) {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(errno(&e)),
        }
    }

    /// A size set cuts or extends the file; a time set is the file's own from then on. Modes
    /// and owners are the mount's to say and are not kept.
    pub(super) fn set(&self, ino: INodeNo, size: Option<u64>, mtime: Option<TimeOrNow>, reply: ReplyAttr) {
        let s = &self.0;
        if let Err(e) = s.may_change() {
            return reply.error(e);
        }
        let answer = move |s: &Arc<Shared>, reply: ReplyAttr| match s.node_lookup.find_entry(ino.0) {
            Some((entry, rel)) => reply.attr(&TTL, &attributes::of_entry(&entry, ino.0, &rel, s.writable(), &s.directory_contents, &s.open_writers)),
            None => reply.error(Errno::ENOENT),
        };
        let timed = move |s: &Arc<Shared>| {
            let (Some(mtime), Some((_, rel))) = (mtime, s.node_lookup.find_entry(ino.0)) else { return };
            let at = match mtime {
                TimeOrNow::SpecificTime(at) => at,
                TimeOrNow::Now => SystemTime::now(),
            };
            if let Ok(file) = s.backing_directory.read(&rel) {
                let _ = konedrive_fs::placeholder::set_mtime(&file, at);
            }
        };
        let Some(size) = size else {
            timed(s);
            return answer(s, reply);
        };
        let reply = Arc::new(std::sync::Mutex::new(Some(reply)));
        let failed = Arc::clone(&reply);
        with_content(
            s,
            ino.0,
            size == 0,
            move |s| {
                let Some(reply) = crate::panic::lock(&reply).take() else { return };
                match s.write_operations.truncate(ino.0, size) {
                    Ok(()) => {
                        timed(s);
                        answer(s, reply)
                    }
                    Err(e) => reply.error(e),
                }
            },
            move |e| {
                if let Some(reply) = crate::panic::lock(&failed).take() {
                    reply.error(e);
                }
            },
        );
    }

    pub(super) fn synced(&self, fh: FileHandle, reply: ReplyEmpty) {
        let file = crate::panic::lock(&self.0.handles).get(&fh.0).and_then(|h| h.copy.clone().filter(|_| h.writes));
        match file.map(|file| file.sync_all()) {
            Some(Err(e)) => reply.error(errno(&e)),
            _ => reply.ok(),
        }
    }
}
