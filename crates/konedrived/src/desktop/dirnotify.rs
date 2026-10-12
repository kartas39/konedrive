//! Telling KDE's programs to read directories again (`org.kde.KDirNotify`).
//!
//! A program that lists a directory follows it with a watch of its own, and that watch ends
//! when the mount under it goes: after a folder is mounted again — the daemon was started
//! again — an open file manager would show what it read from the mount
//! that is gone until the user reloads. `FilesAdded` is the signal every KIO program takes as
//! "something is new in this directory: list it again"; one that does not show the directory
//! ignores it.

use std::path::PathBuf;

use konedrive_tree::Store;

/// The most directories of one folder that are told: which of them a program shows is not
/// known here.
const MOST: usize = 20_000;

/// How many directories one read of the store answers.
const DIRECTORIES_READ_AT_ONCE: usize = 1000;

/// Asks whoever lists one of `directories` to list it again, on `connection`: the session bus
/// of whoever calls — the daemon's own connection, which the folder's service was made with,
/// or the one `main` opens for the tidy step. This opens none itself. Best effort: a signal
/// that cannot be sent ends it.
pub async fn read_again(connection: &zbus::Connection, directories: Vec<PathBuf>) {
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

/// [`read_again`] for the folder at `folder` and every directory `store` names in it: after
/// its mount came, or the modes its mount shows changed.
pub async fn read_folder_again(connection: zbus::Connection, store: Store, folder: PathBuf) {
    let mut directories = Vec::new();
    let mut after = None;
    while directories.len() < MOST {
        let page = match store.read_items(move |reader| reader.list_shown_directories(after, DIRECTORIES_READ_AT_ONCE)).await {
            Ok(page) => page,
            Err(error) => {
                tracing::debug!("cannot read the directories of {} to tell KDE of: {error}", folder.display());
                break;
            }
        };
        let Some((last, _)) = page.last() else { break };
        after = Some(*last);
        directories.extend(page.into_iter().map(|(_, path)| folder.join(path)));
    }
    directories.truncate(MOST);
    read_again(&connection, directories).await;
}
