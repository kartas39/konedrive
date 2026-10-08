//! What the mount says of a name: its kind, size, times and modes.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{FileAttr, FileType, INodeNo};
use konedrive_tree::{Kind, Row};

use super::local::Entry;
use super::shared::{seconds, Shared};

pub(super) const TTL: Duration = Duration::from_secs(1);

impl Shared {
    fn plain(&self, ino: u64, dir: bool) -> FileAttr {
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        FileAttr {
            ino: INodeNo(ino),
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind: if dir { FileType::Directory } else { FileType::RegularFile },
            // A read-only folder shows no write bit anywhere: a file manager reads that
            // from the modes before anything is tried.
            perm: self.perm(dir),
            nlink: if dir { 2 } else { 1 },
            uid,
            gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// The size, blocks and time of the object `meta` is of.
    fn of_object(&self, attr: &mut FileAttr, meta: &fs::Metadata) {
        let changed = UNIX_EPOCH + Duration::new(meta.mtime().max(0) as u64, meta.mtime_nsec() as u32);
        (attr.size, attr.blocks) = (meta.size(), meta.blocks());
        (attr.atime, attr.mtime, attr.ctime) = (changed, changed, changed);
    }

    /// What the mount shows of an item at `rel`: the downloaded file's own size and blocks
    /// when the folder has it — and its time, when its content was changed here — the store's
    /// when it does not.
    pub(super) fn attr(&self, row: &Row, ino: u64, rel: Option<&Path>) -> FileAttr {
        let mut attr = self.plain(ino, row.kind == Kind::Folder);
        let listed: SystemTime = seconds(row.mtime);
        (attr.atime, attr.mtime, attr.ctime) = (listed, listed, listed);
        if row.kind == Kind::Folder {
            return attr;
        }
        attr.size = row.size;
        if let Some(meta) = rel.and_then(|rel| self.copy(rel, &row.id)) {
            if self.waiting().changed.contains(&row.id) || self.being_written(ino) {
                self.of_object(&mut attr, &meta);
            } else {
                (attr.size, attr.blocks) = (meta.size(), meta.blocks());
            }
        }
        attr
    }

    /// What the mount shows of `entry`, which is node `ino` at `rel`.
    pub(super) fn attr_of(&self, entry: &Entry, ino: u64, rel: &Path) -> FileAttr {
        match entry {
            Entry::Item(row) => self.attr(row, ino, Some(rel)),
            Entry::Local(local) => {
                let mut attr = self.plain(ino, local.meta.is_dir());
                self.of_object(&mut attr, &local.meta);
                if local.meta.is_dir() {
                    attr.size = 0;
                }
                attr
            }
        }
    }
}
