//! Everything that changes the folder (`open` for writing, `create`, `mkdir`, `mknod`,
//! `unlink`, `rmdir`, `rename`, `write`, `setattr`, `fsync`). A read-only folder refuses them
//! all; a read-write one has each made on the recorder's thread
//! ([`crate::fuse::write_operations`]), which answers it. A file an operation opens is in
//! the open handles when it answers.

use std::ffi::OsStr;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use fuser::{Errno, FileHandle, FopenFlags, Generation, INodeNo, RenameFlags, ReplyAttr, ReplyCreate, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, TimeOrNow};
use konedrive_graph::pool::Class;
use tokio::sync::oneshot;

use crate::fuse::open_handles::Fh;
use crate::fuse::node_table::NodeId;
use super::KernelRequests;
use crate::fuse::attributes::TTL;
use crate::fuse::errno::to_errno;

impl KernelRequests {
    /// Has `job` run on the recorder's thread.
    fn run_on_recorder(&self, job: impl FnOnce(&KernelRequests) + Send + 'static) {
        let requests = self.clone();
        self.outbox_recorder.run(move || job(&requests));
    }

    /// Has `job` run on the recorder's thread once the content of the node `node_id` is in the
    /// backing directory, when writing it needs that; `refuse` is called instead when the
    /// download fails.
    fn run_with_content(&self, node_id: NodeId, empty: bool, job: impl FnOnce(&KernelRequests) + Send + 'static, refuse: impl FnOnce(Errno) + Send + 'static) {
        let Some((row, rel)) = self.write_operations.needs_download(node_id, empty) else { return self.run_on_recorder(job) };
        let requests = self.clone();
        self.runtime.spawn(async move {
            let (waiter, downloaded) = oneshot::channel();
            requests.file_downloader.download(row, rel, Class::Open, Some(waiter));
            match downloaded.await {
                Ok(Ok(())) => requests.run_on_recorder(job),
                Ok(Err(e)) => refuse(Errno::from_i32(e)),
                Err(_) => refuse(Errno::EIO),
            }
        });
    }

