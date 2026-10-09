//! The kernel's numbers of a request at the edge of the layer: they come in fuser's types,
//! and are the layer's own — an [`Fh`], a [`RequestId`] — from here on. A node id is the
//! kernel's inode number wherever the layer hands one to the kernel, also outside the
//! requests (the attributes, the inotify bridge's invalidation): that pair is with
//! [`NodeId`](crate::fuse::node_table::NodeId) itself.

use fuser::FileHandle;

use super::interruptible_reads::RequestId;
use crate::fuse::open_handles::Fh;

impl From<FileHandle> for Fh {
    fn from(fh: FileHandle) -> Self {
        Fh(fh.0)
    }
}

impl From<Fh> for FileHandle {
    fn from(fh: Fh) -> Self {
        FileHandle(fh.0)
    }
}

impl From<fuser::RequestId> for RequestId {
    fn from(unique: fuser::RequestId) -> Self {
        RequestId(unique.0)
    }
}
