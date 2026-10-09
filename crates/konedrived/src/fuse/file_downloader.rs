//! The download of a whole file: into a file without a name, which lands at the file's path
//! in the backing directory once it is whole and checked.
//!
//! [`FileDownloader`] runs each download and keeps no state of its own: whether a file is
//! being downloaded, who waits for it and what a failed download left are
//! [`DownloadStates`]'s, which it reports to. One download, in order:
//!
//! 1. [`FileDownloader::download`] asks the download states; a download that runs already is
//!    joined, and nothing more is done here;
//! 2. the change is announced (the file is shown as being downloaded), and the rest runs on
//!    the runtime;
//! 3. a slot of the account's pool is waited for;
//! 4. the file to write into is the unfinished file of an earlier download, continued from
//!    its checkpoint, or a new file without a name;
//! 5. the content is fetched as every download of the daemon is ([`download_staged`]: breaks,
//!    the version it belongs to, the hash), through a source that shows its progress
//!    ([`Tracked`]) and tells the download states how far the file is written
//!    ([`WatermarkedSource`]), for the reads that come meanwhile;
//! 6. the directories down to the file's path are made, and the file lands
//!    ([`BackingDirectory::land`]) unless it was moved or removed here meanwhile;
//! 7. the activity records what happened, the space on disk is counted again, the download
//!    states are told how it ended, which answers whoever waits, and the change is announced.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use konedrive_graph::pool::{Class, Size, TransferPool};
use konedrive_tree::Row;

use crate::hydration::source::{download_staged, ContentSource, Fetched, Share, SourceError, Split};
use crate::hydration::tracked::Tracked;
use crate::status::activity::{self, Kind as EventKind};
use crate::status::report::Report;

use super::backing_directory::{BackingDirectory, LandingOutcome};
use super::directory_contents::DirectoryContents;
use super::download_states::{DownloadStart, DownloadStates, Waiter};

/// Tells whoever watches the folder that what is shown of the OneDrive file at a path
/// changed: the path in the folder, and the file's OneDrive id.
pub(super) type FileChangeAnnouncer = Arc<dyn Fn(PathBuf, String) + Send + Sync>;

/// What downloads a OneDrive file whole into the backing directory.
pub(super) struct FileDownloader {
    pub(super) backing_directory: Arc<BackingDirectory>,
    pub(super) directory_contents: Arc<DirectoryContents>,
    pub(super) download_states: Arc<DownloadStates>,
    pub(super) source: Arc<dyn ContentSource>,
    pub(super) pool: Arc<TransferPool>,
    /// What the daemon's large downloads in parallel streams share.
    pub(super) split_download_share: Arc<Share>,
    pub(super) report: Report,
    /// The folder's path, which is the mount: a download is shown under its file's path there.
    pub(super) folder: PathBuf,
    pub(super) announce_file_change: FileChangeAnnouncer,
    pub(super) runtime: tokio::runtime::Handle,
}

/// Abandons a download in the download states unless it was finished there: a download that
/// panics still leaves no open waiting for it for ever.
struct RunningDownload {
    download_states: Arc<DownloadStates>,
    id: String,
    finished: bool,
}

impl Drop for RunningDownload {
    fn drop(&mut self) {
        if !self.finished {
            self.download_states.abandon(&self.id);
        }
    }
}

impl FileDownloader {
    /// Downloads the OneDrive file `row` to `rel` unless a download of it runs already;
    /// `waiter` is answered when the download ends, however it ends.
    pub(super) fn download(self: &Arc<Self>, row: Row, rel: PathBuf, class: Class, waiter: Option<Waiter>) {
        let unfinished_file = match self.download_states.start_or_join(&row.id, waiter) {
            DownloadStart::Started { unfinished_file } => unfinished_file,
            DownloadStart::Joined => return,
        };
        // `hydrating`, for whoever shows the state.
        (self.announce_file_change)(rel.clone(), row.id.clone());
        let downloader = Arc::clone(self);
        self.runtime.spawn(async move {
            let mut running = RunningDownload { download_states: Arc::clone(&downloader.download_states), id: row.id.clone(), finished: false };
            let downloaded = downloader.download_and_land(&row, &rel, class, unfinished_file).await;
            let shown_path = downloader.shown_path(&rel);
            let event = match &downloaded {
                Ok(size) => activity::event(EventKind::Downloaded, shown_path, activity::human_size(*size)),
                Err(errno) => activity::event(EventKind::Failed, shown_path, activity::failure_reason(*errno)),
            };
            downloader.report.activity.record(vec![event]).await;
            downloader.report.space.kick();
            downloader.download_states.finish(&row.id, downloaded.map(|_| ()));
            running.finished = true;
            (downloader.announce_file_change)(rel, row.id);
        });
    }

