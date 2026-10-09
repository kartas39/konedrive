//! What the daemon asks of a mount for a path: download it, free it up, pin it, look at it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_fs::placeholder::State;
use konedrive_graph::pool::Class;
use konedrive_tree::{Kind, Row, Table};
use tokio::sync::oneshot;

use super::inotify_bridge::RemoteChange;
use super::directory_contents::{DirectoryEntry, EntryState, FileSyncState};
use super::download_states::to_hydration_state;
use super::mount::Mount;
use super::pins::download_pinned_files;

/// What a free-up did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Freed {
    pub files: u64,
    pub bytes: u64,
    /// Downloaded files left as they are: pinned, being downloaded, or changed here and not
    /// uploaded yet.
    pub kept: u64,
}

/// Why the mount refuses what it was asked.
#[derive(Debug, thiserror::Error)]
pub enum Refused {
    #[error("not inside this folder")]
    NotOurs,
    /// A file or directory OneDrive has no item for yet, or a name the folder does not have.
    #[error("not an item of OneDrive")]
    NoItem,
    #[error("{0}")]
    Io(String),
}

/// What [`Mount::look`] saw of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Looked {
    pub id: String,
    pub folder: bool,
    /// `None` for a folder.
    pub state: Option<State>,
    pub own_pin: bool,
    /// The folder above that pins it.
    pub pinned_above: Option<PathBuf>,
    /// A file whose content was changed here and is not uploaded yet: its copy is the only
    /// one there is.
    pub changed: bool,
}

impl Mount {
    pub(super) fn rel(&self, path: &Path) -> Result<PathBuf, Refused> {
        path.strip_prefix(&self.shared.root.path).map(Path::to_owned).map_err(|_| Refused::NotOurs)
    }

    /// The entry at `path`, when it is in OneDrive: what is local only is not OneDrive's to
    /// download, free or pin.
    fn onedrive_entry(&self, path: &Path) -> Result<(DirectoryEntry, PathBuf), Refused> {
        let rel = self.rel(path)?;
        let entry = self.shared.directory_contents.find_entry_at(&rel).filter(|entry| entry.row().is_some());
        Ok((entry.ok_or(Refused::NoItem)?, rel))
    }

    pub(super) fn item(&self, path: &Path) -> Result<(Row, PathBuf), Refused> {
        let (entry, rel) = self.onedrive_entry(path)?;
        Ok((entry.row().cloned().ok_or(Refused::NoItem)?, rel))
    }

