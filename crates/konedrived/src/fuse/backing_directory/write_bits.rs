//! The write bits of the folder on disk: taken off while no mount lies over it, so that
//! nothing is put into it by mistake, and given back to its owner when a mount is put over
//! it, or when the folder is the user's own again.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::BackingDirectory;

/// Takes the write bits off everything in the folder on disk, the folder first: nothing is
/// put into it while no mount lies over it. `folder` must be the folder itself, not a mount
/// over it.
pub(in crate::fuse) fn remove_write_bits(folder: &Path) {
    set_modes(folder, 0o555, 0o444);
}

/// Gives the owner's write bits back to everything in the folder on disk, the folder first.
/// `folder` must be the folder itself, not a mount over it.
pub(in crate::fuse) fn restore_write_bits(folder: &Path) {
    set_modes(folder, 0o755, 0o644);
}

/// Sets the modes of everything at and below `path`: of a directory, of a regular file, and
/// of nothing else. A mode is written only where it differs.
fn set_modes(path: &Path, directory_mode: u32, file_mode: u32) {
    let Ok(metadata) = fs::symlink_metadata(path) else { return };
    let wanted = match (metadata.is_dir(), metadata.is_file()) {
        (true, _) => directory_mode,
        (_, true) => file_mode,
        _ => return,
    };
    if metadata.permissions().mode() & 0o777 != wanted {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(wanted));
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path).into_iter().flatten().flatten() {
            set_modes(&entry.path(), directory_mode, file_mode);
        }
    }
}

impl BackingDirectory {
    /// With the mount over it the backing directory is the daemon's alone: its owner's write
    /// bits are given back, which were taken off when the daemon last ended.
    pub(in crate::fuse) fn restore_write_bits(&self) {
        restore_write_bits(&self.resolve_path(Path::new("")));
    }
}

#[cfg(test)]
mod tests;
