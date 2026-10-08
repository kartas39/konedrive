//! Telling KDE's programs to read directories again (`org.kde.KDirNotify`).
//!
//! A program that lists a directory follows it with a watch of its own, and that watch ends
//! when the mount under it goes: after a folder served through FUSE is mounted again — the
//! daemon was started again — an open file manager would show what it read from the mount
//! that is gone until the user reloads. `FilesAdded` is the signal every KIO program takes as
//! "something is new in this directory: list it again"; one that does not show the directory
//! ignores it.

use std::path::PathBuf;

/// Asks whoever lists one of `directories` to list it again. Best effort: with no session bus
/// nothing is said.
pub async fn read_again(directories: Vec<PathBuf>) {
    let Ok(connection) = zbus::Connection::session().await else { return };
    for directory in directories {
        let Ok(url) = url::Url::from_directory_path(&directory) else { continue };
        // As KIO writes a directory: no slash at its end.
        let url = url.as_str().trim_end_matches('/').to_owned();
        if let Err(e) = connection.emit_signal(None::<()>, "/KDirNotify", "org.kde.KDirNotify", "FilesAdded", &(url,)).await {
            tracing::debug!("cannot tell KDE to read {} again: {e}", directory.display());
            return;
        }
    }
}
