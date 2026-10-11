//! A stale mount: the mount a daemon that died left on a folder. Every path in the folder
//! then fails with "not connected", and nothing can be mounted over it.

use std::fs;
use std::path::Path;

/// Takes off the stale mount on `folder`, when there is one.
pub fn unmount_stale_mount(folder: &Path) {
    if let Err(e) = fs::metadata(folder) {
        if e.raw_os_error() == Some(libc::ENOTCONN) {
            tracing::warn!("{} carries the mount of a daemon that is gone; taking it off", folder.display());
            let _ = std::process::Command::new("fusermount3").arg("-u").arg("-z").arg(folder).status();
        }
    }
}
