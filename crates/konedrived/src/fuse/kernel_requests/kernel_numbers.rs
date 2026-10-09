//! The kernel's numbers at the edge of the layer: they come in fuser's types, and are the
//! layer's own — a [`NodeId`], an [`Fh`], a [`RequestId`] — from here on.

use fuser::{FileHandle, INodeNo};

use super::interruptible_reads::RequestId;
use crate::fuse::node_table::NodeId;
use crate::fuse::open_handles::Fh;

impl From<INodeNo> for NodeId {
    fn from(ino: INodeNo) -> Self {
        NodeId(ino.0)
    }
}

impl From<NodeId> for INodeNo {
    fn from(node_id: NodeId) -> Self {
        INodeNo(node_id.0)
    }
}

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