    /// The path a download is shown under: the file's path in the folder.
    fn shown_path(&self, rel: &Path) -> String {
        self.folder.join(rel).display().to_string()
    }

    /// Downloads `row` into a file without a name, continuing `unfinished_file` when an
    /// earlier download left it, and lands it at `rel`. Its size, or the errno it failed
    /// with.
    async fn download_and_land(&self, row: &Row, rel: &Path, class: Class, unfinished_file: Option<Arc<File>>) -> Result<u64, i32> {
        let io_errno = |e: io::Error| e.raw_os_error().unwrap_or(libc::EIO);
        let mut slot = self.pool.acquire_sized(class, Size::of(row.size)).await;
        // The file a download is built in has no name: it is in the backing directory's
        // filesystem and in no directory, so nothing ever lists a file that is not whole, and
        // one the daemon leaves behind goes by itself. What an earlier download of the file
        // left is continued from its checkpoint when it is of the same version.
        let file = match unfinished_file {
            Some(file) => file,
            None => Arc::new(self.backing_directory.make_unnamed_file().map_err(io_errno)?),
        };
        let tracked = match class {
            Class::Open => Tracked::opening(Arc::clone(&self.source), self.report.transfers.clone(), self.shown_path(rel)),
            _ => Tracked::new(Arc::clone(&self.source), self.report.transfers.clone(), self.shown_path(rel)),
        };
        // What of the download is on disk in order, for the reads that come meanwhile.
        let written_up_to = self.download_states.begin_writing(&row.id, Arc::clone(&file));
        let watermarked = WatermarkedSource { inner: &tracked, written_up_to };
        let landed = async {
            // A large file that nothing waits to open (a pinned one) goes in parallel parts,
            // the slot held here being its first stream's.
            let split = (class != Class::Open && Size::of(row.size) == Size::Large).then(|| Split::new(Arc::clone(&self.pool), Arc::clone(&self.split_download_share)));
            download_staged(&file, &row.id, row.size, &watermarked, split.as_ref()).await?;
            let size = tracked.fetched().unwrap_or(row.size);
            // The directories down to its path, each marked with its OneDrive id.
            self.directory_contents.make_marked_directories(rel.parent().unwrap_or(Path::new(""))).map_err(io_errno)?;
            // Moved or removed here while it was downloaded: the download is of no path now,
            // and the next read starts one where the file is.
            if self.directory_contents.find_path(&row.id).as_deref() != Some(rel) {
                return Err(libc::ESTALE);
            }
            let (id, modified, at, file, backing_directory) = (row.id.clone(), crate::clock::to_system_time(row.mtime), rel.to_owned(), Arc::clone(&file), Arc::clone(&self.backing_directory));
            // The file's own steps wait for the disk: not on the runtime's threads.
            let landed = tokio::task::spawn_blocking(move || backing_directory.land(&file, &at, &id, modified)).await.map_err(|_| libc::EIO)?.map_err(io_errno)?;
            if landed == LandingOutcome::Refused {
                return Err(libc::EEXIST);
            }
            Ok(size)
        }
        .await;
        drop(tracked);
        if landed.is_ok() {
            slot.succeeded();
        }
        landed
    }
}

/// The download's source, telling how far the file is written as its one stream is read. A
/// download in parts (`end` given) writes out of order and tells nothing.
struct WatermarkedSource<'a> {
    inner: &'a dyn ContentSource,
    written_up_to: Arc<AtomicU64>,
}

/// The one stream of a download, counting what was read from it.
struct WatermarkedStream {
    inner: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    written_up_to: Arc<AtomicU64>,
}

impl tokio::io::AsyncRead for WatermarkedStream {
    fn poll_read(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &polled {
            self.written_up_to.fetch_add((buf.filled().len() - before) as u64, Ordering::SeqCst);
        }
        polled
    }
}

#[async_trait::async_trait]
impl ContentSource for WatermarkedSource<'_> {
    async fn fetch(&self, item_id: &str, from: u64, end: Option<u64>) -> Result<Fetched, SourceError> {
        let mut fetched = self.inner.fetch(item_id, from, end).await?;
        if end.is_none() {
            // Also when the download starts over: what is on disk counts from here again.
            self.written_up_to.store(fetched.served_from, Ordering::SeqCst);
            fetched.stream = Box::new(WatermarkedStream { inner: fetched.stream, written_up_to: Arc::clone(&self.written_up_to) });
        }
        Ok(fetched)
    }

    fn progress(&self, done: u64, size: u64) {
        self.inner.progress(done, size);
    }
}
