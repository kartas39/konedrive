//! Telling KDE's programs to read directories again (`org.kde.KDirNotify`).
//!
//! A program that lists a directory follows it with a watch of its own, and that watch ends
//! when the mount under it goes: after a folder is mounted again — the daemon was started
//! again — an open file manager would show what it read from the mount
//! that is gone until the user reloads. `FilesAdded` is the signal every KIO program takes as
//! "something is new in this directory: list it again"; one that does not show the directory
//! ignores it.

use std::path::PathBuf;

use konedrive_tree::{Kind, Store, Table};

/// The most directories of one folder that are told: which of them a program shows is not
/// known here.
const MOST: usize = 20_000;

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

/// [`read_again`] for the folder at `folder` and every directory `store` names in it: after
/// its mount came, or the modes its mount shows changed.
pub async fn read_folder_again(store: Store, folder: PathBuf) {
    let listed = store
        .call(move |s| {
            let Some(root) = s.root_item_id()? else { return Ok(vec![folder]) };
            let (mut directories, mut next) = (vec![folder.clone()], vec![(root, folder)]);
            while let Some((id, path)) = next.pop() {
                for row in s.children(Table::OneDriveItems, &id)? {
                    if row.kind == Kind::Folder && directories.len() < MOST {
                        let below = path.join(&row.name);
                        directories.push(below.clone());
                        next.push((row.id, below));
                    }
                }
            }
            Ok(directories)
        })
        .await
        .unwrap_or_default();
    read_again(listed).await;
}
