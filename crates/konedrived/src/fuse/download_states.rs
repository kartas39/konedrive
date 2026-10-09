//! The state of each file's download: whether the whole file is being downloaded, who waits
//! for that, and what a download that failed left to continue from; and, beside the states,
//! how much of each file programs have read as ranges.
//!
//! [`DownloadStates`] holds one [`DownloadState`] per OneDrive file that has one, by its id,
//! and the count of the bytes read as ranges, and nothing else: it does no I/O and starts
//! nothing. [`FileDownloader`](super::file_downloader::FileDownloader) reports to it what a
//! download does; the read request counts in it what it fetched as ranges, and asks it what
//! a running download has written already; free-up and the state shown ask it whether a
//! file is downloading.
//!
//! A file's state changes like this; a file without a state has no download and none that
//! left a file:
//!
//! - none, or `Failed` → `Running`, by [`DownloadStates::start_or_join`], which hands out the
//!   unfinished file of a `Failed` one to continue from;
//! - `Running` → none, by [`DownloadStates::finish`] with a download that ended well, or
//!   badly before it had a file to write into, and by [`DownloadStates::abandon`];
//! - `Running` → `Failed`, by `finish` with one that ended badly and had a file to write into:
//!   the file is kept, and the oldest such file beyond [`UNFINISHED_FILES_KEPT`] is let go,
//!   which leaves its file without a state.
//!
//! The bytes read as ranges are a count of their own, whatever the file's state: they are
//! counted by [`DownloadStates::count_bytes_read`], and dropped only when a download of the
//! file ends well.

use std::collections::HashMap;
use std::fs::File;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

/// How many downloads that failed keep their unfinished file, to be continued from: the
/// latest ones.
const UNFINISHED_FILES_KEPT: usize = 16;

/// How far behind `written_up_to` the file of a running download is surely written: the
/// download writes each piece it has read before it reads the next, and a piece is no larger
/// than this.
const WRITTEN_BEHIND: u64 = 1 << 20;

/// A caller in the daemon that waits for a download to end: told that it ended well, or the
/// errno it failed with.
pub(super) type Waiter = oneshot::Sender<Result<(), i32>>;

/// The state of one file's download. A file no download of which runs, and none of which
/// left a file, has no state.
pub(super) enum DownloadState {
    /// It is being downloaded, from the moment the download was asked for.
    Running {
        waiters: Vec<Waiter>,
        /// How far `file` is written in order. A download in parts writes out of order and
        /// leaves it at nothing.
        written_up_to: Arc<AtomicU64>,
        /// The file without a name the download writes into; `None` until the download has
        /// its turn in the pool.
        file: Option<Arc<File>>,
    },
    /// Its last download failed; the file it wrote into has no name and goes with the daemon.
    Failed {
        unfinished_file: Arc<File>,
        /// Counts the failures up: the unfinished files are kept and let go by it, oldest
        /// first.
        failure_number: u64,
    },
}

/// What asking for a download did.
pub(super) enum DownloadStart {
    /// No download of the file ran: the caller runs one, continuing from `unfinished_file`
    /// when an earlier one left it.
    Started { unfinished_file: Option<Arc<File>> },
    /// A download of the file runs already.
    Joined,
}

/// The download of `id` failed with `unfinished_file` written in part: the file is kept, and
/// the oldest one beyond [`UNFINISHED_FILES_KEPT`] is let go.
fn keep_unfinished_file(states: &mut HashMap<String, DownloadState>, id: &str, unfinished_file: Arc<File>) {
    let failure_number = failure_numbers(states).map(|(_, number)| number + 1).max().unwrap_or(0);
    states.insert(id.to_owned(), DownloadState::Failed { unfinished_file, failure_number });
    if failure_numbers(states).count() <= UNFINISHED_FILES_KEPT {
        return;
    }
    let Some(oldest) = failure_numbers(states).min_by_key(|(_, number)| *number).map(|(id, _)| id.to_owned()) else { return };
    states.remove(&oldest);
}

