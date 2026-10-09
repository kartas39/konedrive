//! The extended attributes the mount shows (`getxattr`, `listxattr`): what the Dolphin
//! plugins, `konedrivectl` and the daemon read of a file or directory. None is written
//! through the mount.
//!
//! [`XattrRequests`] is given the [`NodeResolver`], which finds the entry of a node, the
//! [`Pins`] and the [`DownloadStates`], which the value of an attribute is decided from
//! ([`to_xattr_value`]), and the [`BackingDirectory`], which the folder's own attributes and
//! the upload mark of a file are read from.

use std::ffi::OsStr;
use std::sync::Arc;

use fuser::{Errno, ReplyXattr};
use konedrive_fs::placeholder::{XATTR_DRIVE, XATTR_ITEM_ID, XATTR_PIN, XATTR_ROOT, XATTR_STATE, XATTR_SYNC};

use super::xattr_value::to_xattr_value;
use crate::fuse::backing_directory::BackingDirectory;
use crate::fuse::download_states::DownloadStates;
use crate::fuse::node_resolver::NodeResolver;
use crate::fuse::node_table::NodeId;
use crate::fuse::pins::Pins;

/// Answers with `value`, as the kernel asks for an attribute: its length for a `size` of
/// nothing, and the value when it fits.
fn answer_with_value(value: &[u8], size: u32, reply: ReplyXattr) {
    match size {
        0 => reply.size(value.len() as u32),
        n if (n as usize) < value.len() => reply.error(Errno::ERANGE),
        _ => reply.data(value),
    }
}

#[derive(Clone)]
pub(in crate::fuse) struct XattrRequests {
    node_resolver: Arc<NodeResolver>,
    backing_directory: Arc<BackingDirectory>,
    download_states: Arc<DownloadStates>,
    pins: Arc<Pins>,
}

impl XattrRequests {
    pub(in crate::fuse) fn new(node_resolver: Arc<NodeResolver>, backing_directory: Arc<BackingDirectory>, download_states: Arc<DownloadStates>, pins: Arc<Pins>) -> Self {
        XattrRequests { node_resolver, backing_directory, download_states, pins }
    }

    /// The value of the attribute `name` of the node `node_id`: the folder's own from the
    /// backing directory, as its registration and its first cycle wrote them; the mark the
    /// outbox's worker put on a file it sends, from the backing directory too; and of what
    /// is in OneDrive what the store, the pins and the download states say.
    fn find_xattr(&self, node_id: NodeId, name: &str) -> Option<String> {
        if node_id == NodeId::ROOT && (name == XATTR_ROOT || name == XATTR_DRIVE) {
            return self.backing_directory.read_folder_attribute(name);
        }
        let (entry, rel) = self.node_resolver.find_entry(node_id)?;
        if name == XATTR_SYNC {
            return self.backing_directory.read_attribute(&rel, name);
        }
        let id = entry.onedrive_id()?;
        to_xattr_value(name, &entry, self.pins.is_pinned(id), self.download_states.is_downloading(id))
    }

    pub(super) fn answer_getxattr(&self, node_id: NodeId, name: &OsStr, size: u32, reply: ReplyXattr) {
        let Some(value) = name.to_str().and_then(|name| self.find_xattr(node_id, name)) else { return reply.error(Errno::ENODATA) };
        answer_with_value(value.as_bytes(), size, reply)
    }

    pub(super) fn answer_listxattr(&self, node_id: NodeId, size: u32, reply: ReplyXattr) {
        let names = [XATTR_ROOT, XATTR_DRIVE, XATTR_ITEM_ID, XATTR_STATE, XATTR_PIN, XATTR_SYNC];
        let held: Vec<u8> = names.iter().filter(|name| self.find_xattr(node_id, name).is_some()).flat_map(|name| name.bytes().chain(std::iter::once(0))).collect();
        answer_with_value(&held, size, reply)
    }
}
