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
//! 6. the file lands, in one job on the write-operation thread
//!    ([`FileDownloader::land_at_asked_path`]): where the file is now is asked there, and
//!    the directories down to its path are made and the file is given its name
//!    ([`BackingDirectory::land`]) there, unless it was moved or removed here meanwhile —
//!    no rename runs between the question and the landing; what the download reports of
//!    that is decided in [`landing_attempt`];
//! 7. the activity records what happened, the space on disk is counted again, the download
//!    states are told how it ended, which answers whoever waits, and the change is announced
//!    through the [`InotifyBridge`].

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use konedrive_graph::pool::{Class, Size, TransferPool};
use konedrive_tree::Row;

use crate::hydration::source::{download_staged, ContentSource, Share, Split};
use crate::hydration::tracked::Tracked;
use crate::status::activity::{self, Kind as EventKind};
use crate::status::report::Report;

use super::backing_directory::BackingDirectory;
use super::directory_contents::DirectoryContents;
use super::download_states::{DownloadStart, DownloadStates, Waiter};
use super::errno::to_raw_errno;
use super::inotify_bridge::{Announcement, InotifyBridge};
use super::write_operation_thread::WriteOperationThread;

mod landing_attempt;
mod watermarked_source;

use landing_attempt::{to_download_result, LandingAttempt};
use watermarked_source::WatermarkedSource;

/// A download that failed and can be continued: the size of its unfinished file on disk, and
/// the checkpoint it is continued from, when it has one.
#[cfg(any(test, feature = "fault-injection"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnfinishedDownload {
    pub bytes_on_disk: u64,
    pub checkpoint: Option<u64>,
}

/// What downloads a OneDrive file whole into the backing directory.
pub(super) struct FileDownloader {
    backing_directory: Arc<BackingDirectory>,
    directory_contents: Arc<DirectoryContents>,
    download_states: Arc<DownloadStates>,
    /// What tells whoever watches the folder that what is shown of a file changed.
    inotify_bridge: Arc<InotifyBridge>,
    /// Where a finished download lands: beside no write operation.
    write_operation_thread: Arc<WriteOperationThread>,
    source: Arc<dyn ContentSource>,
    pool: Arc<TransferPool>,
    /// What the daemon's large downloads in parallel streams share.
    split_download_share: Arc<Share>,
    report: Report,
    /// The folder's path, which is the mount: a download is shown under its file's path there.
    folder: PathBuf,
    runtime: tokio::runtime::Handle,
}

/// Abandons a download in the download states unless it was finished there: a download that
/// panics still leaves no open waiting for it for ever.
struct RunningDownload {
    download_states: Arc<DownloadStates>,
    onedrive_id: String,
    finished: bool,
}

impl Drop for RunningDownload {
    fn drop(&mut self) {
        if !self.finished {
            self.download_states.abandon(&self.onedrive_id);
        }
    }
}

impl FileDownloader {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        backing_directory: Arc<BackingDirectory>,
        directory_contents: Arc<DirectoryContents>,
        download_states: Arc<DownloadStates>,
        inotify_bridge: Arc<InotifyBridge>,
        write_operation_thread: Arc<WriteOperationThread>,
        source: Arc<dyn ContentSource>,
        pool: Arc<TransferPool>,
        split_download_share: Arc<Share>,
        report: Report,
        folder: PathBuf,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        FileDownloader { backing_directory, directory_contents, download_states, inotify_bridge, write_operation_thread, source, pool, split_download_share, report, folder, runtime }
    }

    /// Tells whoever watches the folder that what is shown of the OneDrive file
    /// `onedrive_id` at `path` changed.
    fn announce_file_change(&self, path: PathBuf, onedrive_id: String) {
        self.inotify_bridge.announce(vec![Announcement::FileShownChanged { path, onedrive_id }]);
    }

    /// Downloads the OneDrive file `row` to `rel` unless a download of it runs already;
    /// `waiter` is answered when the download ends, however it ends.
    pub(super) fn download(self: &Arc<Self>, row: Row, rel: PathBuf, class: Class, waiter: Option<Waiter>) {
        let unfinished_file = match self.download_states.start_or_join(&row.id, waiter) {
            DownloadStart::Started { unfinished_file } => unfinished_file,
            DownloadStart::Joined => return,
        };
        // Being downloaded, for whoever shows the state.
        self.announce_file_change(rel.clone(), row.id.clone());
        let downloader = Arc::clone(self);
        self.runtime.spawn(async move {
            let mut running = RunningDownload { download_states: Arc::clone(&downloader.download_states), onedrive_id: row.id.clone(), finished: false };
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
            downloader.announce_file_change(rel, row.id);
        });
    }

    /// The downloads that failed and can be continued, oldest failure first.
    #[cfg(any(test, feature = "fault-injection"))]
    pub(super) fn list_unfinished_downloads(&self) -> Vec<UnfinishedDownload> {
        let measure = |file: &Arc<File>| Some(UnfinishedDownload { bytes_on_disk: file.metadata().ok()?.len(), checkpoint: konedrive_fs::placeholder::read_progress(file).ok().flatten().map(|progress| progress.bytes) });
        self.download_states.list_unfinished_files().iter().filter_map(measure).collect()
    }

    /// The path a download is shown under: the file's path in the folder.
    fn shown_path(&self, rel: &Path) -> String {
        self.folder.join(rel).display().to_string()
    }

    /// Downloads `row` into a file without a name, continuing `unfinished_file` when an
    /// earlier download left it, and lands it at `rel`. Its size, or the errno it failed
    /// with.
    async fn download_and_land(self: &Arc<Self>, row: &Row, rel: &Path, class: Class, unfinished_file: Option<Arc<File>>) -> Result<u64, i32> {
        let io_errno = |e: io::Error| to_raw_errno(&e);
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
            // On the write-operation thread, which is no thread of the runtime: the file's
            // own steps wait for the disk. A thread that is stopped drops the job: the
            // folder is no longer served, and nothing lands.
            let (downloader, row, at, file) = (Arc::clone(self), row.clone(), rel.to_owned(), Arc::clone(&file));
            let attempt = self.write_operation_thread.run_and_wait(move || downloader.land_at_asked_path(&file, &row, &at)).await.map_err(|_| libc::EIO)?.map_err(io_errno)?;
            to_download_result(attempt).map(|()| size)
        }
        .await;
        drop(tracked);
        if landed.is_ok() {
            slot.succeeded();
        }
        landed
    }

    /// Lands `file`, the whole and checked content of the OneDrive file `row`, at `rel`, the
    /// path its download was asked for — unless the file is no longer there: moved or
    /// removed here while it was downloaded, the download is of no path now, and nothing
    /// lands. The directories down to the path are made first, each marked with its
    /// OneDrive id.
    ///
    /// Called on the write-operation thread. Where the mount shows the file now is read
    /// here, by its OneDrive id, and the file is given its name here: no write operation —
    /// a rename, a remove — runs in between, so the copy lands where the file is. What is
    /// at the path by then is never replaced unless it holds nothing
    /// ([`BackingDirectory::land`]).
    fn land_at_asked_path(&self, file: &File, row: &Row, rel: &Path) -> io::Result<LandingAttempt> {
        if self.directory_contents.find_path(&row.id).as_deref() != Some(rel) {
            return Ok(LandingAttempt::FileMovedMeanwhile);
        }
        self.directory_contents.make_marked_directories(rel.parent().unwrap_or(Path::new("")))?;
        self.backing_directory.land(file, rel, &row.id, crate::clock::to_system_time(row.mtime)).map(LandingAttempt::Attempted)
    }
}

#[cfg(test)]
mod tests;