/// The OneDrive id and the failure number of every `Failed` state.
fn failure_numbers(states: &HashMap<String, DownloadState>) -> impl Iterator<Item = (&str, u64)> {
    states.iter().filter_map(|(id, state)| match state {
        DownloadState::Failed { failure_number, .. } => Some((id.as_str(), *failure_number)),
        DownloadState::Running { .. } => None,
    })
}

/// What [`DownloadStates`] holds, by the files' OneDrive ids.
#[derive(Default)]
struct Downloads {
    states: HashMap<String, DownloadState>,
    /// How many bytes of each file programs have read as ranges since it was last
    /// downloaded.
    bytes_read_as_ranges: HashMap<String, u64>,
}

/// The state of each file's download, and the bytes of each file read as ranges, by the
/// file's OneDrive id.
pub(super) struct DownloadStates {
    downloads: Mutex<Downloads>,
    /// Once that many bytes of a file were read as ranges, the file is downloaded whole
    /// (`[transfers] download_after` in `config.toml`).
    download_after: u64,
}

impl DownloadStates {
    pub(super) fn new(download_after: u64) -> Self {
        DownloadStates { downloads: Mutex::default(), download_after }
    }

    /// A download of the OneDrive file `id` is asked for; `waiter` is answered when it ends,
    /// however it ends. Whether the caller is to run it, or one runs already.
    pub(super) fn start_or_join(&self, id: &str, waiter: Option<Waiter>) -> DownloadStart {
        let mut downloads = crate::panic::lock(&self.downloads);
        if let Some(DownloadState::Running { waiters, .. }) = downloads.states.get_mut(id) {
            waiters.extend(waiter);
            return DownloadStart::Joined;
        }
        let running = DownloadState::Running { waiters: waiter.into_iter().collect(), written_up_to: Arc::default(), file: None };
        let unfinished_file = match downloads.states.insert(id.to_owned(), running) {
            Some(DownloadState::Failed { unfinished_file, .. }) => Some(unfinished_file),
            Some(DownloadState::Running { .. }) | None => None,
        };
        DownloadStart::Started { unfinished_file }
    }

    /// The running download of `id` writes into `file` from now on. What the download moves
    /// as it writes: how far the file is written in order.
    pub(super) fn begin_writing(&self, id: &str, file: Arc<File>) -> Arc<AtomicU64> {
        match crate::panic::lock(&self.downloads).states.get_mut(id) {
            Some(DownloadState::Running { written_up_to, file: writes_into, .. }) => {
                *writes_into = Some(file);
                Arc::clone(written_up_to)
            }
            // Abandoned meanwhile: nobody reads what it writes.
            _ => Arc::default(),
        }
    }

    /// The file the running download of `id` writes into, when it is surely written up to
    /// byte `end`: a read that ends there is answered from it.
    pub(super) fn find_written_file(&self, id: &str, end: u64) -> Option<Arc<File>> {
        match crate::panic::lock(&self.downloads).states.get(id) {
            Some(DownloadState::Running { written_up_to, file: Some(file), .. }) if end + WRITTEN_BEHIND <= written_up_to.load(Ordering::SeqCst) => Some(Arc::clone(file)),
            _ => None,
        }
    }

    /// The running download of `id`, taken out of the states: its waiters and the file it
    /// wrote into. `None` when no download of it runs, and nothing is changed.
    fn take_running(downloads: &mut Downloads, id: &str) -> Option<(Vec<Waiter>, Option<Arc<File>>)> {
        if !matches!(downloads.states.get(id), Some(DownloadState::Running { .. })) {
            return None;
        }
        match downloads.states.remove(id) {
            Some(DownloadState::Running { waiters, file, .. }) => Some((waiters, file)),
            Some(DownloadState::Failed { .. }) | None => None,
        }
    }

