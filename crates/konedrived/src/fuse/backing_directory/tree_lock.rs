//! The folder on disk while no mount lies over it: locked, so that nothing is put into it by
//! mistake; and what follows the daemon's end.

use std::fs;
use std::path::{Path, PathBuf};

/// Sets the modes of everything in the folder on disk, the folder first: `locked` takes the
/// write bits off, so that nothing is put into it while no mount lies over it; not locked
/// gives its owner the directories and the files back. A mode is written only where it differs. `folder`
/// must be the folder itself, not a mount over it.
pub(in crate::fuse) fn set_tree(folder: &Path, locked: bool) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = fs::symlink_metadata(folder) else { return };
    let wanted = match (meta.is_dir(), locked) {
        (true, true) => 0o555,
        (true, false) => 0o755,
        (false, true) if meta.is_file() => 0o444,
        (false, false) if meta.is_file() => 0o644,
        _ => return,
    };
    if meta.permissions().mode() & 0o777 != wanted {
        let _ = fs::set_permissions(folder, fs::Permissions::from_mode(wanted));
    }
    if meta.is_dir() {
        for entry in fs::read_dir(folder).into_iter().flatten().flatten() {
            set_tree(&entry.path(), locked);
        }
    }
}

/// What follows the daemon's end, however it ended (the unit's `ExecStopPost`, `konedrived
/// --tidy`): a mount the daemon left on `folder` is taken off, and the folder on disk is
/// locked. While the daemon runs the folder is under its mount and open to it alone; from here
/// on it is a plain directory that shows the downloaded files and takes nothing.
pub fn tidy(folder: &Path) {
    clear_stale(folder);
    // Not a folder that is still mounted: its daemon runs.
    let mounted = fs::read_to_string("/proc/self/mounts").unwrap_or_default().lines().any(|line| {
        let mut fields = line.split(' ');
        fields.nth(1).is_some_and(|at| Path::new(at) == folder) && fields.next().is_some_and(|kind| kind.starts_with("fuse"))
    });
    if !mounted {
        set_tree(folder, true);
    }
}

/// The directories of the folder on disk, the folder first: what a file manager is told to
/// read again once the mount over it is gone.
pub fn directories(folder: &Path) -> Vec<PathBuf> {
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

/// [`tidy`] for every folder `config_file` records as showing OneDrive: each is served by a
/// mount while the daemon runs. The folders.
pub fn tidy_all(config_file: &Path) -> Vec<PathBuf> {
    let mut tidied = Vec::new();
    let Some(config) = fs::read_to_string(config_file).ok().and_then(|text| text.parse::<toml::Table>().ok()) else { return tidied };
    let accounts = config.get("accounts").and_then(|accounts| accounts.as_array()).cloned().unwrap_or_default();
    for root in accounts.iter().filter_map(|account| account.get("root")) {
        let served = root.get("source").and_then(|source| source.as_str()) == Some("onedrive");
        if let Some(path) = root.get("path").and_then(|path| path.as_str()).filter(|_| served) {
            tidy(Path::new(path));
            tidied.push(PathBuf::from(path));
        }
    }
    tidied
}

/// Takes off a mount a daemon that died left on `folder`: every path in it then fails with
/// "not connected", and nothing can be mounted over it.
pub fn clear_stale(folder: &Path) {
    if let Err(e) = fs::metadata(folder) {
        if e.raw_os_error() == Some(libc::ENOTCONN) {
            tracing::warn!("{} carries the mount of a daemon that is gone; taking it off", folder.display());
            let _ = std::process::Command::new("fusermount3").arg("-u").arg("-z").arg(folder).status();
        }
    }
}

#[cfg(test)]
mod tests;
