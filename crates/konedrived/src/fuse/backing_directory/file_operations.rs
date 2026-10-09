//! The files and directories a change makes, renames and removes in the backing directory: each
//! one call of the filesystem at a path, with nothing decided here.

use std::fs::{self, DirEntry, File, OpenOptions};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use super::{to_mode_bits, BackingDirectory, EntryKind, WriteBits};

impl BackingDirectory {
    /// What the filesystem says of the name `rel`, a link not followed. `None` when nothing
    /// is there.
    pub(in crate::fuse) fn read_metadata(&self, rel: &Path) -> Option<fs::Metadata> {
        fs::symlink_metadata(self.resolve_path(rel)).ok()
    }

    /// The names in the directory `rel`; none when it cannot be read.
    pub(in crate::fuse) fn list_directory(&self, rel: &Path) -> impl Iterator<Item = DirEntry> {
        fs::read_dir(self.resolve_path(rel)).into_iter().flatten().flatten()
    }

    pub(in crate::fuse) fn make_directory(&self, rel: &Path) -> io::Result<()> {
        fs::create_dir(self.resolve_path(rel))
    }

    /// A new file at `rel`, where nothing is yet, open to read and write.
    pub(in crate::fuse) fn make_file(&self, rel: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).create_new(true).mode(to_mode_bits(EntryKind::File, WriteBits::Present)).open(self.resolve_path(rel))
    }

    pub(in crate::fuse) fn remove_directory(&self, rel: &Path) -> io::Result<()> {
        fs::remove_dir(self.resolve_path(rel))
    }

    pub(in crate::fuse) fn remove_file(&self, rel: &Path) -> io::Result<()> {
        fs::remove_file(self.resolve_path(rel))
    }

    pub(in crate::fuse) fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(self.resolve_path(from), self.resolve_path(to))
    }

    /// Exchanges what is at `from` and what is at `to`, both of which are there, in one
    /// step: each is at the other's path afterwards (`RENAME_EXCHANGE`).
    pub(in crate::fuse) fn exchange(&self, from: &Path, to: &Path) -> io::Result<()> {
        nix::fcntl::renameat2(self.descriptor.as_fd(), from, self.descriptor.as_fd(), to, nix::fcntl::RenameFlags::RENAME_EXCHANGE).map_err(io::Error::from)
    }

    /// The file at `rel`, open to read.
    pub(in crate::fuse) fn open_to_read(&self, rel: &Path) -> io::Result<File> {
        File::open(self.resolve_path(rel))
    }

    /// The file at `rel`, open to read and write, cut to nothing when `truncate`.
    pub(in crate::fuse) fn open_to_write(&self, rel: &Path, truncate: bool) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).truncate(truncate).open(self.resolve_path(rel))
    }

    /// Gives the file at `rel` its owner's write bit: a downloaded copy has none.
    pub(in crate::fuse) fn grant_write_bit(&self, rel: &Path) -> io::Result<()> {
        fs::set_permissions(self.resolve_path(rel), fs::Permissions::from_mode(to_mode_bits(EntryKind::File, WriteBits::Present)))
    }
}

#[cfg(test)]
mod tests;
