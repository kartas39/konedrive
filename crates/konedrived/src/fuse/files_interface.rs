//! What the daemon's bus interface `Files` asks of a mount for a path: download it, free it
//! up, pin it, and say how it stands.
//!
//! [`FilesInterface`] holds the parts these need and no state of its own: what each
//! directory holds ([`DirectoryContents`], and through it the backing directory), the state
//! of the downloads ([`DownloadStates`]) and what downloads ([`FileDownloader`]), what
//! programs hold open ([`OpenFiles`]), the [`Pins`], and the [`InotifyBridge`], which
//! announces what changed of a file to whoever shows it. A path it is asked about is a path
//! of the mount; what is local only is not OneDrive's to download, free or pin, and is
//! refused ([`FilesRefusal::NotInOneDrive`]).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_fs::placeholder::State;
use konedrive_graph::pool::Class;
use konedrive_tree::{Kind, Row, Table};
use tokio::sync::oneshot;

use super::directory_contents::{DirectoryContents, DirectoryEntry, EntryState, FileSyncState};
use super::download_states::{to_hydration_state, DownloadStates};
use super::file_downloader::FileDownloader;
use super::inotify_bridge::{InotifyBridge, Announcement};
use super::open_files::OpenFiles;
use super::pins::{download_pinned_files, Pins};
use crate::status::report::Report;

/// What a free-up did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FreedSpace {
    pub files: u64,
    pub bytes: u64,
    /// Downloaded files left as they are: pinned, being downloaded, or changed here and not
    /// uploaded yet.
    pub kept: u64,
}

/// Why the mount refuses what the `Files` interface asked.
#[derive(Debug, thiserror::Error)]
pub enum FilesRefusal {
    #[error("not inside this folder")]
    OutsideFolder,
    /// A file or directory that is not in OneDrive yet, or a name the folder does not have.
    #[error("not in OneDrive")]
    NotInOneDrive,
    #[error("{0}")]
    Io(String),
}

/// How the file or directory of OneDrive at a path stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStatus {
    /// Its OneDrive id.
    pub id: String,
    pub is_directory: bool,
    /// `None` for a directory.
    pub state: Option<State>,
    pub own_pin: bool,
    /// The directory above that pins it.
    pub pinned_above: Option<PathBuf>,
    /// A file whose content was changed here and is not uploaded yet: its copy is the only
    /// one there is.
    pub modified_here: bool,
}

pub(super) struct FilesInterface {
    /// The folder's path, which is the mount.
    pub(super) folder: PathBuf,
    pub(super) directory_contents: Arc<DirectoryContents>,
    pub(super) download_states: Arc<DownloadStates>,
    pub(super) file_downloader: Arc<FileDownloader>,
    pub(super) open_files: Arc<OpenFiles>,
    pub(super) pins: Arc<Pins>,
    pub(super) inotify_bridge: Arc<InotifyBridge>,
    pub(super) report: Report,
    pub(super) runtime: tokio::runtime::Handle,
}

impl FilesInterface {
    /// The entry at `path`, when it is in OneDrive, and its path in the folder.
    fn find_onedrive_entry(&self, path: &Path) -> Result<(DirectoryEntry, PathBuf), FilesRefusal> {
        let rel = path.strip_prefix(&self.folder).map(Path::to_owned).map_err(|_| FilesRefusal::OutsideFolder)?;
        let entry = self.directory_contents.find_entry_at(&rel).filter(|entry| entry.row().is_some());
        Ok((entry.ok_or(FilesRefusal::NotInOneDrive)?, rel))
    }

    /// The store's row of what is at `path`, when it is in OneDrive, and its path in the
    /// folder.
    fn find_onedrive_row(&self, path: &Path) -> Result<(Row, PathBuf), FilesRefusal> {
        let (entry, rel) = self.find_onedrive_entry(path)?;
        Ok((entry.row().cloned().ok_or(FilesRefusal::NotInOneDrive)?, rel))
    }

