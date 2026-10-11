//! The tidy step: what follows the daemon's end, however it ended (the unit's
//! `ExecStopPost`, `konedrived --tidy`). While the daemon runs a folder is under its mount
//! and open to it alone; from here on it is a plain directory that shows the downloaded
//! files and takes nothing.

use std::fs;
use std::path::{Path, PathBuf};

use super::backing_directory::remove_write_bits;
use super::stale_mount::unmount_stale_mount;

/// Whether `mounts`, the kernel's table of mounts (`/proc/self/mounts`), has a FUSE mount
/// over `folder`.
fn lists_fuse_mount_at(mounts: &str, folder: &Path) -> bool {
    mounts.lines().any(|line| {
        let mut fields = line.split(' ');
        fields.nth(1).is_some_and(|at| Path::new(at) == folder) && fields.next().is_some_and(|kind| kind.starts_with("fuse"))
    })
}

/// Whether a FUSE mount lies over `folder` now.
fn is_mounted(folder: &Path) -> bool {
    lists_fuse_mount_at(&fs::read_to_string("/proc/self/mounts").unwrap_or_default(), folder)
}

/// Tidies `folder`: a stale mount is taken off it, and the write bits of the folder on disk
/// are removed.
pub fn tidy_folder(folder: &Path) {
    unmount_stale_mount(folder);
    // Not a folder that is still mounted: its daemon runs.
    if !is_mounted(folder) {
        remove_write_bits(folder);
    }
}

/// The folders `config_file`, the daemon's configuration, records as showing OneDrive: each
/// is served by a mount while the daemon runs.
fn read_served_folders(config_file: &Path) -> Vec<PathBuf> {
    let Some(config) = fs::read_to_string(config_file).ok().and_then(|text| text.parse::<toml::Table>().ok()) else { return Vec::new() };
    let accounts = config.get("accounts").and_then(|accounts| accounts.as_array()).cloned().unwrap_or_default();
    let roots = accounts.iter().filter_map(|account| account.get("root"));
    let served = roots.filter(|root| root.get("source").and_then(|source| source.as_str()) == Some("onedrive"));
    served.filter_map(|root| root.get("path").and_then(|path| path.as_str())).map(PathBuf::from).collect()
}

/// [`tidy_folder`] for every folder `config_file` records as showing OneDrive. The folders.
pub fn tidy_served_folders(config_file: &Path) -> Vec<PathBuf> {
    let folders = read_served_folders(config_file);
    folders.iter().for_each(|folder| tidy_folder(folder));
    folders
}

/// The directories of the folder on disk, the folder first: what a file manager is told to
/// read again once the mount over it is gone.
pub fn list_directories(folder: &Path) -> Vec<PathBuf> {
    let mut found = vec![folder.to_owned()];
    let mut at = 0;
    while at < found.len() {
        let below = fs::read_dir(&found[at]).into_iter().flatten().flatten().filter(|e| e.file_type().is_ok_and(|kind| kind.is_dir()));
        let below: Vec<PathBuf> = below.map(|e| e.path()).collect();
        found.extend(below);
        at += 1;
    }
    found
}

#[cfg(test)]
mod tests;
