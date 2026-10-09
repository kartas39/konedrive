//! What the daemon's bus interface `Files` asks of a mount for a path: download it, free it
//! up, pin it, and say how it stands.
//!
//! [`FilesInterface`] holds the parts these need and no state of its own: what each
//! directory holds ([`DirectoryContents`]), the [`BackingDirectory`], where a copy that is
//! freed is removed, the state of the downloads ([`DownloadStates`]) and what downloads
//! ([`FileDownloader`], and the [`PinnedFileDownloader`] for what a pin keeps), what
//! programs hold open ([`OpenHandles`]) and which node stands for a OneDrive id
//! ([`NodeResolver`]), the [`Pins`], the [`InotifyBridge`], which
//! announces what changed of a file to whoever shows it, and the [`WriteOperationThread`],
//! on which a copy is freed. A path it is asked about is a path of the mount; what is local
//! only is not OneDrive's to download, free or pin, and is refused
//! ([`FilesRefusal::NotInOneDrive`]).
//!
//! Whether a downloaded copy is freed is decided in one place
//! ([`super::free_up_decision`]): for each copy below a path here, and for a path the
//! folder's sync asks about, through its [`PathStatus`].
//!
//! A free-up walks off the write-operation thread and frees on it. The walk lists the
//! OneDrive ids of the downloaded copies below a path, and decides nothing. Each copy is
//! then freed in a job of its own on the thread, waited for before the next is put there:
//! the job reads how the file stands now, decides, and removes the copy, and no write
//! operation — an open for writing, a close, a truncate, a rename — runs between what it read
//! and what it did. One job a file, so that a program's write waits for one file of a long
//! free-up and not for all of them. What a [`PathStatus`] says of a path was read off the
//! thread, and is what the folder's sync refuses by before anything is freed; the job
//! decides again.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_graph::pool::Class;
use konedrive_tree::Row;
use tokio::sync::oneshot;

use super::backing_directory::BackingDirectory;
use super::directory_contents::{DirectoryContents, DirectoryEntry, EntryState, FileSyncState};
use super::download_states::DownloadStates;
use super::file_downloader::{FileDownloader, FileToDownload};
use super::free_up_decision::{decide_free_up, FreeUpDecision};
use super::inotify_bridge::{Announcement, InotifyBridge};
use super::node_resolver::NodeResolver;
use super::open_handles::OpenHandles;
use super::pinned_file_downloader::PinnedFileDownloader;
use super::pins::Pins;
use super::write_operation_thread::WriteOperationThread;
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

/// The space the downloaded copies at and below a path take: how many files, and their
/// bytes on disk.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DownloadedSpace {
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
    /// Which node stands for a OneDrive id: the open handles say whether that node is open
    /// for writing.
    node_resolver: Arc<NodeResolver>,
    /// Where a downloaded copy that is freed is removed.
    backing_directory: Arc<BackingDirectory>,
    download_states: Arc<DownloadStates>,
    file_downloader: Arc<FileDownloader>,
    open_handles: Arc<OpenHandles>,
    pins: Arc<Pins>,
    inotify_bridge: Arc<InotifyBridge>,
    pinned_file_downloader: Arc<PinnedFileDownloader>,
    /// Where a downloaded copy is freed: beside no write operation.
    write_operation_thread: Arc<WriteOperationThread>,
    report: Report,
}

