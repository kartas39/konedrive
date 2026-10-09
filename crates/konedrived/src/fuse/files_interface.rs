//! What the daemon's bus interface `Files` asks of a mount for a path: download it, free it
//! up, pin it, and say how it stands.
//!
//! [`FilesInterface`] holds the parts these need and no state of its own: what each
//! directory holds ([`DirectoryContents`]), the [`BackingDirectory`], where a copy that is
//! freed is removed, the state of the downloads ([`DownloadStates`]) and what downloads
//! ([`FileDownloader`], and the [`PinnedFileDownloader`] for what a pin keeps), what
//! programs hold open ([`OpenHandles`]), the [`Pins`], and the [`InotifyBridge`], which
//! announces what changed of a file to whoever shows it. A path it is asked about is a path
//! of the mount; what is local only is not OneDrive's to download, free or pin, and is
//! refused ([`FilesRefusal::NotInOneDrive`]).
//!
//! Whether a downloaded copy is freed is decided in one place
//! ([`super::free_up_decision`]): for each copy below a path here, and for a path the
//! folder's sync asks about, through its [`PathStatus`].

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_graph::pool::Class;
use konedrive_tree::Row;
use tokio::sync::oneshot;

use super::backing_directory::BackingDirectory;
use super::directory_contents::{DirectoryContents, DirectoryEntry, EntryState, FileSyncState};
use super::download_states::DownloadStates;
use super::file_downloader::FileDownloader;
use super::free_up_decision::{decide_free_up, FreeUpDecision};
use super::inotify_bridge::{Announcement, InotifyBridge};
use super::open_handles::OpenHandles;
use super::pinned_file_downloader::PinnedFileDownloader;
use super::pins::Pins;
use crate::status::report::Report;

mod path_status;

pub use path_status::PathStatus;

/// What a free-up did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FreedSpace {
    pub files: u64,
    pub bytes: u64,
    /// Downloaded files left as they are: pinned, being downloaded, or changed here and not
    /// uploaded yet.
    pub kept: u64,
}

/// The downloaded copies at and below a path: how many, and their bytes on disk.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DownloadedCopies {
    pub files: u64,
    pub bytes: u64,
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

pub struct FilesInterface {
    /// The folder's path, which is the mount.
    folder: PathBuf,
    directory_contents: Arc<DirectoryContents>,
    /// Where a downloaded copy that is freed is removed.
    backing_directory: Arc<BackingDirectory>,
    download_states: Arc<DownloadStates>,
    file_downloader: Arc<FileDownloader>,
    open_handles: Arc<OpenHandles>,
    pins: Arc<Pins>,
    inotify_bridge: Arc<InotifyBridge>,
    pinned_file_downloader: Arc<PinnedFileDownloader>,
    report: Report,
}

