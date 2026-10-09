//! What the mount says of a name: its kind, size, times and modes.
//!
//! [`attributes`] is a function of facts and of nothing else: whether the folder takes changes,
//! which says the modes, and what the name is ([`Shown`]). The facts of an entry are gathered
//! by [`Shown::of`], from the view and from the files open for writing; [`of_entry`] is the
//! two together, for a request's answer.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{FileAttr, FileType, INodeNo};
use konedrive_tree::Kind;

use super::shared::seconds;
use super::directory_contents::{Entry, DirectoryContents};
use super::open_writers::OpenWriters;

pub(super) const TTL: Duration = Duration::from_secs(1);

/// What the filesystem says of a file or directory of the folder under the mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OnDisk {
    pub(super) size: u64,
    pub(super) blocks: u64,
    pub(super) changed: SystemTime,
}

impl From<&fs::Metadata> for OnDisk {
    fn from(meta: &fs::Metadata) -> Self {
        let changed = UNIX_EPOCH + Duration::new(meta.mtime().max(0) as u64, meta.mtime_nsec() as u32);
        OnDisk { size: meta.size(), blocks: meta.blocks(), changed }
    }
}

/// What a name of the mount is, as far as its attributes go. `listed` is the time the store
/// has for an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Shown {
    /// An item that is a folder.
    Folder { listed: SystemTime },
    /// An item's file the folder holds no copy of: the store's size and time.
    NotDownloaded { size: u64, listed: SystemTime },
    /// An item's file with its downloaded copy. A clean copy has its own size and blocks and
    /// the store's time; one whose content is the user's `own` — changed here and not sent
    /// yet, or open for writing — has its own time too.
    Copy { copy: OnDisk, listed: SystemTime, own: bool },
    /// A file or directory OneDrive has no item for yet: the object's own.
    Local { object: OnDisk, dir: bool },
}

impl Shown {
    /// The facts of `entry`, which is node `ino` at `rel`: its copy and whether the outbox
    /// holds a change of its content are asked of `view`, whether it is being written of
    /// `writers`.
    pub(super) fn of(entry: &Entry, node_id: u64, rel: &Path, directory_contents: &DirectoryContents, open_writers: &OpenWriters) -> Shown {
        match entry {
            Entry::Local(local) => Shown::Local { object: OnDisk::from(&local.object.meta), dir: local.object.meta.is_dir() },
            Entry::Item(row) if row.kind == Kind::Folder => Shown::Folder { listed: seconds(row.mtime) },
            Entry::Item(row) => match directory_contents.backing_directory().find_downloaded_copy(rel, &row.id) {
                Some(meta) => {
                    let own = directory_contents.unsent_changes().changed.contains(&row.id) || open_writers.is_open(node_id);
                    Shown::Copy { copy: OnDisk::from(&meta), listed: seconds(row.mtime), own }
                }
                None => Shown::NotDownloaded { size: row.size, listed: seconds(row.mtime) },
            },
        }
    }

    /// An item that is gone from the store, for the one request that still has to be told it
    /// is there: its kind, and nothing else.
    pub(super) fn gone(kind: Kind) -> Shown {
        match kind {
            Kind::Folder => Shown::Folder { listed: UNIX_EPOCH },
            Kind::File => Shown::NotDownloaded { size: 0, listed: UNIX_EPOCH },
        }
    }
}

/// The modes the mount shows, of a directory or of a file. A read-only folder shows no write
/// bit anywhere: a file manager reads that from the modes before anything is tried.
fn perm(writable: bool, dir: bool) -> u16 {
    match (writable, dir) {
        (true, true) => 0o755,
        (true, false) => 0o644,
        (false, true) => 0o555,
        (false, false) => 0o444,
    }
}

/// The attributes of node `ino`, which is `shown`, in a folder that takes changes
/// (`writable`) or does not.
pub(super) fn attr(node_id: u64, writable: bool, shown: &Shown) -> FileAttr {
    let dir = matches!(shown, Shown::Folder { .. } | Shown::Local { dir: true, .. });
    let (size, blocks, changed) = match *shown {
        Shown::Folder { listed } => (0, 0, listed),
        Shown::NotDownloaded { size, listed } => (size, 0, listed),
        Shown::Copy { copy, own: true, .. } => (copy.size, copy.blocks, copy.changed),
        Shown::Copy { copy, listed, own: false } => (copy.size, copy.blocks, listed),
        Shown::Local { object, dir: true } => (0, object.blocks, object.changed),
        Shown::Local { object, dir: false } => (object.size, object.blocks, object.changed),
    };
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    FileAttr {
        ino: INodeNo(node_id),
        size,
        blocks,
        atime: changed,
        mtime: changed,
        ctime: changed,
        crtime: UNIX_EPOCH,
        kind: if dir { FileType::Directory } else { FileType::RegularFile },
        perm: perm(writable, dir),
        nlink: if dir { 2 } else { 1 },
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// What the mount shows of `entry`, which is node `ino` at `rel`.
pub(super) fn of_entry(entry: &Entry, node_id: u64, rel: &Path, writable: bool, directory_contents: &DirectoryContents, open_writers: &OpenWriters) -> FileAttr {
    attr(node_id, writable, &Shown::of(entry, node_id, rel, directory_contents, open_writers))
}

#[cfg(test)]
mod tests;