    /// Downloads the file at `path` now, and waits for it.
    pub(super) async fn hydrate(&self, path: &Path) -> Result<(), FilesRefusal> {
        let (entry, rel) = self.find_onedrive_entry(path)?;
        let row = match entry.state {
            EntryState::File(FileSyncState::RemoteOnly(row)) => row,
            // Downloaded already.
            EntryState::File(_) => return Ok(()),
            EntryState::Directory(_) => return Err(FilesRefusal::NotInOneDrive),
        };
        let (waiter, downloaded) = oneshot::channel();
        self.file_downloader.download(row, rel, Class::Open, Some(waiter));
        match downloaded.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(errno)) => Err(FilesRefusal::Io(format!("hydration failed: {}", io::Error::from_raw_os_error(errno)))),
            Err(_) => Err(FilesRefusal::Io("the download was given up".into())),
        }
    }

    /// Frees the space of the downloaded copies at and below `path`. What a pin keeps, a
    /// download is filling, or holds a change that is not uploaded yet, is left and counted.
    /// Whether `path` itself may be freed is the caller's to ask first
    /// ([`find_path_status`](Self::find_path_status)).
    pub(super) async fn free_up(&self, path: &Path) -> Result<FreedSpace, FilesRefusal> {
        let (row, _) = self.find_onedrive_row(path)?;
        Ok(self.free_up_below(row).await)
    }

    /// Frees the space of every downloaded copy that no pin keeps.
    pub(super) async fn free_up_all(&self) -> FreedSpace {
        let Some(root) = self.directory_contents.find_root_id().and_then(|id| self.directory_contents.find_row(&id)) else { return FreedSpace::default() };
        self.free_up_below(root).await
    }

    async fn free_up_below(&self, row: Row) -> FreedSpace {
        let (directory_contents, open_files, pins) = (Arc::clone(&self.directory_contents), Arc::clone(&self.open_files), Arc::clone(&self.pins));
        let (download_states, inotify_bridge) = (Arc::clone(&self.download_states), Arc::clone(&self.inotify_bridge));
        let freed = tokio::task::spawn_blocking(move || {
            let (mut freed, mut announcements) = (FreedSpace::default(), Vec::new());
            for (file, at) in directory_contents.list_files_below(&row) {
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
                if open_files.is_open_for_writing(&file.id) || pins.find_pinning_row(&file).is_some() || download_states.is_downloading(&file.id) {
                    freed.kept += 1;
                    continue;
                }
                match directory_contents.backing_directory().remove_downloaded_copy(&at, &file.id) {
                    Some(bytes) => {
                        freed.files += 1;
                        freed.bytes += bytes;
                        announcements.push(Announcement::FileShownChanged { rel: at, id: file.id });
                    }
                    None => freed.kept += 1,
                }
            }
            inotify_bridge.announce(announcements);
            freed
        })
        .await
        .unwrap_or_default();
        self.report.space.kick();
        freed
    }

    /// How the file or directory of OneDrive at `path` stands: its state, its own pin, and
    /// the directory above that pins it.
    pub(super) fn find_path_status(&self, path: &Path) -> Result<PathStatus, FilesRefusal> {
        let (entry, _) = self.find_onedrive_entry(path)?;
        let Some(row) = entry.row().cloned() else { return Err(FilesRefusal::NotInOneDrive) };
        let own_pin = self.pins.is_pinned(&row.id);
        let above = self.directory_contents.find_parent_row(&row).and_then(|above| self.pins.find_pinning_row(&above));
        let pinned_above = above.and_then(|by| self.directory_contents.read(|store| store.locate(Table::Items, &by.id)).flatten()).map(|at| self.folder.join(at.rel));
        let (state, modified_here) = match &entry.state {
            EntryState::File(file) => (to_hydration_state(file, self.download_states.is_downloading(&row.id)), matches!(file, FileSyncState::ModifiedHere(..))),
            EntryState::Directory(_) => (None, false),
        };
        let modified_here = modified_here || self.open_files.is_open_for_writing(&row.id);
        Ok(PathStatus { id: row.id, is_directory: row.kind == Kind::Folder, state, own_pin, pinned_above, modified_here })
    }

    /// Puts the pin on what is at `path`, or takes it off. A pin put on downloads what it
    /// keeps.
    pub(super) async fn set_pin(&self, path: &Path, pinned: bool) -> Result<(), FilesRefusal> {
        let (row, rel) = self.find_onedrive_row(path)?;
        let changed = self.pins.set_pin(&row.id, pinned).await.map_err(|e| FilesRefusal::Io(e.to_string()))?;
        if changed {
            self.inotify_bridge.announce(vec![Announcement::FileShownChanged { rel, id: row.id }]);
        }
        if pinned {
            download_pinned_files(&self.pins, &self.directory_contents, &self.file_downloader, &self.runtime);
        }
        Ok(())
    }

    /// How many files and directories carry a pin of their own.
    pub(super) fn count_pins(&self) -> u64 {
        self.pins.count()
    }

    /// How many downloaded copies, and bytes on disk, there are at and below `path`.
    pub(super) fn count_downloaded_below(&self, path: &Path) -> Result<(u64, u64), FilesRefusal> {
        let (row, _) = self.find_onedrive_row(path)?;
        let copies = self.directory_contents.list_files_below(&row).into_iter().filter_map(|(file, _)| file.downloaded_copy().copied());
        Ok(copies.fold((0, 0), |(files, bytes), copy| (files + 1, bytes + copy.blocks * 512)))
    }
}
