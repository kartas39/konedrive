//! What the mount says of an item: its kind, size, times and modes.

use std::os::unix::fs::MetadataExt;
use std::time::{Duration, UNIX_EPOCH};

use fuser::{FileAttr, FileType, INodeNo};
use konedrive_tree::{Kind, Row};

use super::shared::{seconds, Shared};

pub(super) const TTL: Duration = Duration::from_secs(1);

impl Shared {
    /// What the mount shows of a row: the downloaded file's own size and blocks when the folder
    /// has it, the store's when it does not.
    pub(super) fn attr(&self, row: &Row, ino: u64) -> FileAttr {
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let mut attr = FileAttr {
            ino: INodeNo(ino),
            size: row.size,
            blocks: 0,
            atime: seconds(row.mtime),
            mtime: seconds(row.mtime),
            ctime: seconds(row.mtime),
            crtime: UNIX_EPOCH,
            kind: FileType::RegularFile,
            // No write bit anywhere: the folder takes no change from a program, and a file
            // manager reads that from the modes before anything is tried, as it does in a
            // read-only folder of placeholders.
            perm: 0o444,
            nlink: 1,
            uid,
            gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        };
        if row.kind == Kind::Folder {
            attr.kind = FileType::Directory;
            attr.perm = 0o555;
            attr.nlink = 2;
            attr.size = 0;
            return attr;
        }
        if let Some(meta) = self.rel_of_ino(ino).and_then(|rel| self.copy(&rel, &row.id)) {
            attr.size = meta.size();
            attr.blocks = meta.blocks();
        }
        attr
    }
}