impl FilesInterface {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        folder: PathBuf,
        directory_contents: Arc<DirectoryContents>,
        node_resolver: Arc<NodeResolver>,
        backing_directory: Arc<BackingDirectory>,
        download_states: Arc<DownloadStates>,
        file_downloader: Arc<FileDownloader>,
        open_handles: Arc<OpenHandles>,
        pins: Arc<Pins>,
        inotify_bridge: Arc<InotifyBridge>,
        pinned_file_downloader: Arc<PinnedFileDownloader>,
        write_operation_thread: Arc<WriteOperationThread>,
        report: Report,
    ) -> Self {
        FilesInterface { folder, directory_contents, node_resolver, backing_directory, download_states, file_downloader, open_handles, pins, inotify_bridge, pinned_file_downloader, write_operation_thread, report }
    }

    /// Whether a program holds the OneDrive file `onedrive_id` open for writing now: the
    /// node that stands for it, by the node table, has a writer.
    fn is_open_for_writing(&self, onedrive_id: &str) -> bool {
        self.node_resolver.find_node_id(onedrive_id).is_some_and(|node_id| self.open_handles.is_open_for_writing_by_node_id(node_id))
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
        self.file_downloader.download(FileToDownload { row, path: rel }, Class::Open, Some(waiter));
        match downloaded.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(errno)) => Err(FilesRefusal::Io(format!("the download failed: {}", io::Error::from_raw_os_error(errno)))),
            Err(_) => Err(FilesRefusal::Io("the download was given up".into())),
        }
    }

    /// Frees the space of the downloaded copies at and below `path`. What a pin keeps, a
    /// download is filling, a program writes, or holds a change that is not uploaded yet, is
    /// left and counted. Whether `path` itself may be freed is the caller's to ask first
    /// ([`find_path_status`](Self::find_path_status)). Refused when the mount is stopped
    /// ([`FilesRefusal::Io`]): nothing more is freed then.
    pub async fn free_up(self: &Arc<Self>, path: &Path) -> Result<FreedSpace, FilesRefusal> {
        let (row, _) = self.find_onedrive_row(path)?;
        self.free_up_below(row).await
    }

    /// Frees the space of every downloaded copy that no pin keeps. Refused as
    /// [`free_up`](Self::free_up) is.
    pub async fn free_up_all(self: &Arc<Self>) -> Result<FreedSpace, FilesRefusal> {
        let Some(root) = self.directory_contents.find_root_id().and_then(|id| self.directory_contents.find_row(&id)) else { return Ok(FreedSpace::default()) };
        self.free_up_below(root).await
    }

    /// Frees the downloaded copies at and below `row`: the walk lists them here, off the
    /// write-operation thread, and each is freed on it, in a job of its own that is waited
    /// for before the next is put there ([`free_downloaded_copy`](Self::free_downloaded_copy)).
    /// A thread that is stopped drops the job: the free-up ends there, refused.
    async fn free_up_below(self: &Arc<Self>, row: Row) -> Result<FreedSpace, FilesRefusal> {
        let files_interface = Arc::clone(self);
        let listed = tokio::task::spawn_blocking(move || files_interface.list_downloaded_ids_below(&row)).await.unwrap_or_default();
        let (mut freed, mut given_up) = (FreedSpace::default(), false);
        for onedrive_id in listed {
            let files_interface = Arc::clone(self);
            match self.write_operation_thread.run_and_wait(move || files_interface.free_downloaded_copy(&onedrive_id)).await {
                Ok(copy) => freed = FreedSpace { files: freed.files + copy.files, bytes: freed.bytes + copy.bytes, kept: freed.kept + copy.kept },
                Err(_) => {
                    given_up = true;
                    break;
                }
            }
        }
        self.report.space.kick();
        match given_up {
            true => Err(FilesRefusal::Io("the free-up was given up: the folder is no longer served".into())),
            false => Ok(freed),
        }
    }

    /// The OneDrive ids of the files at and below `row` that have a downloaded copy now:
    /// what a free-up asks about, one by one. What is not downloaded takes no space. Only
    /// the ids are kept of the walk: how each file stands is read again at its turn.
    fn list_downloaded_ids_below(&self, row: &Row) -> Vec<String> {
        let files = self.directory_contents.list_files_below(row).into_iter();
        files.filter(|(file, _)| file.downloaded_copy().is_some()).filter_map(|(file, _)| file.row().map(|row| row.id.clone())).collect()
    }

    /// Frees the downloaded copy of the OneDrive file `onedrive_id`, unless something keeps
    /// it: what was freed, or that one copy was kept; nothing of either for a file that has
    /// no downloaded copy any more, or is gone from the folder.
    ///
    /// Called on the write-operation thread. How the file stands — where the mount shows
    /// it, its sync state, its pin, its download, its writers — is read here, and the copy
    /// is removed here: no write operation runs in between, so what is decided is true of
    /// the copy that is removed.
    fn free_downloaded_copy(&self, onedrive_id: &str) -> FreedSpace {
        let nothing = FreedSpace::default();
        let Some(path) = self.directory_contents.find_path(onedrive_id) else { return nothing };
        let Some(entry) = path.file_name().and_then(|name| name.to_str()).and_then(|name| self.directory_contents.find_entry_by_id(onedrive_id, name, &path)) else { return nothing };
        let (EntryState::File(file), Some(row)) = (&entry.state, entry.row()) else { return nothing };
        if file.downloaded_copy().is_none() {
            return nothing;
        }
        let (is_pinned, is_downloading) = (self.pins.find_pinning_row(row).is_some(), self.download_states.is_downloading(onedrive_id));
        let is_open_for_writing = self.is_open_for_writing(onedrive_id);
        let freed_bytes = match decide_free_up(&entry.state, is_pinned, is_downloading, is_open_for_writing) {
            FreeUpDecision::Free => self.backing_directory.remove_downloaded_copy(&path, onedrive_id),
            FreeUpDecision::LeavePinned | FreeUpDecision::LeaveModifiedHere | FreeUpDecision::LeaveOpenForWriting | FreeUpDecision::LeaveDownloading => None,
            FreeUpDecision::NothingToFree => return nothing,
        };
        match freed_bytes {
            Some(bytes) => {
                self.inotify_bridge.announce(vec![Announcement::FileShownChanged { path, onedrive_id: onedrive_id.to_owned() }]);
                FreedSpace { files: 1, bytes, kept: 0 }
            }
            None => FreedSpace { kept: 1, ..nothing },
        }
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
        let is_open_for_writing = self.is_open_for_writing(&row.id);
        Ok(PathStatus { onedrive_id: row.id, own_pin, pinned_above, entry_state: entry.state, is_downloading, is_open_for_writing })
    }

    /// Puts the pin on what is at `path`, or takes it off. A pin put on downloads what it
    /// keeps.
    pub async fn set_pin(&self, path: &Path, pinned: bool) -> Result<(), FilesRefusal> {
        let (row, rel) = self.find_onedrive_row(path)?;
        let changed = self.pins.set_pin(&row.id, pinned).await.map_err(|e| FilesRefusal::Io(e.to_string()))?;
        if changed {
            self.inotify_bridge.announce(vec![Announcement::FileShownChanged { path: rel, onedrive_id: row.id }]);
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

    /// The space the downloaded copies at and below `path` take.
    pub fn count_downloaded_below(&self, path: &Path) -> Result<DownloadedSpace, FilesRefusal> {
        let (row, _) = self.find_onedrive_row(path)?;
        let copies = self.directory_contents.list_files_below(&row).into_iter().filter_map(|(file, _)| file.downloaded_copy().copied());
        Ok(copies.fold(DownloadedSpace::default(), |counted, copy| DownloadedSpace { files: counted.files + 1, bytes: counted.bytes + copy.blocks * 512 }))
    }
}

#[cfg(test)]
mod tests;
