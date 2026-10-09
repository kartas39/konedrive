//! The extended attributes the mount shows (`getxattr`, `listxattr`): what the Dolphin
//! plugins, `konedrivectl` and the daemon read of a file or directory. None is written
//! through the mount.

use std::ffi::OsStr;
use std::path::Path;

use fuser::{Errno, INodeNo, ReplyXattr};
use konedrive_fs::placeholder::{XATTR_DRIVE, XATTR_ITEM_ID, XATTR_PIN, XATTR_ROOT, XATTR_STATE, XATTR_SYNC};

use super::KernelRequests;
use crate::fuse::directory_contents::EntryState;
use crate::fuse::download_states::to_hydration_state;

impl KernelRequests {
    /// The value of the attribute `name` of the node `ino`: of what is in OneDrive from the
    /// store, the pins and the download states, and what the outbox's worker marked on a
    /// file it sends from the backing directory.
    fn find_xattr(&self, ino: INodeNo, name: &str) -> Option<String> {
        if ino.0 == 1 && (name == XATTR_ROOT || name == XATTR_DRIVE) {
            // The folder's own: what its registration and its first cycle wrote on it.
            return self.backing_directory.read_attribute(Path::new(""), name);
        }
        let (entry, rel) = self.node_resolver.find_entry(ino.0)?;
        if name == XATTR_SYNC {
            return self.backing_directory.read_attribute(&rel, name);
        }
        let row = entry.row()?;
        match (name, &entry.state) {
            (XATTR_ITEM_ID, _) => Some(row.id.clone()),
            (XATTR_STATE, EntryState::File(file)) => Some(to_hydration_state(file, self.download_states.is_downloading(&row.id))?.as_str().to_owned()),
            (XATTR_PIN, _) if self.pins.is_pinned(&row.id) => Some("1".to_owned()),
            _ => None,
        }
    }

    pub(super) fn answer_getxattr(&self, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let Some(value) = name.to_str().and_then(|name| self.find_xattr(ino, name)) else { return reply.error(Errno::ENODATA) };
        match size {
            0 => reply.size(value.len() as u32),
            n if (n as usize) < value.len() => reply.error(Errno::ERANGE),
            _ => reply.data(value.as_bytes()),
        }
    }

    pub(super) fn answer_listxattr(&self, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let names = [XATTR_ROOT, XATTR_DRIVE, XATTR_ITEM_ID, XATTR_STATE, XATTR_PIN, XATTR_SYNC];
        let held: Vec<u8> = names.iter().filter(|n| self.find_xattr(ino, n).is_some()).flat_map(|n| n.bytes().chain(std::iter::once(0))).collect();
        match size {
            0 => reply.size(held.len() as u32),
            n if (n as usize) < held.len() => reply.error(Errno::ERANGE),
            _ => reply.data(&held),
        }
    }
}
