//! The state of each file's download: whether the whole file is being downloaded, who waits
//! for that, how much of it programs have read as ranges, and what a download that failed
//! left to continue from.
//!
//! [`DownloadStates`] holds one [`DownloadState`] per OneDrive file, by its id, and nothing
//! else: it does no I/O and starts nothing. [`FileDownloader`](super::file_download) reports
//! to it what a download does; the read request counts in it what it fetched as ranges, and
//! asks it what a running download has written already; free-up and the state shown ask it
//! whether a file is downloading.
//!
//! A file's state changes like this:
//!
//! - nothing, or `NotRunning`, or `Failed` → `Running`, by [`DownloadStates::start_or_join`],
//!   which hands out the unfinished file of a `Failed` one to continue from;
//! - `Running` → gone, by [`DownloadStates::finish`] with a download that ended well: the file
//!   is downloaded, and nothing is left to remember;
//! - `Running` → `Failed`, by `finish` with one that ended badly and had a file to write into:
//!   the file is kept, the oldest such file beyond [`UNFINISHED_FILES_KEPT`] is let go, and
//!   its state becomes `NotRunning`;
//! - `Running` → `NotRunning`, by `finish` with one that ended badly before it had a file, and
//!   by [`DownloadStates::abandon`].
//!
//! The bytes read as ranges are counted in every state, and forgotten only when a download
//! ends well.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use konedrive_fs::placeholder::State;
use tokio::sync::oneshot;

use super::directory_contents::FileSyncState;

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

/// The state of one file's download. `bytes_read` is how many bytes of the file programs
/// have read as ranges since it was last downloaded.
pub(super) enum DownloadState {
    /// No download of it runs, and none left a file.
    NotRunning { bytes_read: u64 },
    /// It is being downloaded, from the moment the download was asked for.
    Running {
        waiters: Vec<Waiter>,
        bytes_read: u64,
        /// How far `file` is written in order. A download in parts writes out of order and
        /// leaves it at nothing.
        written_up_to: Arc<AtomicU64>,
        /// The file without a name the download writes into; `None` until the download has
        /// its turn in the pool.
        file: Option<Arc<File>>,
    },
    /// Its last download failed; the file it wrote into has no name and goes with the daemon.
    Failed { unfinished_file: Arc<File>, bytes_read: u64 },
}

/// What asking for a download did.
pub(super) enum DownloadStart {
    /// No download of the file ran: the caller runs one, continuing from `unfinished_file`
    /// when an earlier one left it.
    Started { unfinished_file: Option<Arc<File>> },
    /// A download of the file runs already.
    Joined,
}

/// The states by OneDrive id, and the ids of the `Failed` ones, oldest failure first.
#[derive(Default)]
struct StateTable {
    states: HashMap<String, DownloadState>,
    failed_ids: VecDeque<String>,
}

impl StateTable {
    /// The download of `id` failed with `unfinished_file` written in part: kept, and the
    /// oldest one beyond [`UNFINISHED_FILES_KEPT`] is let go.
    fn keep_unfinished_file(&mut self, id: &str, unfinished_file: Arc<File>, bytes_read: u64) {
        self.states.insert(id.to_owned(), DownloadState::Failed { unfinished_file, bytes_read });
        self.failed_ids.push_back(id.to_owned());
        if self.failed_ids.len() <= UNFINISHED_FILES_KEPT {
            return;
        }
        let Some(oldest) = self.failed_ids.pop_front() else { return };
        if let Some(DownloadState::Failed { bytes_read, .. }) = self.states.remove(&oldest) {
            self.states.insert(oldest, DownloadState::NotRunning { bytes_read });
        }
    }
}

/// The state of each file's download, by the file's OneDrive id.
pub(super) struct DownloadStates {
    table: Mutex<StateTable>,
    /// Once that many bytes of a file were read as ranges, the file is downloaded whole
    /// (`[transfers] download_after` in `config.toml`).
    download_after: u64,
}

impl DownloadStates {
    pub(super) fn new(download_after: u64) -> Self {
        DownloadStates { table: Mutex::default(), download_after }
    }

    /// A download of the OneDrive file `id` is asked for; `waiter` is answered when it ends,
    /// however it ends. Whether the caller is to run it, or one runs already.
    pub(super) fn start_or_join(&self, id: &str, waiter: Option<Waiter>) -> DownloadStart {
        let mut table = crate::panic::lock(&self.table);
        let (bytes_read, unfinished_file) = match table.states.remove(id) {
            Some(DownloadState::Running { mut waiters, bytes_read, written_up_to, file }) => {
                waiters.extend(waiter);
                table.states.insert(id.to_owned(), DownloadState::Running { waiters, bytes_read, written_up_to, file });
                return DownloadStart::Joined;
            }
            Some(DownloadState::Failed { unfinished_file, bytes_read }) => {
                table.failed_ids.retain(|failed| failed != id);
                (bytes_read, Some(unfinished_file))
            }
            Some(DownloadState::NotRunning { bytes_read }) => (bytes_read, None),
            None => (0, None),
        };
        let running = DownloadState::Running { waiters: waiter.into_iter().collect(), bytes_read, written_up_to: Arc::default(), file: None };
        table.states.insert(id.to_owned(), running);
        DownloadStart::Started { unfinished_file }
    }

