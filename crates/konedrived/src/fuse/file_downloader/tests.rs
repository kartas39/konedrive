//! A download on the parts of a folder without a mount: the folder of the write operations'
//! tests, a bridge whose thread is not started, and a source whose every file holds what
//! the test gives it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use konedrive_graph::pool::{Class, TransferPool};
use konedrive_tree::Kind;
use tokio::sync::oneshot;

use super::{FileDownloader, RunningDownload};
use crate::fuse::directory_contents::tests::row;
use crate::fuse::download_states::DownloadStates;
use crate::fuse::inotify_bridge::{Announcement, InotifyBridge};
use crate::fuse::write_operations::tests::{at, folder, Folder};
use crate::hydration::source::{ContentSource, Fetched, Share, SourceError};
use crate::status::report::Report;

/// A source whose every file holds `content`, and says nothing of its version.
struct SameContent(&'static [u8]);

#[async_trait::async_trait]
impl ContentSource for SameContent {
    async fn fetch(&self, _item_id: &str, from: u64, _end: Option<u64>) -> Result<Fetched, SourceError> {
        let rest = self.0.get(from as usize..).unwrap_or_default();
        Ok(Fetched { served_from: from, size: self.0.len() as u64, mtime: UNIX_EPOCH, version: None, stream: Box::new(std::io::Cursor::new(rest.to_vec())) })
    }
}

/// What the tests of the parts that are given a file downloader ask of it.
impl FileDownloader {
    /// Whether a download of the OneDrive file `id` runs now.
    pub(in crate::fuse) fn is_downloading(&self, id: &str) -> bool {
        self.download_states.is_downloading(id)
    }
}

struct Downloads {
    folder: Folder,
    download_states: Arc<DownloadStates>,
    inotify_bridge: Arc<InotifyBridge>,
    file_downloader: Arc<FileDownloader>,
    _runtime: tokio::runtime::Runtime,
}

/// A file downloader over the folder of the write operations' tests, whose source gives
/// every file one byte, as the fixture's rows say.
fn downloads() -> Downloads {
    let folder = folder(Vec::new());
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let download_states = Arc::new(DownloadStates::new(1 << 20));
    let inotify_bridge = Arc::new(InotifyBridge::new(folder.disk.clone(), Arc::clone(&folder.node_resolver), Arc::clone(&folder.directory_contents)));
    let file_downloader = Arc::new(FileDownloader::new(
        Arc::clone(&folder.backing_directory),
        Arc::clone(&folder.directory_contents),
        Arc::clone(&download_states),
        Arc::clone(&inotify_bridge),
        Arc::new(SameContent(b"x")),
        TransferPool::new(4),
        Share::new(),
        Report::nowhere(),
        folder.disk.clone(),
        runtime.handle().clone(),
    ));
    Downloads { folder, download_states, inotify_bridge, file_downloader, _runtime: runtime }
}

impl Downloads {
    /// Downloads the fixture's file `A` to `rel`, and waits for how it ended.
    fn download_to(&self, rel: &str) -> Result<(), i32> {
        let (waiter, told) = oneshot::channel();
        self.file_downloader.download(row("A", Some("R"), "a.txt", Kind::File), PathBuf::from(rel), Class::Open, Some(waiter));
        told.blocking_recv().expect("a download answers whoever waits for it")
    }
}

/// The file lands at its path as the file's downloaded copy, and what is shown of it is
/// announced twice: being downloaded, and downloaded.
#[test]
fn a_download_lands_at_the_files_path_as_its_downloaded_copy() {
    let downloads = downloads();
    assert_eq!(downloads.download_to("a.txt"), Ok(()));
    assert_eq!(std::fs::read(downloads.folder.disk.join("a.txt")).unwrap(), b"x");
    assert!(downloads.folder.backing_directory.find_downloaded_copy(&at("a.txt"), "A").is_some());
    assert!(!downloads.file_downloader.is_downloading("A"));
    let announced = downloads.inotify_bridge.list_queued_announcements();
    assert_eq!(announced.first(), Some(&Announcement::FileShownChanged { path: at("a.txt"), onedrive_id: "A".into() }));
}

/// One download of a file runs at a time: whoever asks while it runs waits for the one that
/// runs, and nothing more is started.
#[test]
fn a_download_asked_for_while_one_runs_joins_the_one_that_runs() {
    let downloads = downloads();
    downloads.download_states.start_or_join("A", None);
    let (waiter, mut told) = oneshot::channel();
    downloads.file_downloader.download(row("A", Some("R"), "a.txt", Kind::File), at("a.txt"), Class::Open, Some(waiter));
    // Nothing was started, and nothing is announced: the one that runs said so already.
    std::thread::sleep(Duration::from_millis(100));
    assert!(told.try_recv().is_err() && !downloads.folder.disk.join("a.txt").exists());
    assert!(downloads.inotify_bridge.list_queued_announcements().is_empty());
    // It ends as the one that runs does.
    downloads.download_states.finish("A", Err(libc::ENOSPC));
    assert_eq!(told.blocking_recv(), Ok(Err(libc::ENOSPC)));
}

/// The file is no longer where the download was asked for — moved or removed here while it
/// was downloaded: nothing lands, and the next read starts a download where the file is.
#[test]
fn a_download_of_a_file_that_moved_meanwhile_lands_nowhere() {
    let downloads = downloads();
    assert_eq!(downloads.download_to("docs/a.txt"), Err(libc::ESTALE));
    assert!(!downloads.folder.disk.join("docs/a.txt").exists() && !downloads.folder.disk.join("a.txt").exists());
    assert!(!downloads.file_downloader.is_downloading("A"));
}

/// A download that never reports its end — it panicked — leaves no open waiting for it for
/// ever.
#[test]
fn a_running_download_that_is_dropped_unfinished_is_abandoned() {
    let download_states = Arc::new(DownloadStates::new(1 << 20));
    let (waiter, told) = oneshot::channel();
    download_states.start_or_join("A", Some(waiter));
    drop(RunningDownload { download_states: Arc::clone(&download_states), onedrive_id: "A".into(), finished: false });
    assert_eq!(told.blocking_recv(), Ok(Err(libc::EIO)));
    assert!(!download_states.is_downloading("A"));
}

#[test]
fn a_running_download_that_finished_is_not_abandoned() {
    let download_states = Arc::new(DownloadStates::new(1 << 20));
    let (waiter, mut told) = oneshot::channel();
    download_states.start_or_join("A", Some(waiter));
    drop(RunningDownload { download_states: Arc::clone(&download_states), onedrive_id: "A".into(), finished: true });
    assert!(download_states.is_downloading("A") && told.try_recv().is_err(), "its end is the download states' to be told");
}
