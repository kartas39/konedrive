//! The files and directories a change makes, renames and removes in the backing directory: each
//! one call of the filesystem at a path, with nothing decided here.

use std::fs::{self, DirEntry, File, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use super::BackingDirectory;

impl BackingDirectory {
    /// What the filesystem says of the name `rel`, a link not followed. `None` when nothing
    /// is there.
    pub(in crate::fuse) fn stat(&self, rel: &Path) -> Option<fs::Metadata> {
        fs::symlink_metadata(self.path(rel)).ok()
    }

    /// The names in the directory `rel`; none when it cannot be read.
    pub(in crate::fuse) fn list(&self, rel: &Path) -> impl Iterator<Item = DirEntry> {
        fs::read_dir(self.path(rel)).into_iter().flatten().flatten()
    }

    pub(in crate::fuse) fn make_dir(&self, rel: &Path) -> io::Result<()> {
        fs::create_dir(self.path(rel))
    }

    /// A new file at `rel`, where nothing is yet, open to read and write.
    pub(in crate::fuse) fn make_file(&self, rel: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).create_new(true).mode(0o644).open(self.path(rel))
    }

    pub(in crate::fuse) fn remove_dir(&self, rel: &Path) -> io::Result<()> {
        fs::remove_dir(self.path(rel))
    }

    pub(in crate::fuse) fn remove_file(&self, rel: &Path) -> io::Result<()> {
        fs::remove_file(self.path(rel))
    }

    pub(in crate::fuse) fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(self.path(from), self.path(to))
    }

    /// The file at `rel`, open to read.
    pub(in crate::fuse) fn read(&self, rel: &Path) -> io::Result<File> {
        File::open(self.path(rel))
    }

    /// The file at `rel`, open to read and write, cut to nothing when `truncate`.
    pub(in crate::fuse) fn write(&self, rel: &Path, truncate: bool) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).truncate(truncate).open(self.path(rel))
    }

    /// Gives the file at `rel` its owner's write bit: a downloaded copy has none.
    pub(in crate::fuse) fn writable(&self, rel: &Path) -> io::Result<()> {
        fs::set_permissions(self.path(rel), fs::Permissions::from_mode(0o644))
    }
}

#[cfg(test)]
mod tests;
