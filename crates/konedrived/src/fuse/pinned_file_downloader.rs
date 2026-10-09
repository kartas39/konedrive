//! What a pin keeps on this device is downloaded: every pinned file that is not downloaded.
//!
//! [`PinnedFileDownloader`] holds the parts the sweep uses and no state of its own: the
//! [`Pins`], which say what is pinned, the [`DirectoryContents`], which say what of it is
//! not downloaded, and the [`FileDownloader`], which downloads it. The sweep runs when the
//! mount starts, when a pin is put on and after every cycle, so that a new file in a pinned
//! directory and a pinned file whose content changed are downloaded by it.

use std::sync::Arc;

use konedrive_graph::pool::Class;

use super::directory_contents::{DirectoryContents, FileSyncState};
use super::file_downloader::{FileDownloader, FileToDownload};
use super::pins::Pins;

pub(super) struct PinnedFileDownloader {
    pins: Arc<Pins>,
    directory_contents: Arc<DirectoryContents>,
    file_downloader: Arc<FileDownloader>,
    /// Where the sweep runs.
    runtime: tokio::runtime::Handle,
}

impl PinnedFileDownloader {
    pub(super) fn new(pins: Arc<Pins>, directory_contents: Arc<DirectoryContents>, file_downloader: Arc<FileDownloader>, runtime: tokio::runtime::Handle) -> Self {
        PinnedFileDownloader { pins, directory_contents, file_downloader, runtime }
    }

    /// Downloads every pinned file that is not downloaded. Off the caller's thread: a cycle
    /// calls this while it holds the folder's tree lock, and a large pinned directory is a
    /// long walk.
    pub(super) fn download_pinned_files(&self) {
        let (pins, directory_contents, file_downloader) = (Arc::clone(&self.pins), Arc::clone(&self.directory_contents), Arc::clone(&self.file_downloader));
        self.runtime.spawn_blocking(move || {
            for file in list_pinned_files_to_download(&pins, &directory_contents) {
                file_downloader.download(file, Class::Download, None);
            }
        });
    }
}

/// The files at and below what carries a pin that are not downloaded.
pub(super) fn list_pinned_files_to_download(pins: &Pins, directory_contents: &DirectoryContents) -> Vec<FileToDownload> {
    let mut files = Vec::new();
    for id in pins.list_pinned_ids() {
        let Some(row) = directory_contents.find_row(&id) else { continue };
        for (file, path) in directory_contents.list_files_below(&row) {
            if let FileSyncState::RemoteOnly(row) = file {
                files.push(FileToDownload { row, path });
            }
        }
    }
    files
}

#[cfg(test)]
mod tests;
