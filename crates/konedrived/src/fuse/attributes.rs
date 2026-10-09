//! The attributes the mount gives the kernel for a name: its kind, size, times and modes.
//!
//! [`attributes`] is a function of a directory entry, whose state says everything there is
//! to say, and of the access mode, which says the modes. It reads nothing: not the store, not
//! the disk.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{FileAttr, FileType, INodeNo};

use super::access_mode::AccessMode;
use super::directory_contents::{DirectoryEntry, DirectorySyncState, EntryState, FileSyncState};
use super::shared::seconds;

/// For how long the kernel may keep an entry and its attributes without asking again.
pub(super) const TTL: Duration = Duration::from_secs(1);

/// The modes the mount shows, of a directory or of a file. A read-only folder shows no write
/// bit anywhere: a file manager reads that from the modes before anything is tried.
fn permissions(access_mode: &AccessMode, directory: bool) -> u16 {
    match (access_mode.is_read_write(), directory) {
        (true, true) => 0o755,
        (true, false) => 0o644,
        (false, true) => 0o555,
        (false, false) => 0o444,
    }
}

/// The attributes of the node `node_id`, which stands for `entry`, in a folder in
/// `access_mode`.
///
/// - A directory of OneDrive has the store's time and no size.
/// - A file that is not downloaded has the store's size and time, and no blocks.
/// - A file whose downloaded copy matches has the copy's size and blocks and the store's
///   time: a download does not change when the file was last modified.
/// - A file modified here has its copy's size, blocks and time.
/// - A local-only entry has what the filesystem says of it; a directory shows no size.
pub(super) fn attributes(node_id: u64, access_mode: &AccessMode, entry: &DirectoryEntry) -> FileAttr {
    let (directory, size, blocks, modified): (bool, u64, u64, SystemTime) = match &entry.state {
        EntryState::Directory(DirectorySyncState::Synced(row)) => (true, 0, 0, seconds(row.mtime)),
        EntryState::Directory(DirectorySyncState::LocalOnly(local_only)) => (true, 0, local_only.on_disk.metadata.blocks, local_only.on_disk.metadata.modified),
        EntryState::File(FileSyncState::RemoteOnly(row)) => (false, row.size, 0, seconds(row.mtime)),
        EntryState::File(FileSyncState::Synced(row, copy)) => (false, copy.size, copy.blocks, seconds(row.mtime)),
        EntryState::File(FileSyncState::ModifiedHere(_, copy)) => (false, copy.size, copy.blocks, copy.modified),
        EntryState::File(FileSyncState::LocalOnly(local_only)) => (false, local_only.on_disk.metadata.size, local_only.on_disk.metadata.blocks, local_only.on_disk.metadata.modified),
    };
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    FileAttr {
        ino: INodeNo(node_id),
        size,
        blocks,
        atime: modified,
        mtime: modified,
        ctime: modified,
        crtime: UNIX_EPOCH,
        kind: if directory { FileType::Directory } else { FileType::RegularFile },
        perm: permissions(access_mode, directory),
        nlink: if directory { 2 } else { 1 },
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

#[cfg(test)]
mod tests;
