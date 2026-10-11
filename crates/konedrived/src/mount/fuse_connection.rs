//! The connection of a mount to the kernel, as the kernel numbers it, and its abort.
//!
//! **State owned:** none: the number is read from the mount when it is asked for.
//!
//! A program that still holds a file of the mount open keeps the session, and whoever takes
//! the mount off, waiting for it. Aborting the connection first ends every request and lets
//! the mount be detached at once.

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// The connection of one mount: its number, which is the minor number of the device the
/// kernel gives the mount (`/sys/fs/fuse/connections/<n>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FuseConnection {
    number: u32,
}

impl FuseConnection {
    /// The connection of the mount that lies over `folder` now. One whose number cannot be
    /// read is numbered 0, which is no mount's: its abort fails, and is said.
    pub(super) fn of_mount_at(folder: &Path) -> Self {
        FuseConnection { number: fs::metadata(folder).map_or(0, |metadata| libc::minor(metadata.dev())) }
    }

    /// The file the kernel aborts the connection through.
    fn abort_path(&self) -> PathBuf {
        PathBuf::from(format!("/sys/fs/fuse/connections/{}/abort", self.number))
    }

    /// Aborts the connection: every request of the mount ends with an error from now on.
    pub(super) fn abort(&self) -> io::Result<()> {
        fs::write(self.abort_path(), "1")
    }
}

#[cfg(test)]
mod tests;
