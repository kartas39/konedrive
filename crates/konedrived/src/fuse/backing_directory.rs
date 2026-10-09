//! The backing directory: the directory on disk the mount lies over.
//!
//! [`BackingDirectory`] owns the descriptor opened on the directory before the mount, and
//! everything in it is reached through that descriptor, never through the mount. It knows the
//! disk and nothing else: not the store, not the kernel's node ids, not the outbox, not the
//! mount. Which file or directory of OneDrive an entry on disk belongs to is told by the
//! OneDrive id the entry is marked with, and whoever asks brings the id.
//!
//! What it answers for, each in a module of its own:
//!
//! - [`file_handles`]: the entry at a path, with its file handle and metadata, and the
//!   OneDrive id it is marked with;
//! - [`downloaded_copies`]: the downloaded copy of a OneDrive file — whether a path holds it,
//!   marking one, removing one;
//! - [`file_operations`]: the files and directories a change makes, renames and removes;
//! - [`directory_marks`]: the directories down to a path, each marked with its OneDrive id;
//! - [`download_landing`]: a finished download marked as a downloaded copy and given its name;
//! - [`write_bits`]: the write bits of the folder on disk, removed while no mount lies over
//!   it and restored for the daemon or the user;
//! - [`legacy_placeholders`]: a directory that holds an older konedrive's placeholders, which
//!   is never opened.
//!
//! A path is always relative to the backing directory (`rel`); the directory itself is the
//! empty path.

use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use super::stale_mount::unmount_stale_mount;

mod directory_marks;
mod download_landing;
mod downloaded_copies;
mod file_handles;
mod file_operations;
mod legacy_placeholders;
mod write_bits;

pub(super) use download_landing::LandingOutcome;
pub(super) use file_handles::{DiskEntry, DiskMetadata, EntryKind};
pub(super) use write_bits::{remove_write_bits, restore_write_bits};

/// Where an earlier build built its downloads, inside the backing directory: removed when the
/// directory is opened. A download is built in a file without a name now.
const LEGACY_STAGING_DIRECTORY: &str = ".konedrive-staging";

/// The backing directory, by the descriptor opened on it before the mount.
pub(super) struct BackingDirectory {
    descriptor: File,
}

impl BackingDirectory {
    /// Opens `folder`, which no mount lies over yet: afterwards its path is the mount, and
    /// this is the one way to what it holds. A mount a daemon that died left on it is taken
    /// off first, and a directory an older konedrive kept is refused.
    pub(super) fn open(folder: &Path) -> io::Result<Self> {
        unmount_stale_mount(folder);
        if legacy_placeholders::holds_legacy_placeholders(folder) {
            return Err(legacy_placeholders::refusal(folder));
        }
        // `fusermount3` mounts only where its user may write; its write bits were removed
        // when the daemon last ended.
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(folder, fs::Permissions::from_mode(0o755));
        }
        let backing_directory = BackingDirectory { descriptor: File::open(folder)? };
        let _ = fs::remove_dir_all(backing_directory.resolve_path(Path::new(LEGACY_STAGING_DIRECTORY)));
        Ok(backing_directory)
    }

    /// The path of the backing directory itself, through the descriptor: for what walks it
    /// without going through the mount.
    pub(super) fn resolve_root_path(&self) -> PathBuf {
        konedrive_fs::proc_path(&self.descriptor)
    }

    /// The path of `rel` in the backing directory: through the descriptor, never through the
    /// mount. Good for as long as this lives.
    pub(super) fn resolve_path(&self, rel: &Path) -> PathBuf {
        konedrive_fs::proc_path(&self.descriptor).join(rel)
    }

    /// One more descriptor of the backing directory, for what in the daemon opens the folder
    /// by its path: that path is the mount.
    pub(super) fn clone_descriptor(&self) -> io::Result<File> {
        self.descriptor.try_clone()
    }

    /// The space of the filesystem the backing directory is on.
    pub(super) fn measure_space(&self) -> io::Result<libc::statvfs> {
        // SAFETY: a plain structure the call fills.
        let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: the backing directory's own descriptor.
        match unsafe { libc::fstatvfs(self.descriptor.as_raw_fd(), &mut v) } {
            0 => Ok(v),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// The extended attribute `name` of the entry at `rel`; of the backing directory itself
    /// for the empty path.
    pub(super) fn read_attribute(&self, rel: &Path, name: &str) -> Option<String> {
        let value = match rel.as_os_str().is_empty() {
            true => xattr::FileExt::get_xattr(&self.descriptor, name),
            false => xattr::get(self.resolve_path(rel), name),
        };
        value.ok().flatten().map(|v| String::from_utf8_lossy(&v).into_owned())
    }
}

#[cfg(test)]
mod tests;
