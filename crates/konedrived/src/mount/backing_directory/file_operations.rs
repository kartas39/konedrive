//! The files and directories a change makes and removes in the backing directory, and the
//! file without a name a download is built in: each one call of the filesystem, with nothing
//! decided here.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use super::{to_mode_bits, BackingDirectory, EntryKind, WriteBits};

impl BackingDirectory {
    /// What the filesystem says of the name `rel`, a link not followed. `None` when nothing
    /// is there.
    pub(in crate::mount) fn read_metadata(&self, rel: &Path) -> Option<fs::Metadata> {
        fs::symlink_metadata(self.resolve_path(rel)).ok()
    }

    /// The names in the directory `rel`, or why it cannot be read.
    pub(in crate::mount) fn read_directory(&self, rel: &Path) -> io::Result<fs::ReadDir> {
        fs::read_dir(self.resolve_path(rel))
    }

    pub(in crate::mount) fn make_directory(&self, rel: &Path) -> io::Result<()> {
        fs::create_dir(self.resolve_path(rel))
    }

    /// A new file at `rel`, where nothing is yet, open to read and write.
    pub(in crate::mount) fn make_file(&self, rel: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).create_new(true).mode(to_mode_bits(EntryKind::File, WriteBits::Present)).open(self.resolve_path(rel))
    }

    /// A file of the backing directory's filesystem that is in no directory, to build a
    /// download in.
    pub(in crate::mount) fn make_unnamed_file(&self) -> io::Result<File> {
        File::options().read(true).write(true).custom_flags(libc::O_TMPFILE).mode(to_mode_bits(EntryKind::File, WriteBits::Present)).open(self.resolve_root_directory_path())
    }

    pub(in crate::mount) fn remove_directory(&self, rel: &Path) -> io::Result<()> {
        fs::remove_dir(self.resolve_path(rel))
    }

    pub(in crate::mount) fn remove_file(&self, rel: &Path) -> io::Result<()> {
        fs::remove_file(self.resolve_path(rel))
    }

    /// The file at `rel`, open to read.
    pub(in crate::mount) fn open_to_read(&self, rel: &Path) -> io::Result<File> {
        File::open(self.resolve_path(rel))
    }

    /// The file at `rel`, open to read and write, with the content it has.
    pub(in crate::mount) fn open_to_write(&self, rel: &Path) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).open(self.resolve_path(rel))
    }

    /// Gives the file at `rel` its owner's write bit: a downloaded copy has none.
    pub(in crate::mount) fn grant_write_bit(&self, rel: &Path) -> io::Result<()> {
        fs::set_permissions(self.resolve_path(rel), fs::Permissions::from_mode(to_mode_bits(EntryKind::File, WriteBits::Present)))
    }
}

#[cfg(test)]
mod tests;
