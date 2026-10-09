//! The requests that change a name of the folder: one made (`mkdir`, `mknod`, `create`),
//! removed (`unlink`, `rmdir`) or renamed (`rename`). Each is a write operation, put on the
//! write-operation thread with the name it is about, which answers it.

use std::ffi::OsStr;

use fuser::{Errno, FileHandle, FopenFlags, Generation, RenameFlags, ReplyCreate, ReplyEmpty, ReplyEntry};

use super::{ErrnoReply, WriteRequests};
use crate::fuse::attributes::{to_attributes, TTL};
use crate::fuse::backing_directory::EntryKind;
use crate::fuse::node_table::NodeId;
use crate::fuse::open_handles::Fh;
use crate::fuse::write_operations::{EntryToMake, RenameMode};

/// Whether `mknod` with `mode` makes anything: a regular file is made, and nothing else is
/// (`EPERM`) — a device, a pipe or a socket has nothing OneDrive could hold.
pub(super) fn check_mknod_mode(mode: u32) -> Result<(), Errno> {
    match mode & libc::S_IFMT == libc::S_IFREG {
        true => Ok(()),
        false => Err(Errno::EPERM),
    }
}

impl WriteRequests {
    /// What a rename with `flags` does to what is at the new place: it is replaced, or the
    /// rename is refused there (`RENAME_NOREPLACE`). An exchange of two names
    /// (`RENAME_EXCHANGE`) is a change the mount never takes
    /// ([`refuse_unsupported`](Self::refuse_unsupported), `EINVAL`).
    pub(super) fn find_rename_mode(&self, flags: RenameFlags) -> Result<RenameMode, Errno> {
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            return Err(self.refuse_unsupported(Errno::EINVAL));
        }
        Ok(if flags.contains(RenameFlags::RENAME_NOREPLACE) { RenameMode::NoReplace } else { RenameMode::Replace })
    }

    /// Has `operation` run on the write-operation thread with `name`, and answer `reply`.
    /// Refused before that, at once: with `not_a_name` for a name that is not text, and in a
    /// read-only folder ([`run_in_read_write_folder`](Self::run_in_read_write_folder)).
    pub(super) fn run_named_operation<R: ErrnoReply>(&self, name: &OsStr, not_a_name: Errno, reply: R, operation: impl FnOnce(&WriteRequests, &str, R) + Send + 'static) {
        let Some(name) = name.to_str().map(str::to_owned) else { return reply.refuse(not_a_name) };
        self.run_in_read_write_folder(reply, move |requests, reply| operation(requests, &name, reply));
    }

    /// A directory (`mkdir`) or a file that is not opened (`mknod`) made.
    pub(in crate::fuse::kernel_requests) fn answer_make(&self, parent: NodeId, name: &OsStr, entry_to_make: EntryToMake, reply: ReplyEntry) {
        self.run_named_operation(name, Errno::EINVAL, reply, move |requests, name, reply| match requests.write_operations.make(parent, name, entry_to_make) {
            Ok((node_id, entry, _)) => reply.entry(&TTL, &to_attributes(node_id, requests.access_mode_switch.access_mode(), &entry), Generation(0)),
            Err(e) => reply.error(e),
        });
    }

    /// `mknod`: a regular file is made, and nothing else is ([`check_mknod_mode`]).
    pub(in crate::fuse::kernel_requests) fn answer_mknod(&self, parent: NodeId, name: &OsStr, mode: u32, reply: ReplyEntry) {
        match check_mknod_mode(mode) {
            Ok(()) => self.answer_make(parent, name, EntryToMake::File, reply),
            Err(e) => reply.error(e),
        }
    }

    pub(in crate::fuse::kernel_requests) fn answer_create(&self, parent: NodeId, name: &OsStr, reply: ReplyCreate) {
        self.run_named_operation(name, Errno::EINVAL, reply, move |requests, name, reply| match requests.write_operations.make(parent, name, EntryToMake::FileOpenedForWriting) {
            Ok((node_id, entry, opened)) => {
                let attr = to_attributes(node_id, requests.access_mode_switch.access_mode(), &entry);
                reply.created(&TTL, &attr, Generation(0), FileHandle::from(opened.unwrap_or(Fh::NOT_OPENED)), FopenFlags::empty())
            }
            Err(e) => reply.error(e),
        });
    }

    /// A file (`unlink`) or a directory (`rmdir`) removed.
    pub(in crate::fuse::kernel_requests) fn answer_remove(&self, parent: NodeId, name: &OsStr, kind: EntryKind, reply: ReplyEmpty) {
        self.run_named_operation(name, Errno::ENOENT, reply, move |requests, name, reply| match requests.write_operations.remove(parent, name, kind) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        });
    }

    pub(in crate::fuse::kernel_requests) fn answer_rename(&self, parent: NodeId, name: &OsStr, new_parent: NodeId, new_name: &OsStr, flags: RenameFlags, reply: ReplyEmpty) {
        let (Some(_), Some(new_name)) = (name.to_str(), new_name.to_str().map(str::to_owned)) else { return reply.error(Errno::EINVAL) };
        let mode = match self.find_rename_mode(flags) {
            Ok(mode) => mode,
            Err(e) => return reply.error(e),
        };
        self.run_named_operation(name, Errno::EINVAL, reply, move |requests, name, reply| match requests.write_operations.rename(parent, name, new_parent, &new_name, mode) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        });
    }
}

#[cfg(test)]
mod tests;
