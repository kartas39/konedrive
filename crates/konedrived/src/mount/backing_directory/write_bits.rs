//! The write bits of the folder on disk: taken off while no mount lies over it, so that
//! nothing is put into it by mistake, and given back to its owner when a mount is put over
//! it, or when the folder is the user's own again.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::{BackingDirectory, EntryKind};

/// Whether an entry has its owner's write bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::mount) enum WriteBits {
    /// It has: what the owner may change.
    Present,
    /// It has none: nothing is changed in it.
    Removed,
}

/// The mode of a directory or a file with or without its owner's write bit: the one table of
/// the modes the layer sets on disk and shows through the mount.
pub(in crate::mount) fn to_mode_bits(kind: EntryKind, write_bits: WriteBits) -> u32 {
    match (kind, write_bits) {
        (EntryKind::Directory, WriteBits::Present) => 0o755,
        (EntryKind::File, WriteBits::Present) => 0o644,
        (EntryKind::Directory, WriteBits::Removed) => 0o555,
        (EntryKind::File, WriteBits::Removed) => 0o444,
    }
}

/// Takes the write bits off everything in the folder on disk, the folder first: nothing is
/// put into it while no mount lies over it. `folder` must be the folder itself, not a mount
/// over it.
pub(in crate::mount) fn remove_write_bits(folder: &Path) {
    set_modes(folder, WriteBits::Removed);
}

/// Gives the owner's write bits back to everything in the folder on disk, the folder first.
/// `folder` must be the folder itself, not a mount over it. Blocking: it walks the folder.
/// Outside `fuse` it is called for a folder that is forgotten while no mount lies over it
/// (`sync_tasks::stop_order::forget_folder_without_mount`).
pub(crate) fn restore_write_bits(folder: &Path) {
    set_modes(folder, WriteBits::Present);
}

/// Sets the modes of everything at and below `path`, with or without the write bits: of a
/// directory, of a regular file, and of nothing else. A mode is written only where it
/// differs.
fn set_modes(path: &Path, write_bits: WriteBits) {
    let Ok(metadata) = fs::symlink_metadata(path) else { return };
    let wanted = match (metadata.is_dir(), metadata.is_file()) {
        (true, _) => to_mode_bits(EntryKind::Directory, write_bits),
        (_, true) => to_mode_bits(EntryKind::File, write_bits),
        _ => return,
    };
    if metadata.permissions().mode() & 0o777 != wanted {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(wanted));
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path).into_iter().flatten().flatten() {
            set_modes(&entry.path(), write_bits);
        }
    }
}

impl BackingDirectory {
    /// With the mount over it the backing directory is the daemon's alone: its owner's write
    /// bits are given back, which were taken off when the daemon last ended.
    pub(in crate::mount) fn restore_write_bits(&self) {
        restore_write_bits(&self.resolve_root_directory_path());
    }
}

#[cfg(test)]
mod tests;