    /// The running download of `id` ended, well or with an errno: its waiters are told, and
    /// one that ended badly keeps the file it wrote into, to be continued from. Nothing for
    /// a download that does not run: it was abandoned already.
    pub(super) fn finish(&self, id: &str, outcome: Result<(), i32>) {
        let waiters = {
            let mut downloads = crate::panic::lock(&self.downloads);
            let Some((waiters, file)) = Self::take_running(&mut downloads, id) else { return };
            match (outcome, file) {
                // Downloaded: what was read of it as ranges counts no more.
                (Ok(()), _) => drop(downloads.bytes_read_as_ranges.remove(id)),
                (Err(_), Some(file)) => keep_unfinished_file(&mut downloads.states, id, file),
                (Err(_), None) => {}
            }
            waiters
        };
        for waiter in waiters {
            let _ = waiter.send(outcome);
        }
    }

    /// The running download of `id` will never report its end (it panicked): its waiters are
    /// failed, and nothing of it is kept.
    pub(super) fn abandon(&self, id: &str) {
        let Some((waiters, _)) = Self::take_running(&mut crate::panic::lock(&self.downloads), id) else { return };
        for waiter in waiters {
            let _ = waiter.send(Err(libc::EIO));
        }
    }

    /// `bytes` more of the OneDrive file `id`, which is `file_size` long, were read as a
    /// range. Whether enough of it was read by now for the whole file to be downloaded.
    pub(super) fn count_bytes_read(&self, id: &str, bytes: u64, file_size: u64) -> bool {
        let mut downloads = crate::panic::lock(&self.downloads);
        let bytes_read = downloads.bytes_read_as_ranges.entry(id.to_owned()).or_default();
        *bytes_read += bytes;
        reaches_download_threshold(*bytes_read, self.download_after, file_size)
    }

    /// Whether the OneDrive file `id` is being downloaded whole: from the moment its download
    /// was asked for until it ended.
    pub(super) fn is_downloading(&self, id: &str) -> bool {
        matches!(crate::panic::lock(&self.downloads).states.get(id), Some(DownloadState::Running { .. }))
    }

    /// The mount is being taken off: whoever waits for a download is failed now. A download
    /// that still runs ends as any other, with nobody waiting.
    pub(super) fn fail_waiters(&self) {
        let waiters: Vec<Waiter> = {
            let mut downloads = crate::panic::lock(&self.downloads);
            let running = downloads.states.values_mut().filter_map(|state| match state {
                DownloadState::Running { waiters, .. } => Some(std::mem::take(waiters)),
                DownloadState::Failed { .. } => None,
            });
            running.flatten().collect()
        };
        for waiter in waiters {
            let _ = waiter.send(Err(libc::EIO));
        }
    }

    /// The unfinished files of the downloads that failed, oldest failure first.
    #[cfg(any(test, feature = "fault-injection"))]
    pub(super) fn list_unfinished_files(&self) -> Vec<Arc<File>> {
        let downloads = crate::panic::lock(&self.downloads);
        let mut kept: Vec<(u64, Arc<File>)> = downloads.states.values().filter_map(|state| match state {
            DownloadState::Failed { unfinished_file, failure_number } => Some((*failure_number, Arc::clone(unfinished_file))),
            DownloadState::Running { .. } => None,
        }).collect();
        kept.sort_by_key(|(failure_number, _)| *failure_number);
        kept.into_iter().map(|(_, unfinished_file)| unfinished_file).collect()
    }
}

/// Whether `bytes_read` bytes of a file `file_size` long, read as ranges, are enough for the
/// whole file to be downloaded. A file smaller than `download_after` is downloaded once all
/// of it was read: it would be fetched again at every read otherwise.
pub(super) fn reaches_download_threshold(bytes_read: u64, download_after: u64, file_size: u64) -> bool {
    bytes_read >= download_after.min(file_size)
}

#[cfg(test)]
mod tests;