    /// Downloads the file at `path` now, and waits for it.
    pub async fn hydrate(&self, path: &Path) -> Result<(), Refused> {
        let (entry, rel) = self.onedrive_entry(path)?;
        let row = match entry.state {
            EntryState::File(FileSyncState::RemoteOnly(row)) => row,
            // Downloaded already.
            EntryState::File(_) => return Ok(()),
            EntryState::Directory(_) => return Err(Refused::NoItem),
        };
        let (tell, told) = oneshot::channel();
        self.shared.file_downloader.download(row, rel, Class::Open, Some(tell));
        match told.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(errno)) => Err(Refused::Io(format!("hydration failed: {}", io::Error::from_raw_os_error(errno)))),
            Err(_) => Err(Refused::Io("the download was given up".into())),
        }
    }

    /// Frees the space of the downloaded files at and below `path`. What a pin keeps, a
    /// download is filling, or holds a change that is not uploaded yet, is left and counted. Whether `path` itself may be freed is the
    /// caller's to ask first ([`look`](Self::look)).
    pub async fn free_up(&self, path: &Path) -> Result<Freed, Refused> {
        let (row, _) = self.item(path)?;
        Ok(self.free_below(row).await)
    }

    /// Frees the space of every downloaded file that no pin keeps.
    pub async fn free_up_all(&self) -> Freed {
        let Some(root) = self.shared.directory_contents.root_id().and_then(|id| self.shared.directory_contents.row(&id)) else { return Freed::default() };
        self.free_below(root).await
    }

    pub(super) async fn free_below(&self, row: Row) -> Freed {
        let s = Arc::clone(&self.shared);
        let freed = tokio::task::spawn_blocking(move || {
            let (mut freed, mut told) = (Freed::default(), Vec::new());
            for (file, at) in s.directory_contents.files_below(&row) {
                let file = match file {
                    // What is not downloaded takes no space.
                    FileSyncState::RemoteOnly(_) | FileSyncState::LocalOnly(_) => continue,
                    // Content of the user's own that is not in OneDrive yet stays.
                    FileSyncState::ModifiedHere(..) => {
                        freed.kept += 1;
                        continue;
                    }
                    FileSyncState::Synced(file, _) => file,
                };
                // What a pin keeps, a download is filling, or a program holds open for
                // writing, stays. The sync state says the last too while the outbox holds
                // the file's row; the open files say it also once that row is dropped under
                // a writer, by a forced switch to read-only.
                if s.open_files.is_open_for_writing(&file.id) || s.pins.pinned_by(&file).is_some() || s.download_states.is_downloading(&file.id) {
                    freed.kept += 1;
                    continue;
                }
                match s.backing_directory.remove_downloaded_copy(&at, &file.id) {
                    Some(bytes) => {
                        freed.files += 1;
                        freed.bytes += bytes;
                        told.push(RemoteChange::ContentChanged { rel: at, id: file.id });
                    }
                    None => freed.kept += 1,
                }
            }
            s.inotify_bridge.announce(told);
            freed
        })
        .await
        .unwrap_or_default();
        self.shared.report.space.kick();
        freed
    }

    /// The state of the item at `path`, its own pin, and the folder above that pins it.
    pub fn look(&self, path: &Path) -> Result<Looked, Refused> {
        let (entry, _) = self.onedrive_entry(path)?;
        let Some(row) = entry.row().cloned() else { return Err(Refused::NoItem) };
        let s = &self.shared;
        let own_pin = s.pins.is_pinned(&row.id);
        let above = s.directory_contents.parent(&row).and_then(|above| s.pins.pinned_by(&above));
        let pinned_above = above.and_then(|by| s.directory_contents.read(|store| store.locate(Table::Items, &by.id)).flatten()).map(|at| s.root.path.join(at.rel));
        let (state, modified_here) = match &entry.state {
            EntryState::File(file) => (to_hydration_state(file, s.download_states.is_downloading(&row.id)), matches!(file, FileSyncState::ModifiedHere(..))),
            EntryState::Directory(_) => (None, false),
        };
        let changed = modified_here || s.open_files.is_open_for_writing(&row.id);
        Ok(Looked { id: row.id, folder: row.kind == Kind::Folder, state, own_pin, pinned_above, changed })
    }

    /// Puts the pin on the item at `path`, or takes it off. A pin put on downloads what it
    /// keeps.
    pub async fn set_pin(&self, path: &Path, pinned: bool) -> Result<(), Refused> {
        let (row, rel) = self.item(path)?;
        let s = &self.shared;
        let changed = s.pins.set_pin(&row.id, pinned).await.map_err(|e| Refused::Io(e.to_string()))?;
        if changed {
            s.inotify_bridge.announce(vec![RemoteChange::ContentChanged { rel, id: row.id }]);
        }
        if pinned {
            download_pinned_files(&s.pins, &s.directory_contents, &s.file_downloader, &s.runtime);
        }
        Ok(())
    }

    /// How many items carry a pin of their own.
    pub fn pinned_count(&self) -> u64 {
        self.shared.pins.count()
    }

    /// How many downloaded files, and bytes, the items at and below `path` hold.
    pub fn downloaded_under(&self, path: &Path) -> Result<(u64, u64), Refused> {
        let (row, _) = self.item(path)?;
        let s = &self.shared;
        let copies = s.directory_contents.files_below(&row).into_iter().filter_map(|(file, _)| file.downloaded_copy().copied());
        Ok(copies.fold((0, 0), |(files, bytes), copy| (files + 1, bytes + copy.blocks * 512)))
    }
}