    /// The running download of `id` writes into `file` from now on. What the download moves
    /// as it writes: how far the file is written in order.
    pub(super) fn begin_writing(&self, id: &str, file: Arc<File>) -> Arc<AtomicU64> {
        match crate::panic::lock(&self.table).states.get_mut(id) {
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
        match crate::panic::lock(&self.table).states.get(id) {
            Some(DownloadState::Running { written_up_to, file: Some(file), .. }) if end + WRITTEN_BEHIND <= written_up_to.load(Ordering::SeqCst) => Some(Arc::clone(file)),
            _ => None,
        }
    }

    /// The running download of `id` ended, well or with an errno: its waiters are told, and
    /// one that ended badly keeps the file it wrote into, to be continued from.
    pub(super) fn finish(&self, id: &str, outcome: Result<(), i32>) {
        let waiters = {
            let mut table = crate::panic::lock(&self.table);
            match table.states.remove(id) {
                Some(DownloadState::Running { waiters, bytes_read, file, .. }) => {
                    match (outcome, file) {
                        (Ok(()), _) => {}
                        (Err(_), Some(file)) => table.keep_unfinished_file(id, file, bytes_read),
                        (Err(_), None) => drop(table.states.insert(id.to_owned(), DownloadState::NotRunning { bytes_read })),
                    }
                    waiters
                }
                // Not running: abandoned already.
                Some(other) => {
                    table.states.insert(id.to_owned(), other);
                    Vec::new()
                }
                None => Vec::new(),
            }
        };
        for waiter in waiters {
            let _ = waiter.send(outcome);
        }
    }

    /// The running download of `id` will never report its end (it panicked): its waiters are
    /// failed, and nothing of it is kept.
    pub(super) fn abandon(&self, id: &str) {
        let waiters = {
            let mut table = crate::panic::lock(&self.table);
            match table.states.remove(id) {
                Some(DownloadState::Running { waiters, bytes_read, .. }) => {
                    table.states.insert(id.to_owned(), DownloadState::NotRunning { bytes_read });
                    waiters
                }
                Some(other) => {
                    table.states.insert(id.to_owned(), other);
                    Vec::new()
                }
                None => Vec::new(),
            }
        };
        for waiter in waiters {
            let _ = waiter.send(Err(libc::EIO));
        }
    }

    /// `bytes` more of the OneDrive file `id`, which is `file_size` long, were read as a
    /// range. Whether enough of it was read by now for the whole file to be downloaded.
    pub(super) fn count_bytes_read(&self, id: &str, bytes: u64, file_size: u64) -> bool {
        let mut table = crate::panic::lock(&self.table);
        let state = table.states.entry(id.to_owned()).or_insert(DownloadState::NotRunning { bytes_read: 0 });
        let (DownloadState::NotRunning { bytes_read } | DownloadState::Running { bytes_read, .. } | DownloadState::Failed { bytes_read, .. }) = state;
        *bytes_read += bytes;
        reaches_download_threshold(*bytes_read, self.download_after, file_size)
    }

    /// Whether the OneDrive file `id` is being downloaded whole: from the moment its download
    /// was asked for until it ended.
    pub(super) fn is_downloading(&self, id: &str) -> bool {
        matches!(crate::panic::lock(&self.table).states.get(id), Some(DownloadState::Running { .. }))
    }

    /// The mount is being taken off: whoever waits for a download is failed now. A download
    /// that still runs ends as any other, with nobody waiting.
    pub(super) fn fail_waiters(&self) {
        let waiters: Vec<Waiter> = {
            let mut table = crate::panic::lock(&self.table);
            let running = table.states.values_mut().filter_map(|state| match state {
                DownloadState::Running { waiters, .. } => Some(std::mem::take(waiters)),
                _ => None,
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
        let table = crate::panic::lock(&self.table);
        let kept = table.failed_ids.iter().filter_map(|id| match table.states.get(id) {
            Some(DownloadState::Failed { unfinished_file, .. }) => Some(Arc::clone(unfinished_file)),
            _ => None,
        });
        kept.collect()
    }
}

/// Whether `bytes_read` bytes of a file `file_size` long, read as ranges, are enough for the
/// whole file to be downloaded. A file smaller than `download_after` is downloaded once all
/// of it was read: it would be fetched again at every read otherwise.
pub(super) fn reaches_download_threshold(bytes_read: u64, download_after: u64, file_size: u64) -> bool {
    bytes_read >= download_after.min(file_size)
}

/// What the daemon shows of a file: being downloaded, downloaded, or neither. `None` for a
/// file that is not in OneDrive yet.
pub(super) fn to_hydration_state(file: &FileSyncState, is_downloading: bool) -> Option<State> {
    match file {
        FileSyncState::LocalOnly(_) => None,
        _ if is_downloading => Some(State::Hydrating),
        FileSyncState::Synced(..) | FileSyncState::ModifiedHere(..) => Some(State::Hydrated),
        FileSyncState::RemoteOnly(_) => Some(State::OnlineOnly),
    }
}

#[cfg(test)]
mod tests;