impl FilesInterface {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        folder: PathBuf,
        directory_contents: Arc<DirectoryContents>,
        backing_directory: Arc<BackingDirectory>,
        download_states: Arc<DownloadStates>,
        file_downloader: Arc<FileDownloader>,
        open_handles: Arc<OpenHandles>,
        pins: Arc<Pins>,
        inotify_bridge: Arc<InotifyBridge>,
        pinned_file_downloader: Arc<PinnedFileDownloader>,
        report: Report,
    ) -> Self {
        FilesInterface { folder, directory_contents, backing_directory, download_states, file_downloader, open_handles, pins, inotify_bridge, pinned_file_downloader, report }
    }

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
    pub async fn download(&self, path: &Path) -> Result<(), FilesRefusal> {
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
            Ok(Err(errno)) => Err(FilesRefusal::Io(format!("the download failed: {}", io::Error::from_raw_os_error(errno)))),
            Err(_) => Err(FilesRefusal::Io("the download was given up".into())),
        }
    }

    /// Frees the space of the downloaded copies at and below `path`. What a pin keeps, a
    /// download is filling, a program writes, or holds a change that is not uploaded yet, is
    /// left and counted. Whether `path` itself may be freed is the caller's to ask first
    /// ([`find_path_status`](Self::find_path_status)).
    pub async fn free_up(&self, path: &Path) -> Result<FreedSpace, FilesRefusal> {
        let (row, _) = self.find_onedrive_row(path)?;
        Ok(self.free_up_below(row).await)
    }

    /// Frees the space of every downloaded copy that no pin keeps.
    pub async fn free_up_all(&self) -> FreedSpace {
        let Some(root) = self.directory_contents.find_root_id().and_then(|id| self.directory_contents.find_row(&id)) else { return FreedSpace::default() };
        self.free_up_below(root).await
    }

    async fn free_up_below(&self, row: Row) -> FreedSpace {
        let (directory_contents, open_handles, pins) = (Arc::clone(&self.directory_contents), Arc::clone(&self.open_handles), Arc::clone(&self.pins));
        let backing_directory = Arc::clone(&self.backing_directory);
        let (download_states, inotify_bridge) = (Arc::clone(&self.download_states), Arc::clone(&self.inotify_bridge));
        let freed = tokio::task::spawn_blocking(move || {
            let (mut freed, mut announcements) = (FreedSpace::default(), Vec::new());
            for (file, at) in directory_contents.list_files_below(&row) {
                // What is not downloaded takes no space: only a downloaded copy is asked
                // about.
                let Some(file_row) = file.row().filter(|_| file.downloaded_copy().is_some()).cloned() else { continue };
                let (is_pinned, is_downloading) = (pins.find_pinning_row(&file_row).is_some(), download_states.is_downloading(&file_row.id));
                let is_open_for_writing = open_handles.is_open_for_writing_by_onedrive_id(&file_row.id);
                let freed_bytes = match decide_free_up(&EntryState::File(file), is_pinned, is_downloading, is_open_for_writing) {
                    FreeUpDecision::Free => backing_directory.remove_downloaded_copy(&at, &file_row.id),
                    FreeUpDecision::LeavePinned | FreeUpDecision::LeaveModifiedHere | FreeUpDecision::LeaveOpenForWriting | FreeUpDecision::LeaveDownloading => None,
                    FreeUpDecision::NotDownloaded => continue,
                };
                match freed_bytes {
                    Some(bytes) => {
                        freed.files += 1;
                        freed.bytes += bytes;
                        announcements.push(Announcement::FileShownChanged { rel: at, id: file_row.id });
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

    /// How the file or directory of OneDrive at `path` stands: its pin — its own, and the
    /// directory above that pins it — and what is shown of it and decided for it.
    pub fn find_path_status(&self, path: &Path) -> Result<PathStatus, FilesRefusal> {
        let (entry, _) = self.find_onedrive_entry(path)?;
        let Some(row) = entry.row().cloned() else { return Err(FilesRefusal::NotInOneDrive) };
        let own_pin = self.pins.is_pinned(&row.id);
        let above = self.directory_contents.find_parent_row(&row).and_then(|above| self.pins.find_pinning_row(&above));
        let pinned_above = above.and_then(|by| self.directory_contents.find_stored_path(&by.id)).map(|at| self.folder.join(at));
        let is_downloading = self.download_states.is_downloading(&row.id);
        let is_open_for_writing = self.open_handles.is_open_for_writing_by_onedrive_id(&row.id);
        Ok(PathStatus { id: row.id, own_pin, pinned_above, entry_state: entry.state, is_downloading, is_open_for_writing })
    }

    /// Puts the pin on what is at `path`, or takes it off. A pin put on downloads what it
    /// keeps.
    pub async fn set_pin(&self, path: &Path, pinned: bool) -> Result<(), FilesRefusal> {
        let (row, rel) = self.find_onedrive_row(path)?;
        let changed = self.pins.set_pin(&row.id, pinned).await.map_err(|e| FilesRefusal::Io(e.to_string()))?;
        if changed {
            self.inotify_bridge.announce(vec![Announcement::FileShownChanged { rel, id: row.id }]);
        }
        if pinned {
            self.pinned_file_downloader.download_pinned_files();
        }
        Ok(())
    }

    /// The downloads that failed and can be continued, oldest failure first.
    #[cfg(any(test, feature = "fault-injection"))]
    pub fn list_unfinished_downloads(&self) -> Vec<super::file_downloader::UnfinishedDownload> {
        self.file_downloader.list_unfinished_downloads()
    }

    /// How many files and directories carry a pin of their own.
    pub fn count_pins(&self) -> u64 {
        self.pins.count()
    }

    /// The downloaded copies at and below `path`.
    pub fn count_downloaded_below(&self, path: &Path) -> Result<DownloadedCopies, FilesRefusal> {
        let (row, _) = self.find_onedrive_row(path)?;
        let copies = self.directory_contents.list_files_below(&row).into_iter().filter_map(|(file, _)| file.downloaded_copy().copied());
        Ok(copies.fold(DownloadedCopies::default(), |counted, copy| DownloadedCopies { files: counted.files + 1, bytes: counted.bytes + copy.blocks * 512 }))
    }
}

#[cfg(test)]
mod tests;