    pub(super) fn answer_open_to_write(&self, ino: INodeNo, truncate: bool, reply: ReplyOpen) {
        if let Err(e) = self.access_mode.check_read_write() {
            return reply.error(e);
        }
        // The two ends of one reply: whichever is called answers.
        let reply = Arc::new(Mutex::new(Some(reply)));
        let failed = Arc::clone(&reply);
        self.run_with_content(
            NodeId::from(ino),
            truncate,
            move |requests| {
                let Some(reply) = crate::panic::lock(&reply).take() else { return };
                match requests.write_operations.open_to_write(NodeId::from(ino), truncate) {
                    Ok(fh) => reply.opened(FileHandle::from(fh), FopenFlags::empty()),
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

    /// A directory (`mkdir`) or a file (`mknod`) made.
    pub(super) fn answer_make(&self, parent: INodeNo, name: &OsStr, directory: bool, reply: ReplyEntry) {
        let Some(name) = name.to_str().map(str::to_owned) else { return reply.error(Errno::EINVAL) };
        if let Err(e) = self.access_mode.check_read_write() {
            return reply.error(e);
        }
        self.run_on_recorder(move |requests| match requests.write_operations.make(NodeId::from(parent), &name, directory, false) {
            Ok((entry, _)) => {
                let node_id = requests.node_resolver.assign_node_id(&entry, NodeId::from(parent));
                reply.entry(&TTL, &requests.attributes_of(&entry, node_id), Generation(0))
            }
            Err(e) => reply.error(e),
        });
    }

    pub(super) fn answer_create(&self, parent: INodeNo, name: &OsStr, reply: ReplyCreate) {
        let Some(name) = name.to_str().map(str::to_owned) else { return reply.error(Errno::EINVAL) };
        if let Err(e) = self.access_mode.check_read_write() {
            return reply.error(e);
        }
        self.run_on_recorder(move |requests| match requests.write_operations.make(NodeId::from(parent), &name, false, true) {
            Ok((entry, opened)) => {
                let node_id = requests.node_resolver.assign_node_id(&entry, NodeId::from(parent));
                let fh = opened.unwrap_or(Fh::NOT_OPENED);
                reply.created(&TTL, &requests.attributes_of(&entry, node_id), Generation(0), FileHandle::from(fh), FopenFlags::empty())
            }
            Err(e) => reply.error(e),
        });
    }

    /// A file (`unlink`) or a directory (`rmdir`) removed.
    pub(super) fn answer_remove(&self, parent: INodeNo, name: &OsStr, directory: bool, reply: ReplyEmpty) {
        let Some(name) = name.to_str().map(str::to_owned) else { return reply.error(Errno::ENOENT) };
        if let Err(e) = self.access_mode.check_read_write() {
            return reply.error(e);
        }
        self.run_on_recorder(move |requests| match requests.write_operations.remove(NodeId::from(parent), &name, directory) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        });
    }

    pub(super) fn answer_rename(&self, parent: INodeNo, name: &OsStr, new_parent: INodeNo, new_name: &OsStr, flags: RenameFlags, reply: ReplyEmpty) {
        let (Some(name), Some(new_name)) = (name.to_str().map(str::to_owned), new_name.to_str().map(str::to_owned)) else { return reply.error(Errno::EINVAL) };
        if let Err(e) = self.access_mode.check_read_write() {
            return reply.error(e);
        }
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            return reply.error(Errno::EINVAL);
        }
        let keep = flags.contains(RenameFlags::RENAME_NOREPLACE);
        self.run_on_recorder(move |requests| match requests.write_operations.rename(NodeId::from(parent), &name, NodeId::from(new_parent), &new_name, keep) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        });
    }

    /// Bytes written through a handle that may write: into the file in the backing
    /// directory. The outbox holds the file since it was opened.
    pub(super) fn answer_write(&self, fh: FileHandle, offset: u64, data: &[u8], reply: ReplyWrite) {
        let Some((node_id, file)) = self.open_handles.find_file_to_write(Fh::from(fh)) else { return reply.error(Errno::EBADF) };
        self.open_handles.mark_written(node_id);
        match file.write_all_at(data, offset) {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(to_errno(&e)),
        }
    }

    /// A size set cuts or extends the file; a time set is the file's own from then on. Modes
    /// and owners are the mount's to say and are not kept.
    pub(super) fn answer_setattr(&self, ino: INodeNo, size: Option<u64>, mtime: Option<TimeOrNow>, reply: ReplyAttr) {
        if let Err(e) = self.access_mode.check_read_write() {
            return reply.error(e);
        }
        let set_mtime = move |requests: &KernelRequests| {
            let (Some(mtime), Some((_, rel))) = (mtime, requests.node_resolver.find_entry(NodeId::from(ino))) else { return };
            let at = match mtime {
                TimeOrNow::SpecificTime(at) => at,
                TimeOrNow::Now => SystemTime::now(),
            };
            if let Ok(file) = requests.backing_directory.read(&rel) {
                let _ = konedrive_fs::placeholder::set_mtime(&file, at);
            }
        };
        let Some(size) = size else {
            set_mtime(self);
            return self.answer_getattr(ino, reply);
        };
        let reply = Arc::new(Mutex::new(Some(reply)));
        let failed = Arc::clone(&reply);
        self.run_with_content(
            NodeId::from(ino),
            size == 0,
            move |requests| {
                let Some(reply) = crate::panic::lock(&reply).take() else { return };
                match requests.write_operations.truncate(NodeId::from(ino), size) {
                    Ok(()) => {
                        set_mtime(requests);
                        requests.answer_getattr(ino, reply)
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

    pub(super) fn answer_fsync(&self, fh: FileHandle, reply: ReplyEmpty) {
        match self.open_handles.find_file_to_write(Fh::from(fh)).map(|(_, file)| file.sync_all()) {
            Some(Err(e)) => reply.error(to_errno(&e)),
            _ => reply.ok(),
        }
    }
}
